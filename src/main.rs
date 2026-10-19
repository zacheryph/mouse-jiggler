use esp_idf_sys as _;
use esp32_nimble::{
    BLEAdvertisementData, BLECharacteristic, BLEDevice, BLEHIDDevice, BLEServer, enums::*, hid::*,
    utilities::mutex::Mutex,
};
use std::sync::Arc;
use zerocopy::IntoBytes;
use zerocopy_derive::{Immutable, IntoBytes};

const BLE_DEVICE_NAME: &str = env!("BLE_DEVICE_NAME");
const MOUSE_ID: u8 = 0x01;

const HID_REPORT_DESCRIPTOR: &[u8] = hid!(
    (USAGE_PAGE, 0x01), // USAGE_PAGE (Generic Desktop Ctrls)
    (USAGE, 0x02),      // USAGE (Mouse)
    (COLLECTION, 0x01), // COLLECTION (Application)
    (REPORT_ID, MOUSE_ID),
    (USAGE, 0x01),      //   USAGE (Pointer)
    (COLLECTION, 0x00), //   COLLECTION (Physical)
    // ------------------------------------------------- Buttons (Left, Right, Middle, Back, Forward)
    (USAGE_PAGE, 0x09),      //     USAGE_PAGE (Buttons)
    (USAGE_MINIMUM, 0x01),   //     USAGE_MINIMUM (Button 1)
    (USAGE_MAXIMUM, 0x05),   //     USAGE_MAXIMUM (Button 5)
    (LOGICAL_MINIMUM, 0x00), //     LOGICAL_MINIMUM (0)
    (LOGICAL_MAXIMUM, 0x01), //     LOGICAL_MAXIMUM (1)
    (REPORT_SIZE, 0x01),     //     REPORT_SIZE (1) (1 bit)
    (REPORT_COUNT, 0x05),    //     REPORT_COUNT (5) (5 times)
    (HIDINPUT, 0x02),        //     INPUT (Data, Variable, Absolute)
    // ------------------------------------------------- Padding
    (REPORT_SIZE, 0x03),  //     REPORT_SIZE (3) (3 bits)
    (REPORT_COUNT, 0x01), //     REPORT_COUNT (1) (1 time)
    (HIDINPUT, 0x03),     //     INPUT (Constant, Variable, Absolute) ;3 bit padding
    // ------------------------------------------------- X/Y position, Wheel
    (USAGE_PAGE, 0x01),      //    USAGE_PAGE (Generic Desktop)
    (USAGE, 0x30),           //    USAGE (X)
    (USAGE, 0x31),           //    USAGE (Y)
    (USAGE, 0x38),           //    USAGE (Wheel)
    (LOGICAL_MINIMUM, 0x81), //    LOGICAL_MINIMUM (-127)
    (LOGICAL_MAXIMUM, 0x7f), //    LOGICAL_MAXIMUM (127)
    (REPORT_SIZE, 0x08),     //    REPORT_SIZE (8)
    (REPORT_COUNT, 0x03),    //    REPORT_COUNT (3)
    (HIDINPUT, 0x06),        //    INPUT (Data, Variable, Relative) ;3 bytes (X,Y,Wheel)
    // ------------------------------------------------- Horizontal wheel
    (USAGE_PAGE, 0x0c),      //    USAGE PAGE (Consumer Devices)
    (USAGE, 0x38, 0x02),     //    USAGE (AC Pan)
    (LOGICAL_MINIMUM, 0x81), //    LOGICAL_MINIMUM (-127)
    (LOGICAL_MAXIMUM, 0x7f), //    LOGICAL_MAXIMUM (127)
    (REPORT_SIZE, 0x08),     //    REPORT_SIZE (8)
    (REPORT_COUNT, 0x01),    //    REPORT_COUNT (1)
    (HIDINPUT, 0x06),        //    INPUT (Data, Var, Rel)
    (END_COLLECTION),        //   END_COLLECTION
    (END_COLLECTION),        // END_COLLECTION
);

const DELAY: esp_idf_hal::delay::Delay = esp_idf_hal::delay::Delay::new_default();

#[derive(Immutable, IntoBytes)]
#[repr(packed)]
struct MouseReport {
    buttons: u8,
    x: i8,
    y: i8,
    wheel: i8,
    horizontal_wheel: i8,
}

struct Mouse {
    server: &'static mut BLEServer,
    input_mouse: Arc<Mutex<BLECharacteristic>>,
    mouse_report: MouseReport,
}

impl Mouse {
    fn new() -> Self {
        BLEDevice::set_device_name(BLE_DEVICE_NAME).unwrap();

        let device = BLEDevice::take();
        let server = device.get_server();
        let mut hid = BLEHIDDevice::new(server);
        let ble_advertising = device.get_advertising();
        let mut advertisement_data = BLEAdvertisementData::new();

        device
            .security()
            .set_auth(AuthReq::Bond) // Bonding enables key storage for reconnection
            .set_passkey(123456) // Optional, sets the passkey for pairing
            .set_io_cap(SecurityIOCap::NoInputNoOutput) // You can choose any IO capability
            .resolve_rpa(); // Crucial for managing iOS's dynamic Bluetooth addresses

        server.on_connect(|server, desc| {
            ::log::info!("Client connected: {:?}", desc);

            if server.connected_count() < Self::max_connections() {
                let mut advertise = ble_advertising.lock();
                if advertise.is_advertising() {
                    return;
                }

                ::log::info!("Multi-connect support: start advertising");
                advertise.start().unwrap();
            }
        });

        server.on_disconnect(|desc, err| {
            ::log::info!("Client disconnected: {:?} => {:?}", desc, err);
            let mut advertise = ble_advertising.lock();
            if advertise.is_advertising() {
                return;
            }

            advertise.start().unwrap();
        });

        let input_mouse = hid.input_report(MOUSE_ID);

        hid.manufacturer("routine.sh");
        hid.pnp(0x02, 0x05ac, 0x820a, 0x0210);
        hid.hid_info(0x00, 0x02);
        hid.report_map(HID_REPORT_DESCRIPTOR);
        hid.set_battery_level(100);

        advertisement_data
            .name(BLE_DEVICE_NAME)
            .appearance(0x03C2) // 0x03c1 is KEYBOARD
            .add_service_uuid(hid.hid_service().lock().uuid());

        {
            // lock mutex once for all changes
            let mut advertise = ble_advertising.lock();
            advertise.scan_response(false);
            advertise.set_data(&mut advertisement_data).unwrap();
            advertise.start().unwrap();
        }

        Self {
            server,
            input_mouse,
            mouse_report: MouseReport {
                buttons: 0,
                x: 0,
                y: 0,
                wheel: 0,
                horizontal_wheel: 0,
            },
        }
    }

    fn max_connections() -> usize {
        esp_idf_svc::sys::CONFIG_BT_NIMBLE_MAX_CONNECTIONS as _
    }

    fn connected(&self) -> bool {
        self.connected_count() > 0
    }

    fn connected_count(&self) -> usize {
        self.server.connected_count()
    }

    fn click(&mut self, b: u8) {
        self.mouse_report.buttons = b;
        self.send_report(&self.mouse_report);
    }

    fn move_mouse(&mut self, x: i8, y: i8, wheel: i8, horizontal_wheel: i8) {
        self.mouse_report.x = x;
        self.mouse_report.y = y;
        self.mouse_report.wheel = wheel;
        self.mouse_report.horizontal_wheel = horizontal_wheel;
        self.send_report(&self.mouse_report);
    }

    fn send_report(&self, mouse: &MouseReport) {
        self.input_mouse.lock().set_value(mouse.as_bytes()).notify();
        DELAY.delay_ms(7);
    }
}

fn main() {
    // It is necessary to call this function once. Otherwise some patches to the runtime
    // implemented by esp-idf-sys might not link properly. See https://github.com/esp-rs/esp-idf-template/issues/71
    esp_idf_svc::sys::link_patches();

    // Bind the log crate to the ESP Logging facilities
    esp_idf_svc::log::EspLogger::initialize_default();

    log::info!("ESP Clicker Starting...");
    log::info!("  - BLE Device Name: {}", BLE_DEVICE_NAME);
    log::info!(
        "  - Maximum Device Connections: {}",
        Mouse::max_connections()
    );

    let mut mouse = Mouse::new();

    loop {
        ::log::info!("Sending jiggle to {} devices.", mouse.connected_count());
        if mouse.connected() {
            mouse.click(1);
            DELAY.delay_ms(50);
            mouse.move_mouse(10, 10, 0, 0);
            DELAY.delay_ms(100);
            mouse.move_mouse(-10, -10, 0, 0);
            DELAY.delay_ms(100);
            mouse.move_mouse(10, 10, 0, 0);
            DELAY.delay_ms(100);
            mouse.move_mouse(-10, -10, 0, 0);
            DELAY.delay_ms(50);
            mouse.click(0);
        }
        DELAY.delay_ms(60 * 1000);
    }
}
