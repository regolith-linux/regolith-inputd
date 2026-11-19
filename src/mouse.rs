use crate::sway;
use crate::traits::{
    forget_accel_profile, set_enabled_if_changed, InputHandler, PointerMethods,
    PrimitiveToSwayType, SwayTypeToPrimitive,
};
use gio::{prelude::SettingsExtManual, Settings};
use log::info;
use serde_json::Value;
use std::error::Error;
pub struct MouseHandler {
    settings: Settings,
}

impl MouseHandler {
    pub fn new() -> MouseHandler {
        let settings = Settings::new("org.gnome.desktop.peripherals.mouse");
        MouseHandler { settings }
    }

    /// Touchpads whose left-handed setting is "mouse" (GNOME's default)
    /// follow the mouse, so a change to the mouse applies to them too.
    fn apply_touchpad_left_handed(&mut self) -> Result<(), Box<dyn Error>> {
        let touchpad_settings = Settings::new("org.gnome.desktop.peripherals.touchpad");
        if touchpad_settings.get::<String>("left-handed") != "mouse" {
            return Ok(());
        }
        let new_val: &str = self
            .settings()
            .get::<bool>("left-handed")
            .to_sway_type()
            .to_primitive();
        let cmd = format!("input type:touchpad left_handed {new_val}");
        info!("Executing command: {cmd}");
        sway::run_command(cmd)?;
        Ok(())
    }
}

impl PointerMethods for MouseHandler {
    fn pointer_type(&self) -> &str {
        "pointer"
    }
    fn apply_left_handed(&mut self) -> Result<(), Box<dyn Error>> {
        let new_val: &str = self
            .settings()
            .get::<bool>("left-handed")
            .to_sway_type()
            .to_primitive();
        let pointer_type = self.pointer_type();
        let cmd = format!("input type:{pointer_type} left_handed {new_val}");
        info!("Executing command: {cmd}");
        sway::run_command(cmd)?;
        Ok(())
    }

    fn apply_click_method(&mut self) -> Result<(), Box<dyn Error>> {
        Ok(())
    }
}

impl InputHandler for MouseHandler {
    fn apply_changes(&mut self, key: &str) -> Result<(), Box<dyn Error>> {
        info!("org.gnome.desktop.peripherals.mouse -> Key: {key} chaged");
        match key {
            "speed" => self.apply_speed()?,
            "accel-profile" => self.apply_accel_profile()?,
            "natural-scroll" => self.apply_natural_scroll()?,
            "left-handed" => {
                self.apply_left_handed()?;
                self.apply_touchpad_left_handed()?;
            }
            _ => (),
        };
        Ok(())
    }
    fn settings(&self) -> &Settings {
        &self.settings
    }
    fn reset(&mut self) {
        forget_accel_profile(self.pointer_type());
    }
    fn apply_all(&mut self) -> Result<(), Box<dyn Error>> {
        self.apply_speed()?;
        self.apply_accel_profile()?;
        self.apply_left_handed()?;
        self.apply_natural_scroll()?;
        Ok(())
    }
    fn sway_input_type(&self) -> Option<&str> {
        Some(self.pointer_type())
    }
    fn sync_gsettings(&self, setting: &str, value: &Value) -> Result<(), Box<dyn Error>> {
        match setting {
            "left_handed" => set_enabled_if_changed(self.settings(), "left-handed", value),
            _ => self.sync_pointer_gsettings(setting, value),
        }
    }
}

unsafe impl Send for MouseHandler {}
