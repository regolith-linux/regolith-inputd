use crate::traits::{InputHandler, PointerMethods, PrimitiveToSwayType, SwayTypeToPrimitive};
use crate::utils;
use gio::{prelude::SettingsExtManual, traits::SettingsExt, Settings};
use log::info;
use serde_json::Value;
use std::error::Error;
use swayipc::{Connection as SwayConnection, Input};
pub struct MouseHandler {
    settings: Settings,
    sway_connection: SwayConnection,
}

impl MouseHandler {
    pub fn new() -> MouseHandler {
        let settings = Settings::new("org.gnome.desktop.peripherals.mouse");
        let sway_connection = SwayConnection::new().unwrap();
        MouseHandler {
            settings,
            sway_connection,
        }
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
        self.sway_connection().run_command(cmd)?;
        Ok(())
    }

    fn middle_emulation(&self) -> Option<bool> {
        let has_key = self
            .settings()
            .settings_schema()
            .is_some_and(|schema| schema.has_key("middle-click-emulation"));
        has_key.then(|| self.settings().get("middle-click-emulation"))
    }

    /// Middle-click emulation, for every mouse sway reports as offering it.
    fn apply_middle_emulation(&mut self) -> Result<(), Box<dyn Error>> {
        let Some(enabled) = self.middle_emulation() else {
            return Ok(());
        };
        let commands = middle_emulation_commands(&utils::get_inputs_json()?, enabled);
        utils::run_command_list(self.sway_connection(), &commands)?;
        Ok(())
    }
}

/// Whether a GET_INPUTS pointer is a pointing stick. sway reports sticks as
/// plain pointers, so the kernel's device name is what tells them apart.
fn is_pointing_stick(device: &Value) -> bool {
    let name = utils::str_field(device, "name").to_lowercase();
    [
        "trackpoint",
        "pointing stick",
        "pointingstick",
        "dualpoint stick",
    ]
    .iter()
    .any(|marker| name.contains(marker))
}

/// Commands that set middle-click emulation on each of `devices` that is a
/// mouse offering it. Trackballs follow their own setting and touchpads
/// theirs, so neither is included, nor is a mouse that lacks the feature.
/// Pointing sticks keep whatever middle emulation they already have.
fn middle_emulation_commands(devices: &[Value], enabled: bool) -> Vec<String> {
    let value: &str = enabled.to_sway_type().to_primitive();
    devices
        .iter()
        .filter(|d| utils::str_field(d, "type") == "pointer" && !utils::is_trackball(d))
        .filter(|d| !is_pointing_stick(d))
        .filter(|d| utils::offers(d, "middle_emulation"))
        .map(|d| {
            let identifier = utils::str_field(d, "identifier");
            format!("input \"{identifier}\" middle_emulation {value}")
        })
        .collect()
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
        self.sway_connection().run_command(cmd)?;
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
            "middle-click-emulation" => self.apply_middle_emulation()?,
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
        self.apply_middle_emulation()?;
        Ok(())
    }
    fn device_added(&mut self, device: &Value) -> Result<(), Box<dyn Error>> {
        let Some(enabled) = self.middle_emulation() else {
            return Ok(());
        };
        let commands = middle_emulation_commands(std::slice::from_ref(device), enabled);
        // The event thread runs this; a connection of its own keeps it apart
        // from commands a settings change may be sending at the same time.
        utils::run_commands(&commands)?;
        Ok(())
    }
    fn sync_gsettings(&mut self, input: &Input) -> Result<(), Box<dyn Error>> {
        info!("Syncronizing mouse input state of sway with gsettings...");
        self.sync_pointer_gsettings(input)?;
        if input.libinput.is_none() {
            return Ok(());
        }
        let libinput = input.libinput.as_ref().unwrap();
        if let Some(left_handed) = libinput.left_handed.as_ref() {
            self.settings()
                .set_boolean("left-handed", left_handed.to_primitive())?;
        }
        Ok(())
    }
}

unsafe impl Send for MouseHandler {}

#[cfg(test)]
mod tests {
    use super::middle_emulation_commands;
    use serde_json::json;

    #[test]
    fn middle_emulation_reaches_only_mice_offering_it() {
        let devices = [
            json!({"identifier": "1:1:Mouse", "name": "Mouse", "type": "pointer",
                   "libinput": {"middle_emulation": "disabled"}}),
            json!({"identifier": "1:2:Plain_Mouse", "name": "Plain Mouse", "type": "pointer",
                   "libinput": {"accel_speed": 0.0}}),
            json!({"identifier": "1:3:Big_TrackBall", "name": "Big TrackBall", "type": "pointer",
                   "libinput": {"middle_emulation": "disabled"}}),
            json!({"identifier": "1:4:Touchpad", "name": "Touchpad", "type": "touchpad",
                   "libinput": {"middle_emulation": "disabled"}}),
            json!({"identifier": "2:10:TPPS2_IBM_TrackPoint", "name": "TPPS/2 IBM TrackPoint",
                   "type": "pointer", "libinput": {"middle_emulation": "enabled"}}),
            json!({"identifier": "2:8:AlpsPS2_ALPS_DualPoint_Stick",
                   "name": "AlpsPS/2 ALPS DualPoint Stick", "type": "pointer",
                   "libinput": {"middle_emulation": "enabled"}}),
        ];
        assert_eq!(
            middle_emulation_commands(&devices, true),
            ["input \"1:1:Mouse\" middle_emulation enabled"]
        );
        assert_eq!(
            middle_emulation_commands(&devices, false),
            ["input \"1:1:Mouse\" middle_emulation disabled"]
        );
    }
}
