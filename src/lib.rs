mod keyboard;
mod mouse;
mod touchpad;

use async_trait::async_trait;
use gio::{traits::SettingsExt, Settings};
use keyboard::KeyboardHandler;
use mouse::MouseHandler;
use touchpad::TouchpadHandler;

pub trait InputHandler {
    fn settings(&self) -> &Settings;
    fn monitor_sway_inputs(&self);
    fn apply_changes(&mut self, key: &str) {
        println!("{key}");
    }
    fn monitor_gsettings_change(&mut self)
    where
        Self: 'static,
    {
        let ptr: *mut Self = self;
        self.settings().connect_changed(None, move |_, key| unsafe {
            if !ptr.is_null() {
                (*ptr).apply_changes(key);
            }
        });
    }
}

pub struct SettingsManager {
    handlers: [Box<dyn InputHandler>; 3],
}

impl SettingsManager {
    pub async fn new() -> SettingsManager {
        let handlers: [Box<dyn InputHandler>; 3] = [
            Box::new(MouseHandler::new().await),
            Box::new(KeyboardHandler::new().await),
            Box::new(TouchpadHandler::new().await),
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
