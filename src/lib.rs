mod keyboard;
mod mouse;
mod touchpad;

use gio::{traits::SettingsExt, Settings};
use keyboard::KeyboardHandler;
use mouse::MouseHandler;
use std::error::Error;
use touchpad::TouchpadHandler;

pub trait InputHandler {
    fn settings(&self) -> &Settings;
    fn monitor_sway_inputs(&self);
    fn apply_changes(&mut self, key: &str) -> Result<(), Box<dyn Error>> {
        println!("{key}");
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
                    eprintln!("{e}");
                };
            }
        });
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
