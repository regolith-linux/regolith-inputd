use gio::prelude::SettingsExtManual;
use gio::{traits::SettingsExt, Settings};
use log::error;
use std::error::Error;
use std::sync::atomic::Ordering;
use std::thread;
use std::time::Duration;
use swayipc::{Connection as SwayConnection, EnabledOrDisabled, Input, SendEvents};

use crate::{ALLOW_GSETTINGS_APPLY, ALLOW_SWAYINPUT_APPLY};

pub trait InputHandler {
    fn settings(&self) -> &Settings;
    fn sway_connection(&mut self) -> &mut SwayConnection;
    fn apply_changes(&mut self, _: &str) -> Result<(), Box<dyn Error>>;
    fn apply_all(&mut self) -> Result<(), Box<dyn Error>>;
    fn sync_gsettings(&mut self, _: &Input) -> Result<(), Box<dyn Error>>;

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
