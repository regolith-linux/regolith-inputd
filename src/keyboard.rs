use crate::InputHandler;
use gio::Settings;

pub struct KeyboardHandler {
    settings: Settings,
}
impl KeyboardHandler {
    pub fn new() -> KeyboardHandler {
        let settings = Settings::new("org.gnome.desktop.peripherals.keyboard");
        KeyboardHandler { settings }
    }
}

impl InputHandler for KeyboardHandler {
    // fn apply_changes(&self) {}
    fn settings(&self) -> &Settings {
        &self.settings
    }
    fn monitor_sway_inputs(&self) {}
}
