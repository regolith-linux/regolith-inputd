use crate::InputHandler;
use gio::Settings;
pub struct MouseHandler {
    settings: Settings,
}
impl MouseHandler {
    pub fn new() -> MouseHandler {
        let settings = Settings::new("org.gnome.desktop.peripherals.mouse");
        MouseHandler { settings }
    }
}

impl InputHandler for MouseHandler {
    // fn apply_changes(&self, key: &str) {}
    fn settings(&self) -> &Settings {
        &self.settings
    }
    fn monitor_sway_inputs(&self) {}
}
