use crate::traits::{
    set_enabled_if_changed, set_if_changed, InputHandler, PointerMethods, PrimitiveToSwayType,
    SwayTypeToPrimitive,
};
use crate::utils;
use gio::{prelude::SettingsExtManual, Settings};
use log::info;

use serde_json::Value;
use std::collections::BTreeMap;
use std::error::Error;
use swayipc::Connection as SwayConnection;

pub struct TouchpadHandler {
    settings: Settings,
    sway_connection: SwayConnection,
    /// Click method of each touchpad, by identifier, before the daemon
    /// first set one; what GNOME's "default" restores.
    default_click_methods: BTreeMap<String, &'static str>,
    click_method_set: bool,
}
impl TouchpadHandler {
    pub fn new() -> TouchpadHandler {
        let settings = Settings::new("org.gnome.desktop.peripherals.touchpad");
        let connection = SwayConnection::new().unwrap();
        TouchpadHandler {
            settings,
            sway_connection: connection,
            default_click_methods: BTreeMap::new(),
            click_method_set: false,
        }
    }
    /// Remember the click method of touchpads not seen before.
    fn remember_default_click_methods(&mut self) -> Result<(), Box<dyn Error>> {
        // Devices without vendor/product ids aren't libinput touchpads.
        for input in utils::get_inputs()? {
            if input.input_type != "touchpad" {
                continue;
            }
            if let Some(method) = input.libinput.and_then(|l| l.click_method) {
                self.default_click_methods
                    .entry(input.identifier)
                    .or_insert_with(|| method.to_primitive());
            }
        }
        Ok(())
    }
    fn apply_tap(&mut self) -> Result<(), Box<dyn Error>> {
        let tap_enabled: &str = self
            .settings
            .get::<bool>("tap-to-click")
            .to_sway_type()
            .to_primitive();
        let cmd = format!("input type:touchpad tap {tap_enabled}");
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
        let dwt_enabled: &str = self
            .settings
            .get::<bool>("disable-while-typing")
            .to_sway_type()
            .to_primitive();
        let cmd = format!("input type:touchpad dwt {dwt_enabled}");
        self.sway_connection.run_command(cmd)?;
        Ok(())
    }
    fn send_events(&mut self) -> Result<(), Box<dyn Error>> {
        let new_val: String = self.settings().get("send-events");
        // GNOME spells it disabled-on-external-mouse, sway disabled_on_external_mouse.
        let new_val = new_val.replace('-', "_");
        let cmd = format!("input type:touchpad events {new_val}");
        info!("{cmd}");
        self.sway_connection.run_command(cmd)?;
        Ok(())
    }
    fn use_drag(&mut self) -> Result<(), Box<dyn Error>> {
        let use_drag_enabled: &str = self
            .settings
            .get::<bool>("tap-and-drag")
            .to_sway_type()
            .to_primitive();
        let cmd = format!("input type:touchpad drag {use_drag_enabled}");
        info!("{cmd}");
        self.sway_connection.run_command(cmd)?;
        Ok(())
    }
    fn use_drag_lock(&mut self) -> Result<(), Box<dyn Error>> {
        let drag_lock_enabled: &str = self
            .settings
            .get::<bool>("tap-and-drag-lock")
            .to_sway_type()
            .to_primitive();
        let cmd = format!("input type:touchpad drag_lock {drag_lock_enabled}");
        info!("{cmd}");
        self.sway_connection.run_command(cmd)?;
        Ok(())
    }

    fn emulate_middle_click(&mut self) -> Result<(), Box<dyn Error>> {
        let emulate_middle_enabled: &str = self
            .settings
            .get::<bool>("middle-click-emulation")
            .to_sway_type()
            .to_primitive();
        let cmd = format!("input type:touchpad middle_emulation {emulate_middle_enabled}");
        self.sway_connection.run_command(cmd)?;
        Ok(())
    }
}

impl PointerMethods for TouchpadHandler {
    fn pointer_type(&self) -> &str {
        "touchpad"
    }
    fn apply_left_handed(&mut self) -> Result<(), Box<dyn Error>> {
        let left_handed: String = self.settings().get("left-handed");

        let left_handed_enabled: &str = match left_handed.as_ref() {
            "left" => true,
            "right" => false,
            "mouse" => {
                let mouse_settings = Settings::new("org.gnome.desktop.peripherals.mouse");
                mouse_settings.get("left-handed")
            }
            _ => false,
        }
        .to_sway_type()
        .to_primitive();
        let pointer_type = self.pointer_type();
        let cmd = format!("input type:{pointer_type} left_handed {left_handed_enabled}");
        info!("{cmd}");
        self.sway_connection().run_command(cmd)?;
        Ok(())
    }

    fn apply_click_method(&mut self) -> Result<(), Box<dyn Error>> {
        let click_method: String = self.settings().get("click-method");
        self.remember_default_click_methods()?;
        let commands = click_method_commands(
            &click_method,
            self.click_method_set,
            &self.default_click_methods,
        );
        self.click_method_set |= sway_click_method(&click_method).is_some();
        for cmd in commands {
            info!("{cmd}");
            self.sway_connection().run_command(cmd)?;
        }
        Ok(())
    }
}

/// sway's `click_method` for a GNOME `click-method` value, or `None` for
/// "default" (the device's own default), which sway has no value for.
fn sway_click_method(gnome_value: &str) -> Option<&'static str> {
    match gnome_value {
        "areas" => Some("button_areas"),
        "fingers" => Some("clickfinger"),
        "none" => Some("none"),
        _ => None,
    }
}

/// Commands that apply a GNOME `click-method` value to every touchpad.
///
/// "default" cannot be expressed as a sway setting, and sway cannot unset
/// one. Until the daemon has set a click method there is nothing to undo,
/// so "default" leaves each touchpad as libinput or the user's sway config
/// set it. After that it restores, per touchpad, what sway reported
/// before the daemon's first change.
fn click_method_commands(
    gnome_value: &str,
    already_set: bool,
    defaults: &BTreeMap<String, &'static str>,
) -> Vec<String> {
    match sway_click_method(gnome_value) {
        Some(method) => vec![format!("input type:touchpad click_method {method}")],
        None if already_set => defaults
            .iter()
            .map(|(identifier, method)| format!("input \"{identifier}\" click_method {method}"))
            .collect(),
        None => Vec::new(),
    }
}

impl InputHandler for TouchpadHandler {
    fn apply_changes(&mut self, key: &str) -> Result<(), Box<dyn Error>> {
        info!("org.gnome.desktop.peripherals.touchpad -> Key: {key} chaged");
        match key {
            "speed" => self.apply_speed()?,
            "accel-profile" => self.apply_accel_profile()?,
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
            "click-method" => self.apply_click_method()?,
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
        self.apply_speed()?;
        self.apply_accel_profile()?;
        self.apply_left_handed()?;
        self.apply_natural_scroll()?;
        self.apply_tap()?;
        self.apply_scroll_method()?;
        self.send_events()?;
        self.use_dwt()?;
        self.use_drag()?;
        self.use_drag_lock()?;
        self.emulate_middle_click()?;
        self.apply_click_method()?;
        Ok(())
    }
    fn sway_input_type(&self) -> Option<&str> {
        Some(self.pointer_type())
    }
    fn sync_gsettings(&self, setting: &str, value: &Value) -> Result<(), Box<dyn Error>> {
        let settings = self.settings();
        match setting {
            "send_events" => match value.as_str().and_then(gnome_send_events) {
                Some(send_events) => {
                    set_if_changed(settings, "send-events", send_events.to_string())
                }
                None => Ok(()),
            },
            "tap" => set_enabled_if_changed(settings, "tap-to-click", value),
            "tap_drag" => set_enabled_if_changed(settings, "tap-and-drag", value),
            "tap_drag_lock" => set_enabled_if_changed(settings, "tap-and-drag-lock", value),
            _ => self.sync_pointer_gsettings(setting, value),
        }
    }
}

/// GNOME's `send-events` for sway's `send_events`. sway spells it
/// disabled_on_external_mouse, GNOME disabled-on-external-mouse.
fn gnome_send_events(sway_value: &str) -> Option<&'static str> {
    match sway_value {
        "enabled" => Some("enabled"),
        "disabled" => Some("disabled"),
        "disabled_on_external_mouse" => Some("disabled-on-external-mouse"),
        _ => None,
    }
}

unsafe impl Send for TouchpadHandler {}
unsafe impl Sync for TouchpadHandler {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sway_send_events_map_to_gnome() {
        assert_eq!(gnome_send_events("enabled"), Some("enabled"));
        assert_eq!(gnome_send_events("disabled"), Some("disabled"));
        assert_eq!(
            gnome_send_events("disabled_on_external_mouse"),
            Some("disabled-on-external-mouse")
        );
        assert_eq!(gnome_send_events("something_new"), None);
    }

    #[test]
    fn gnome_click_methods_map_to_sway() {
        assert_eq!(sway_click_method("areas"), Some("button_areas"));
        assert_eq!(sway_click_method("fingers"), Some("clickfinger"));
        assert_eq!(sway_click_method("none"), Some("none"));
        assert_eq!(sway_click_method("default"), None);
    }

    #[test]
    fn default_restores_each_touchpad_only_after_a_change() {
        let defaults = BTreeMap::from([
            ("1:2:Pad_A".to_string(), "button_areas"),
            ("3:4:Pad_B".to_string(), "clickfinger"),
        ]);
        assert!(click_method_commands("default", false, &defaults).is_empty());
        assert_eq!(
            click_method_commands("default", true, &defaults),
            [
                "input \"1:2:Pad_A\" click_method button_areas",
                "input \"3:4:Pad_B\" click_method clickfinger",
            ]
        );
        assert_eq!(
            click_method_commands("fingers", true, &defaults),
            ["input type:touchpad click_method clickfinger"]
        );
    }
}
