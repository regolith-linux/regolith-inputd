use gio::prelude::SettingsExtManual;
use gio::{traits::SettingsExt, Settings};
use glib::{FromVariant, ToVariant};
use log::info;
use serde_json::Value;
use std::collections::BTreeSet;
use std::error::Error;
use std::sync::mpsc::Sender;
use std::sync::Mutex;
use swayipc::{ClickMethod, Connection as SwayConnection, EnabledOrDisabled, SendEvents};

use crate::Message;

/// Pointer types whose acceleration profile the daemon has set.
static ACCEL_PROFILE_SET: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

/// The command that applies a GNOME `accel-profile` value to every device of
/// `pointer_type`. "default" means the device's own default, which sway has
/// no value for: until the daemon has set a profile there is nothing to
/// undo, so it leaves the devices as libinput or the user's sway config set
/// them; after that it restores adaptive, libinput's default profile.
pub fn accel_profile_command(
    pointer_type: &str,
    gnome_value: &str,
    already_set: bool,
) -> Option<String> {
    let profile = match gnome_value {
        "flat" => "flat",
        "adaptive" => "adaptive",
        _ if already_set => "adaptive",
        _ => return None,
    };
    Some(format!("input type:{pointer_type} accel_profile {profile}"))
}

/// Set `key` to `value` unless it has that value already, so that
/// mirroring sway to GSettings only writes what differs.
pub fn set_if_changed<T>(settings: &Settings, key: &str, value: T) -> Result<(), Box<dyn Error>>
where
    T: ToVariant + FromVariant + PartialEq,
{
    if settings.get::<T>(key) != value {
        info!("Mirroring sway's {key} {} to gsettings", value.to_variant());
        settings.set(key, &value)?;
    }
    Ok(())
}

/// Set boolean `key` from a libinput setting sway reports as "enabled" or
/// "disabled".
pub fn set_enabled_if_changed(
    settings: &Settings,
    key: &str,
    value: &Value,
) -> Result<(), Box<dyn Error>> {
    match value.as_str() {
        Some("enabled") => set_if_changed(settings, key, true),
        Some("disabled") => set_if_changed(settings, key, false),
        _ => Ok(()),
    }
}

pub trait InputHandler {
    fn settings(&self) -> &Settings;
    fn sway_connection(&mut self) -> &mut SwayConnection;
    fn apply_changes(&mut self, _: &str) -> Result<(), Box<dyn Error>>;
    fn apply_all(&mut self) -> Result<(), Box<dyn Error>>;

    /// The type of the sway input devices whose settings this handler
    /// mirrors back to GSettings, if any.
    fn sway_input_type(&self) -> Option<&str> {
        None
    }

    /// Mirror a libinput setting (named as in sway's GET_INPUTS reply) that
    /// all devices of `sway_input_type` now have to GSettings.
    fn sync_gsettings(&self, _setting: &str, _value: &Value) -> Result<(), Box<dyn Error>> {
        Ok(())
    }

    /// Tell the owner of the handlers, as handler `index`, about every
    /// change to this handler's schema.
    fn monitor_gsettings_change(&self, index: usize, sender: Sender<Message>) {
        self.settings().connect_changed(None, move |_, key| {
            // The owner is gone only when the daemon is stopping.
            let _ = sender.send(Message::GSettingsChanged(index, key.to_string()));
        });
    }
}

pub trait PointerMethods: InputHandler {
    fn pointer_type(&self) -> &str;
    fn apply_left_handed(&mut self) -> Result<(), Box<dyn Error>>;
    fn apply_speed(&mut self) -> Result<(), Box<dyn Error>> {
        let new_val: f64 = self.settings().get("speed");
        let pointer_type = self.pointer_type();
        let cmd = format!("input type:{pointer_type} pointer_accel {new_val}");
        self.sway_connection().run_command(cmd)?;
        Ok(())
    }
    fn apply_accel_profile(&mut self) -> Result<(), Box<dyn Error>> {
        let has_key = self
            .settings()
            .settings_schema()
            .map_or(false, |schema| schema.has_key("accel-profile"));
        if !has_key {
            return Ok(());
        }
        let profile: String = self.settings().get("accel-profile");
        let pointer_type = self.pointer_type().to_string();
        let mut set = ACCEL_PROFILE_SET.lock().unwrap();
        let already_set = set.contains(&pointer_type);
        if let Some(cmd) = accel_profile_command(&pointer_type, &profile, already_set) {
            info!("{cmd}");
            self.sway_connection().run_command(cmd)?;
            set.insert(pointer_type);
        }
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
    /// Mirror the settings mice and touchpads share to GSettings.
    fn sync_pointer_gsettings(&self, setting: &str, value: &Value) -> Result<(), Box<dyn Error>> {
        match (setting, value.as_f64()) {
            ("accel_speed", Some(speed)) => {
                set_if_changed(self.settings(), "speed", gsettings_speed(speed))
            }
            ("natural_scroll", _) => {
                set_enabled_if_changed(self.settings(), "natural-scroll", value)
            }
            _ => Ok(()),
        }
    }
    fn apply_click_method(&mut self) -> Result<(), Box<dyn Error>>;
}

/// GSettings' `speed` for the acceleration speed sway reports. sway keeps
/// the speed as a float, so a speed set as 0.1 comes back as
/// 0.10000000149011612; it is mirrored as 0.1.
fn gsettings_speed(sway_speed: f64) -> f64 {
    (sway_speed as f32)
        .to_string()
        .parse()
        .unwrap_or(sway_speed)
}

pub trait SwayTypeToPrimitive<T> {
    fn to_primitive(&self) -> T;
}

pub trait PrimitiveToSwayType<T> {
    fn to_sway_type(self) -> T;
}

impl SwayTypeToPrimitive<bool> for EnabledOrDisabled {
    fn to_primitive(&self) -> bool {
        match self {
            EnabledOrDisabled::Enabled => true,
            EnabledOrDisabled::Disabled => false,
        }
    }
}

impl SwayTypeToPrimitive<&str> for EnabledOrDisabled {
    fn to_primitive(&self) -> &'static str {
        match self {
            EnabledOrDisabled::Enabled => "enabled",
            EnabledOrDisabled::Disabled => "disabled",
        }
    }
}

impl PrimitiveToSwayType<EnabledOrDisabled> for bool {
    fn to_sway_type(self) -> EnabledOrDisabled {
        if self {
            EnabledOrDisabled::Enabled
        } else {
            EnabledOrDisabled::Disabled
        }
    }
}

impl SwayTypeToPrimitive<bool> for SendEvents {
    fn to_primitive(&self) -> bool {
        matches!(self, SendEvents::Enabled)
    }
}

impl SwayTypeToPrimitive<&str> for SendEvents {
    fn to_primitive(&self) -> &'static str {
        match self {
            SendEvents::Enabled => "enabled",
            _ => "disabled",
        }
    }
}

impl SwayTypeToPrimitive<&str> for ClickMethod {
    fn to_primitive(&self) -> &'static str {
        match self {
            ClickMethod::ButtonAreas => "button_areas",
            ClickMethod::Clickfinger => "clickfinger",
            _ => "none",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::Cell;
    use std::rc::Rc;

    #[test]
    fn speeds_are_mirrored_at_the_precision_sway_keeps() {
        assert_eq!(gsettings_speed(0.1_f32 as f64), 0.1);
        assert_eq!(gsettings_speed(-0.5), -0.5);
        assert_eq!(gsettings_speed(0.7_f32 as f64), 0.7);
    }

    /// Mirroring writes a key only when its value differs, so sway's state
    /// never causes a redundant GSettings write.
    #[test]
    fn only_values_that_differ_are_written() {
        const MOUSE: &str = "org.gnome.desktop.peripherals.mouse";
        let installed = gio::SettingsSchemaSource::default()
            .and_then(|source| source.lookup(MOUSE, true))
            .is_some();
        if !installed {
            eprintln!("{MOUSE} is not installed, skipping");
            return;
        }
        let context = glib::MainContext::new();
        context
            .with_thread_default(|| {
                let settings = Settings::with_backend(MOUSE, &gio::memory_settings_backend_new());
                let writes = Rc::new(Cell::new(0));
                let counter = writes.clone();
                settings.connect_changed(None, move |_, _| counter.set(counter.get() + 1));
                let written = || {
                    while context.iteration(false) {}
                    writes.get()
                };
                // natural-scroll and speed start at false and 0.
                set_enabled_if_changed(&settings, "natural-scroll", &json!("disabled")).unwrap();
                assert_eq!(written(), 0);
                set_enabled_if_changed(&settings, "natural-scroll", &json!("enabled")).unwrap();
                assert_eq!(written(), 1);
                set_enabled_if_changed(&settings, "natural-scroll", &json!("enabled")).unwrap();
                set_enabled_if_changed(&settings, "natural-scroll", &json!(null)).unwrap();
                set_if_changed(&settings, "speed", 0.0).unwrap();
                assert_eq!(written(), 1);
                set_if_changed(&settings, "speed", 0.25).unwrap();
                assert_eq!(written(), 2);
                assert!(settings.boolean("natural-scroll"));
                assert_eq!(settings.double("speed"), 0.25);
            })
            .unwrap();
    }
}
