use crate::InputHandler;
use gio::{prelude::SettingsExtManual, Settings};
use std::error::Error;
use swayipc::Connection as SwayConnection;

pub struct TouchpadHandler {
    settings: Settings,
    sway_connection: SwayConnection,
}

impl TouchpadHandler {
    pub fn new() -> TouchpadHandler {
        let settings = Settings::new("org.gnome.desktop.peripherals.touchpad");
        let connection = SwayConnection::new().unwrap();
        TouchpadHandler {
            settings,
            sway_connection: connection,
        }
    }
}

impl InputHandler for TouchpadHandler {
    fn apply_changes(&mut self, key: &str) -> Result<(), Box<dyn Error>> {
        println!("Applying");
        match key {
            "speed" => {
                let new_val: f64 = self.settings.get(key);
                let cmd = format!("input type:touchpad pointer_accel {new_val}");
                self.sway_connection.run_command(cmd)?;
            }
            _ => (),
        };
        Ok(())
    }
    fn settings(&self) -> &Settings {
        &self.settings
    }
    fn monitor_sway_inputs(&self) {}
}
