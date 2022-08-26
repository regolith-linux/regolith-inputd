mod keyboard;
mod mouse;
mod touchpad;

use gio::prelude::SettingsExtManual;
use gio::{traits::SettingsExt, Settings};
use keyboard::KeyboardHandler;
use log::{debug, error};
use mouse::MouseHandler;
use std::error::Error;
use swayipc::Connection as SwayConnection;
use touchpad::TouchpadHandler;

pub trait InputHandler {
    fn settings(&self) -> &Settings;
    fn sway_connection(&mut self) -> &mut SwayConnection;
    fn monitor_sway_inputs(&self);
    fn apply_changes(&mut self, _: &str) -> Result<(), Box<dyn Error>> {
        Ok(())
    }
    fn monitor_gsettings_change(&mut self)
    where
        Self: 'static,
    {
        let ptr: *mut Self = self;
        self.settings().connect_changed(None, move |_, key| unsafe {
            if !ptr.is_null() {
                if let Err(e) = (*ptr).apply_changes(key) {
                    error!("{e}");
                };
            }
        });
    }
}

pub trait PointerMethods: InputHandler {
    fn pointer_type(&self) -> &str {
        "pointer"
    }
    fn apply_speed(&mut self) -> Result<(), Box<dyn Error>> {
        let new_val: f64 = self.settings().get("speed");
        let pointer_type = self.pointer_type();
        let cmd = format!("input type:{pointer_type} pointer_accel {new_val}");
        self.sway_connection().run_command(cmd)?;
        Ok(())
    }
    fn apply_natural_scroll(&mut self) -> Result<(), Box<dyn Error>> {
        let new_val: &str = if self.settings().get("natural-scroll") {
            "enabled"
        } else {
            "disabled"
        };
        let pointer_type = self.pointer_type();
        let cmd = format!("input type:{pointer_type} natural_scroll {new_val}");
        self.sway_connection().run_command(cmd)?;
        Ok(())
    }
    fn apply_left_handed(&mut self) -> Result<(), Box<dyn Error>> {
        let new_val: &str = if self.settings().get("left-handed") {
            "disabled"
        } else {
            "enabled"
        };
        let pointer_type = self.pointer_type();
        let cmd = format!("input type:{pointer_type} left_handed {new_val}");
        debug!("{cmd}");
        self.sway_connection().run_command(cmd)?;
        Ok(())
    }
}

pub struct SettingsManager {
    handlers: [Box<dyn InputHandler>; 3],
}

impl SettingsManager {
    pub fn new() -> SettingsManager {
        let handlers: [Box<dyn InputHandler>; 3] = [
            Box::new(MouseHandler::new()),
            Box::new(KeyboardHandler::new()),
            Box::new(TouchpadHandler::new()),
        ];
        SettingsManager { handlers }
    }

    pub fn start_monitoring(&mut self) {
        for handle in &mut self.handlers {
            handle.monitor_gsettings_change();
            handle.monitor_sway_inputs();
        }
    }
}
