use crate::{InputHandler, PointerMethods};
use gio::Settings;
use log::debug;
use std::error::Error;
use swayipc::Connection as SwayConnection;
pub struct MouseHandler {
    settings: Settings,
    sway_connection: SwayConnection,
}
impl MouseHandler {
    pub fn new() -> MouseHandler {
        let settings = Settings::new("org.gnome.desktop.peripherals.mouse");
        let sway_connection = SwayConnection::new().unwrap();
        MouseHandler {
            settings,
            sway_connection,
        }
    }
}

impl PointerMethods for MouseHandler {}

impl InputHandler for MouseHandler {
    fn apply_changes(&mut self, key: &str) -> Result<(), Box<dyn Error>> {
        debug!("{key}");
        match key {
            "speed" => self.apply_speed()?,
            "natural-scroll" => self.apply_natural_scroll()?,
            "left-handed" => self.apply_left_handed()?,
            _ => (),
        };
        Ok(())
    }
    fn settings(&self) -> &Settings {
        &self.settings
    }
    fn sway_connection(&mut self) -> &mut swayipc::Connection {
        &mut self.sway_connection
    }
    fn monitor_sway_inputs(&self) {}
}
