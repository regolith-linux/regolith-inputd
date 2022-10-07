use gio::prelude::SettingsExtManual;
use gio::{traits::SettingsExt, Settings};
use log::{debug, error};
use std::error::Error;
use std::sync::mpsc::{Receiver, Sender};
use swayipc::{Connection as SwayConnection, EnabledOrDisabled, Input, SendEvents};
pub trait InputHandler {
    fn settings(&self) -> &Settings;
    fn sway_connection(&mut self) -> &mut SwayConnection;
    fn monitor_sway_inputs(&self) {}
    fn apply_changes(&mut self, _: &str) -> Result<(), Box<dyn Error>> {
        Ok(())
    }
    fn get_swayinput_tx(&self) -> Sender<Input>;
    fn get_swayinput_rx(&self) -> &Receiver<Input>;
    fn monitor_gsettings_change(&mut self)
    where
        Self: 'static,
    {
        let ptr: *mut Self = self;
        self.settings().connect_changed(None, move |_, key| unsafe {
            if !ptr.is_null() {
                if let Err(e) = (*ptr).apply_changes(key) {
                    error!("{e}");
                };
            }
        });
    }
    fn sync_gsettings(&mut self, _: Input) -> Result<(), Box<dyn Error>> {
        Ok(())
    }
    fn apply_all(&mut self) -> Result<(), Box<dyn Error>>;
}

pub trait PointerMethods: InputHandler {
    fn pointer_type(&self) -> &str {
        "pointer"
    }
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
    fn apply_left_handed(&mut self) -> Result<(), Box<dyn Error>> {
        let new_val: &str = self
            .settings()
            .get::<bool>("left-handed")
            .to_sway_type()
            .to_primitive();
        let pointer_type = self.pointer_type();
        let cmd = format!("input type:{pointer_type} left_handed {new_val}");
        debug!("{cmd}");
        self.sway_connection().run_command(cmd)?;
        Ok(())
    }
    fn sync_pointer_gsettings(&self, input: &Input) -> Result<(), Box<dyn Error>> {
        if input.libinput.is_none() {
            return Ok(());
        }
        let libinput = input.libinput.as_ref().unwrap();
        if let Some(enabled) = libinput.send_events.as_ref() {
            self.settings()
                .set_string("send-events", enabled.to_primitive())?;
        }
        if let Some(speed) = libinput.accel_speed {
            self.settings().set_double("speed", speed)?;
        }
        if let Some(natural) = libinput.natural_scroll.as_ref() {
            self.settings()
                .set_boolean("natural-scroll", natural.to_primitive())?;
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
        match self {
            SendEvents::Enabled => true,
            _ => false,
        }
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
