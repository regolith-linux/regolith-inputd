use crate::{InputHandler, PointerMethods};
use gio::{prelude::SettingsExtManual, Settings};
use log::debug;
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
    // fn apply_speed(&mut self, key: &str) -> Result<(), Box<dyn Error>> {
    //     let new_val: f64 = self.settings.get(key);
    //     let cmd = format!("input type:touchpad pointer_accel {new_val}");
    //     self.sway_connection.run_command(cmd)?;
    //     Ok(())
    // }
    // fn apply_natural_scroll(&mut self, key: &str) -> Result<(), Box<dyn Error>> {
    //     let new_val: &str = if self.settings.get(key) {
    //         "enabled"
    //     } else {
    //         "disabled"
    //     };
    //     let cmd = format!("input type:touchpad natural_scroll {new_val}");
    //     self.sway_connection.run_command(cmd)?;
    //     Ok(())
    // }
    fn apply_tap(&mut self) -> Result<(), Box<dyn Error>> {
        let new_val: &str = if self.settings.get("tap-to-click") {
            "enabled"
        } else {
            "disabled"
        };
        let cmd = format!("input type:touchpad tap {new_val}");
        self.sway_connection.run_command(cmd)?;
        Ok(())
    }
    fn apply_scroll_method(&mut self) -> Result<(), Box<dyn Error>> {
        let two_finger: bool = self.settings().get("two-finger-scrolling-enabled");
        let edge_scroll: bool = self.settings().get("edge-scrolling-enabled");
        let scroll_method = if two_finger {
            "two_finger"
        } else if edge_scroll {
            "edge"
        } else {
            "none"
        };
        let cmd = format!("input type:touchpad scroll_method {scroll_method}");
        self.sway_connection.run_command(cmd)?;
        Ok(())
    }
}

impl PointerMethods for TouchpadHandler {
    fn pointer_type(&self) -> &str {
        "touchpad"
    }
}

impl InputHandler for TouchpadHandler {
    fn apply_changes(&mut self, key: &str) -> Result<(), Box<dyn Error>> {
        debug!("{key}");
        match key {
            "speed" => self.apply_speed()?,
            "natural-scroll" => self.apply_natural_scroll()?,
            "tap-to-click" => self.apply_tap()?,
            "two-finger-scrolling-enabled" | "edge-scrolling-enabled" => {
                self.apply_scroll_method()?
            }
            "left-handed" => self.left_handed()?,
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
