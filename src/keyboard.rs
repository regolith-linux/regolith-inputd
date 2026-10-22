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
    fn apply_repeat_rate(&mut self) -> Result<(), Box<dyn Error>> {
        let rate = repeat_rate(
            self.settings().get("repeat"),
            self.settings().get("repeat-interval"),
        );
        let cmd = format!("input type:keyboard repeat_rate {rate}");
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
            "repeat" | "repeat-interval" => self.apply_repeat_rate()?,
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
        self.apply_repeat_rate()?;
        self.apply_repeat_delay()?;
        Ok(())
    }
    fn sync_gsettings(&mut self, _: &swayipc::Input) -> Result<(), Box<dyn Error>> {
        Ok(())
    }
}
unsafe impl Send for KeyboardHandler {}

/// sway's `repeat_rate` (repeats per second) for GNOME's `repeat` and
/// `repeat-interval` (milliseconds between repeats). 0 turns repeat off.
fn repeat_rate(repeat: bool, interval_ms: u32) -> u32 {
    if repeat {
        (1000 / interval_ms.max(1)).max(1)
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::repeat_rate;

    #[test]
    fn repeat_interval_maps_to_rate() {
        assert_eq!(repeat_rate(true, 30), 33);
        assert_eq!(repeat_rate(true, 20), 50);
        assert_eq!(repeat_rate(true, 2000), 1);
        assert_eq!(repeat_rate(true, 0), 1000);
    }

    #[test]
    fn repeat_off_is_rate_zero() {
        assert_eq!(repeat_rate(false, 30), 0);
    }
}
