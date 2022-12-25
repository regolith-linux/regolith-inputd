use crate::InputHandler;
use gio::{prelude::SettingsExtManual, Settings};
use log::info;
use std::error::Error;
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
    fn apply_repeat_interval(&mut self) -> Result<(), Box<dyn Error>> {
        let interval: u32 = self.settings().get("repeat-interval");
        let repeat_freq = 1000f64 / interval as f64;
        let cmd = format!("input type:keyboard repeat_rate {repeat_freq}");
        self.sway_connection().run_command(cmd)?;
        Ok(())
    }
    fn apply_repeat_delay(&mut self) -> Result<(), Box<dyn Error>> {
        let delay: u32 = self.settings().get("delay");
        let cmd = format!("input type:keyboard repeat_delay {delay}");
        self.sway_connection().run_command(cmd)?;
        Ok(())
    }
}

impl InputHandler for KeyboardHandler {
    fn apply_changes(&mut self, key: &str) -> Result<(), Box<dyn Error>> {
        info!("org.gnome.desktop.peripherals.keyboard -> Key: {key} chaged");
        match key {
            "repeat-interval" => self.apply_repeat_interval()?,
            "delay" => self.apply_repeat_delay()?,
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
    fn apply_all(&mut self) -> Result<(), Box<dyn Error>> {
        Ok(())
    }
    fn sync_gsettings(&mut self, _: &swayipc::Input) -> Result<(), Box<dyn Error>> {
        Ok(())
    }
}
unsafe impl Send for KeyboardHandler {}
