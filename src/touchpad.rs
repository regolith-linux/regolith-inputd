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
    fn use_dwt(&mut self) -> Result<(), Box<dyn Error>> {
        let new_val: &str = if self.settings.get("disable-while-typing") {
            "enabled"
        } else {
            "disabled"
        };
        let cmd = format!("input type:touchpad dwt {new_val}");
        self.sway_connection.run_command(cmd)?;
        Ok(())
    }
    fn send_events(&mut self) -> Result<(), Box<dyn Error>> {
        let new_val: String = self.settings().get("send-events");
        let cmd = format!("input type:touchpad events {new_val}");
        debug!("{cmd}");
        self.sway_connection.run_command(cmd)?;
        Ok(())
    }
    fn use_drag(&mut self) -> Result<(), Box<dyn Error>> {
        let new_val: &str = if self.settings.get("tap-and-drag") {
            "enabled"
        } else {
            "disabled"
        };
        let cmd = format!("input type:touchpad drag {new_val}");
        debug!("{cmd}");
        self.sway_connection.run_command(cmd)?;
        Ok(())
    }
    fn use_drag_lock(&mut self) -> Result<(), Box<dyn Error>> {
        let new_val: &str = if self.settings.get("tap-and-drag-lock") {
            "enabled"
        } else {
            "disabled"
        };
        let cmd = format!("input type:touchpad drag_lock {new_val}");
        debug!("{cmd}");
        self.sway_connection.run_command(cmd)?;
        Ok(())
    }

    fn emulate_middle_click(&mut self) -> Result<(), Box<dyn Error>> {
        let new_val: &str = if self.settings.get("middle-click-emulation") {
            "enabled"
        } else {
            "disabled"
        };
        let cmd = format!("input type:touchpad middle_emulation {new_val}");
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
            "left-handed" => self.apply_left_handed()?,
            "send-events" => self.send_events()?,
            "disable-while-typing" => self.use_dwt()?,
            "tap-and-drag" => self.use_drag()?,
            "tap-and-drag-lock" => self.use_drag_lock()?,
            "middle-click-emulation" => self.emulate_middle_click()?,
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
