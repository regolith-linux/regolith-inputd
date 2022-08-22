use crate::InputHandler;
use gio::Settings;
use swayipc::Connection as SwayConnection;

pub struct KeyboardHandler {
    settings: Settings,
    sway_connection: SwayConnection,
}
impl KeyboardHandler {
    pub fn new() -> KeyboardHandler {
        let settings = Settings::new("org.gnome.desktop.peripherals.keyboard");
        let sway_connection = SwayConnection::new().unwrap();
        KeyboardHandler {
            settings,
            sway_connection,
        }
    }
}

impl InputHandler for KeyboardHandler {
    // fn apply_changes(&self) {}
    fn settings(&self) -> &Settings {
        &self.settings
    }
    fn sway_connection(&mut self) -> &mut swayipc::Connection {
        &mut self.sway_connection
    }
    fn monitor_sway_inputs(&self) {}
}
