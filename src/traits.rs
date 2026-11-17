use gio::prelude::SettingsExtManual;
use gio::{traits::SettingsExt, Settings};
use log::{error, info};
use std::collections::BTreeSet;
use std::error::Error;
use std::sync::Mutex;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;
use swayipc::{ClickMethod, Connection as SwayConnection, EnabledOrDisabled, Input, SendEvents};

use crate::{ALLOW_GSETTINGS_APPLY, ALLOW_SWAYINPUT_APPLY};

/// Pointer types whose acceleration profile the daemon has set. Trackballs,
/// which are configured one by one rather than by type, are recorded as
/// [`TRACKBALL_PROFILE`].
pub(crate) static ACCEL_PROFILE_SET: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

/// The [`ACCEL_PROFILE_SET`] entry for trackballs.
pub(crate) const TRACKBALL_PROFILE: &str = "trackball";

/// sway's `accel_profile` for a GNOME `accel-profile` value. "default" means
/// the device's own default, which sway has no value for: until the daemon
/// has set a profile there is nothing to undo, so it leaves the devices as
/// libinput or the user's sway config set them; after that it restores
/// adaptive, libinput's default profile.
pub fn sway_accel_profile(gnome_value: &str, already_set: bool) -> Option<&'static str> {
    match gnome_value {
        "flat" => Some("flat"),
        "adaptive" => Some("adaptive"),
        _ if already_set => Some("adaptive"),
        _ => None,
    }
}

/// The command that applies a GNOME `accel-profile` value to every device of
/// `pointer_type`; see [`sway_accel_profile`].
pub fn accel_profile_command(
    pointer_type: &str,
    gnome_value: &str,
    already_set: bool,
) -> Option<String> {
    let profile = sway_accel_profile(gnome_value, already_set)?;
    Some(format!("input type:{pointer_type} accel_profile {profile}"))
}

pub trait InputHandler {
    fn settings(&self) -> &Settings;
    fn sway_connection(&mut self) -> &mut SwayConnection;
    fn apply_changes(&mut self, _: &str) -> Result<(), Box<dyn Error>>;
    fn apply_all(&mut self) -> Result<(), Box<dyn Error>>;
    fn sync_gsettings(&mut self, _: &Input) -> Result<(), Box<dyn Error>>;

    /// Configure a device sway has just reported as added (its GET_INPUTS
    /// object). Settings sway applies by device type reach new devices on
    /// their own; handlers that configure devices one by one override this.
    fn device_added(&mut self, _device: &serde_json::Value) -> Result<(), Box<dyn Error>> {
        Ok(())
    }

    fn apply_changes_sync(&mut self, key: &str) -> Result<(), Box<dyn Error>> {
        unsafe {
            ALLOW_SWAYINPUT_APPLY.store(false, Ordering::Relaxed);
        }
        let result = self.apply_changes(key);

        thread::sleep(Duration::from_millis(100));

        unsafe {
            let allow = ALLOW_SWAYINPUT_APPLY.load(Ordering::Relaxed) && true;
            ALLOW_SWAYINPUT_APPLY.store(allow, Ordering::Relaxed);
        }
        result
    }

    fn apply_all_sync(&mut self) -> Result<(), Box<dyn Error>> {
        unsafe {
            if !ALLOW_GSETTINGS_APPLY.load(Ordering::Relaxed) {
                return Ok(());
            }
            ALLOW_SWAYINPUT_APPLY.store(false, Ordering::Relaxed);
        }

        let result = self.apply_all();

        thread::sleep(Duration::from_millis(100));

        unsafe {
            let allow = ALLOW_SWAYINPUT_APPLY.load(Ordering::Relaxed) && true;
            ALLOW_SWAYINPUT_APPLY.store(allow, Ordering::Relaxed);
        }
        result
    }

    fn sync_gsettings_sync(&mut self, _: &Input) -> Result<(), Box<dyn Error>> {
        unsafe {
            if !ALLOW_SWAYINPUT_APPLY.load(Ordering::Relaxed) {
                return Ok(());
            }
            ALLOW_GSETTINGS_APPLY.store(false, Ordering::Relaxed);
        }

        let result = self.apply_all();

        thread::sleep(Duration::from_millis(100));

        unsafe {
            let allow = ALLOW_GSETTINGS_APPLY.load(Ordering::Relaxed) && true;
            ALLOW_GSETTINGS_APPLY.store(allow, Ordering::Relaxed);
        }
        result
    }

    fn monitor_gsettings_change(&mut self)
    where
        Self: 'static,
    {
        let ptr: *mut Self = self;
        self.settings().connect_changed(None, move |_, key| unsafe {
            if !ptr.is_null() {
                if let Err(e) = (*ptr).apply_changes_sync(key) {
                    error!("{e}");
                };
            }
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
    fn sync_pointer_gsettings(&self, input: &Input) -> Result<(), Box<dyn Error>> {
        if input.libinput.is_none() {
            return Ok(());
        }
        let libinput = input.libinput.as_ref().unwrap();
        if let Some(speed) = libinput.accel_speed {
            self.settings().set_double("speed", speed)?;
        }
        if let Some(natural) = libinput.natural_scroll.as_ref() {
            self.settings()
                .set_boolean("natural-scroll", natural.to_primitive())?;
        }
        if let Some(accel) = libinput.accel_speed {
            self.settings().set_double("speed", accel)?;
        }
        Ok(())
    }
    fn apply_click_method(&mut self) -> Result<(), Box<dyn Error>>;
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
