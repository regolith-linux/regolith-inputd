//! Mirrors GNOME's trackball settings to sway's trackballs.
//!
//! Trackballs are pointers like mice, and take the mouse's speed, natural
//! scroll and handedness from `input type:pointer` commands. Their own
//! settings (acceleration profile, scroll-wheel emulation button and its
//! lock, middle-click emulation) are applied to each trackball by its
//! identifier, which wins over sway's type commands.

use crate::traits::{
    sway_accel_profile, InputHandler, PrimitiveToSwayType, SwayTypeToPrimitive, ACCEL_PROFILE_SET,
    TRACKBALL_PROFILE,
};
use crate::utils;
use gio::{prelude::*, Settings, SettingsSchemaSource};
use log::{error, info};
use serde_json::Value;
use std::error::Error;
use swayipc::{Connection as SwayConnection, Input};

const TRACKBALL_SCHEMA: &str = "org.gnome.desktop.peripherals.trackball";
const MOUSE_SCHEMA: &str = "org.gnome.desktop.peripherals.mouse";

const BTN_LEFT: i32 = 0x110;
const BTN_RIGHT: i32 = 0x111;
const BTN_MIDDLE: i32 = 0x112;

/// Which of the trackball settings to apply.
#[derive(Clone, Copy)]
struct Parts {
    profile: bool,
    scroll: bool,
    middle: bool,
}

impl Parts {
    const ALL: Parts = Parts {
        profile: true,
        scroll: true,
        middle: true,
    };
    const PROFILE: Parts = Parts {
        profile: true,
        scroll: false,
        middle: false,
    };
    const SCROLL: Parts = Parts {
        profile: false,
        scroll: true,
        middle: false,
    };
    const MIDDLE: Parts = Parts {
        profile: false,
        scroll: false,
        middle: true,
    };
}

pub struct TrackballHandler {
    settings: Settings,
    /// The mouse settings: a mouse profile decides what "default" means for
    /// a trackball.
    mouse_settings: Settings,
    sway_connection: SwayConnection,
}

impl TrackballHandler {
    /// `None` where the installed schemas have no trackball settings.
    pub fn new() -> Option<TrackballHandler> {
        let source = SettingsSchemaSource::default()?;
        source.lookup(TRACKBALL_SCHEMA, true)?;
        Some(TrackballHandler {
            settings: Settings::new(TRACKBALL_SCHEMA),
            mouse_settings: Settings::new(MOUSE_SCHEMA),
            sway_connection: SwayConnection::new().unwrap(),
        })
    }

    fn has_key(settings: &Settings, key: &str) -> bool {
        settings
            .settings_schema()
            .is_some_and(|schema| schema.has_key(key))
    }

    /// sway's profile for the trackball's `accel-profile`. "default" is
    /// libinput's default, adaptive, once a pointer profile has taken effect
    /// in this session, from the mouse settings or the trackball's; before
    /// that it leaves trackballs as sway has them.
    fn profile(&self) -> Option<&'static str> {
        if !Self::has_key(&self.settings, "accel-profile") {
            return None;
        }
        let value: String = self.settings.get("accel-profile");
        let mouse_value: String = if Self::has_key(&self.mouse_settings, "accel-profile") {
            self.mouse_settings.get("accel-profile")
        } else {
            String::new()
        };
        let set = ACCEL_PROFILE_SET.lock().unwrap();
        // The mouse handler may apply a mouse profile only after this runs.
        let profile_in_effect = set.contains("pointer")
            || set.contains(TRACKBALL_PROFILE)
            || sway_accel_profile(&mouse_value, false).is_some();
        sway_accel_profile(&value, profile_in_effect)
    }

    /// The commands that apply `parts` of the settings to the trackballs
    /// among `devices`.
    fn commands(&self, devices: &[Value], parts: Parts) -> Vec<String> {
        let trackballs: Vec<&Value> = devices.iter().filter(|d| utils::is_trackball(d)).collect();
        if trackballs.is_empty() {
            return Vec::new();
        }
        let profile = if parts.profile { self.profile() } else { None };
        let scroll = (parts.scroll
            && Self::has_key(&self.settings, "scroll-wheel-emulation-button"))
        .then(|| {
            let button: i32 = self.settings.get("scroll-wheel-emulation-button");
            let lock = Self::has_key(&self.settings, "scroll-wheel-emulation-button-lock")
                && self
                    .settings
                    .get::<bool>("scroll-wheel-emulation-button-lock");
            (button, lock)
        });
        let middle = (parts.middle && Self::has_key(&self.settings, "middle-click-emulation"))
            .then(|| self.settings.get::<bool>("middle-click-emulation"));

        let mut commands = Vec::new();
        for device in trackballs {
            let identifier = utils::str_field(device, "identifier");
            if let Some(profile) = profile {
                commands.push(format!("input \"{identifier}\" accel_profile {profile}"));
            }
            if let Some((button, lock)) = scroll {
                commands.extend(scroll_commands(identifier, button, lock));
            }
            if let Some(enabled) = middle.filter(|_| utils::offers(device, "middle_emulation")) {
                let enabled: &str = enabled.to_sway_type().to_primitive();
                commands.push(format!("input \"{identifier}\" middle_emulation {enabled}"));
            }
        }
        if profile.is_some() {
            ACCEL_PROFILE_SET
                .lock()
                .unwrap()
                .insert(TRACKBALL_PROFILE.to_string());
        }
        commands
    }

    fn apply(&mut self, parts: Parts) -> Result<(), Box<dyn Error>> {
        let commands = self.commands(&utils::get_inputs_json()?, parts);
        utils::run_command_list(&mut self.sway_connection, &commands)?;
        Ok(())
    }
}

/// The evdev code of the button GNOME's `scroll-wheel-emulation-button`
/// names, or `None` for no button scrolling. GNOME numbers buttons as X11
/// does: 1, 2 and 3 are left, middle and right; 4 to 7 are the wheel, which
/// can't be held; from 8 on, X11 button n is evdev code n + 267, so 8 is the
/// side button and 9 the extra button.
fn scroll_button_code(button: i32) -> Option<i32> {
    match button {
        1 => Some(BTN_LEFT),
        2 => Some(BTN_MIDDLE),
        3 => Some(BTN_RIGHT),
        8.. => Some(button + BTN_LEFT - 5),
        _ => None,
    }
}

/// Commands for a trackball's scroll-wheel emulation: scrolling while
/// `button` is held, or no button scrolling for 0, with the button lock.
fn scroll_commands(identifier: &str, button: i32, lock: bool) -> Vec<String> {
    let mut commands = match scroll_button_code(button) {
        Some(code) => vec![
            format!("input \"{identifier}\" scroll_method on_button_down"),
            format!("input \"{identifier}\" scroll_button {code}"),
        ],
        None => vec![format!("input \"{identifier}\" scroll_method none")],
    };
    let lock: &str = lock.to_sway_type().to_primitive();
    commands.push(format!("input \"{identifier}\" scroll_button_lock {lock}"));
    commands
}

impl InputHandler for TrackballHandler {
    fn apply_changes(&mut self, key: &str) -> Result<(), Box<dyn Error>> {
        info!("{TRACKBALL_SCHEMA} -> Key: {key} changed");
        match key {
            "accel-profile" => self.apply(Parts::PROFILE)?,
            "scroll-wheel-emulation-button" | "scroll-wheel-emulation-button-lock" => {
                self.apply(Parts::SCROLL)?
            }
            "middle-click-emulation" => self.apply(Parts::MIDDLE)?,
            _ => (),
        };
        Ok(())
    }
    fn settings(&self) -> &Settings {
        &self.settings
    }
    fn sway_connection(&mut self) -> &mut SwayConnection {
        &mut self.sway_connection
    }
    fn apply_all(&mut self) -> Result<(), Box<dyn Error>> {
        self.apply(Parts::ALL)
    }
    fn sync_gsettings(&mut self, _: &Input) -> Result<(), Box<dyn Error>> {
        Ok(())
    }
    fn device_added(&mut self, device: &Value) -> Result<(), Box<dyn Error>> {
        let commands = self.commands(std::slice::from_ref(device), Parts::ALL);
        // The event thread runs this; a connection of its own keeps it apart
        // from commands a settings change may be sending at the same time.
        utils::run_commands(&commands)?;
        Ok(())
    }

    fn monitor_gsettings_change(&mut self)
    where
        Self: 'static,
    {
        let ptr: *mut Self = self;
        let apply = move |key: &str| unsafe {
            if !ptr.is_null() {
                if let Err(e) = (*ptr).apply_changes_sync(key) {
                    error!("{e}");
                }
            }
        };
        self.settings
            .connect_changed(None, move |_, key| apply(key));
        // A mouse profile can change what "default" means for trackballs.
        self.mouse_settings
            .connect_changed(Some("accel-profile"), move |_, _| apply("accel-profile"));
    }
}

unsafe impl Send for TrackballHandler {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn x11_buttons_map_to_evdev_codes() {
        assert_eq!(scroll_button_code(0), None);
        assert_eq!(scroll_button_code(1), Some(272));
        assert_eq!(scroll_button_code(2), Some(274));
        assert_eq!(scroll_button_code(3), Some(273));
        for wheel in 4..=7 {
            assert_eq!(scroll_button_code(wheel), None);
        }
        assert_eq!(scroll_button_code(8), Some(275));
        assert_eq!(scroll_button_code(9), Some(276));
        assert_eq!(scroll_button_code(12), Some(279));
    }

    #[test]
    fn scroll_button_zero_turns_button_scrolling_off() {
        assert_eq!(
            scroll_commands("1:2:Ball", 0, true),
            [
                "input \"1:2:Ball\" scroll_method none",
                "input \"1:2:Ball\" scroll_button_lock enabled",
            ]
        );
        assert_eq!(
            scroll_commands("1:2:Ball", 8, false),
            [
                "input \"1:2:Ball\" scroll_method on_button_down",
                "input \"1:2:Ball\" scroll_button 275",
                "input \"1:2:Ball\" scroll_button_lock disabled",
            ]
        );
    }

    #[test]
    fn trackballs_are_pointers_named_so() {
        use serde_json::json;
        assert!(utils::is_trackball(
            &json!({"type": "pointer", "name": "Kensington Expert TrackBall"})
        ));
        assert!(!utils::is_trackball(
            &json!({"type": "touchpad", "name": "Trackball Touchpad"})
        ));
        assert!(!utils::is_trackball(
            &json!({"type": "pointer", "name": "Ball Mouse"})
        ));
    }
}
