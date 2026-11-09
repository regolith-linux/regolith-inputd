use crate::InputHandler;
use gio::{prelude::SettingsExtManual, Settings};
use log::{debug, info, warn};
use serde_json::Value;
use std::env;
use std::error::Error;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use swayipc::{CommandType, Connection as SwayConnection, Input, MAGIC};

/// The Typing features of Settings > Accessibility.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Feature {
    Sticky,
    Slow,
    Bounce,
    Mouse,
}

const FEATURES: [Feature; 4] = [
    Feature::Sticky,
    Feature::Slow,
    Feature::Bounce,
    Feature::Mouse,
];

impl Feature {
    /// The feature a key of `org.gnome.desktop.a11y.keyboard` configures.
    fn of_key(key: &str) -> Option<Feature> {
        match key {
            "stickykeys-enable" | "stickykeys-two-key-off" => Some(Feature::Sticky),
            "slowkeys-enable" | "slowkeys-delay" => Some(Feature::Slow),
            "bouncekeys-enable" | "bouncekeys-delay" => Some(Feature::Bounce),
            "mousekeys-enable"
            | "mousekeys-max-speed"
            | "mousekeys-accel-time"
            | "mousekeys-init-delay" => Some(Feature::Mouse),
            _ => None,
        }
    }
}

/// The GNOME settings that sway's keyboard accessibility options mirror.
#[derive(Clone, Debug, Default)]
struct A11ySettings {
    stickykeys_enable: bool,
    stickykeys_two_key_off: bool,
    slowkeys_enable: bool,
    slowkeys_delay: i32,
    bouncekeys_enable: bool,
    bouncekeys_delay: i32,
    mousekeys_enable: bool,
    mousekeys_max_speed: i32,
    mousekeys_accel_time: i32,
    mousekeys_init_delay: i32,
}

impl A11ySettings {
    fn from_gsettings(settings: &Settings) -> A11ySettings {
        A11ySettings {
            stickykeys_enable: settings.get("stickykeys-enable"),
            stickykeys_two_key_off: settings.get("stickykeys-two-key-off"),
            slowkeys_enable: settings.get("slowkeys-enable"),
            slowkeys_delay: settings.get("slowkeys-delay"),
            bouncekeys_enable: settings.get("bouncekeys-enable"),
            bouncekeys_delay: settings.get("bouncekeys-delay"),
            mousekeys_enable: settings.get("mousekeys-enable"),
            mousekeys_max_speed: settings.get("mousekeys-max-speed"),
            mousekeys_accel_time: settings.get("mousekeys-accel-time"),
            mousekeys_init_delay: settings.get("mousekeys-init-delay"),
        }
    }

    fn enabled(&self, feature: Feature) -> bool {
        match feature {
            Feature::Sticky => self.stickykeys_enable,
            Feature::Slow => self.slowkeys_enable,
            Feature::Bounce => self.bouncekeys_enable,
            Feature::Mouse => self.mousekeys_enable,
        }
    }

    /// The sway commands that apply `feature` to every keyboard. Delays are
    /// in milliseconds and speeds in pixels per second on both sides; sway
    /// turns slow and bounce keys off with a delay of 0.
    fn commands(&self, feature: Feature) -> Vec<String> {
        let on_off = |on: bool| if on { "enabled" } else { "disabled" };
        let delay = |on: bool, ms: i32| if on { ms.max(0) } else { 0 };
        let options: Vec<(&str, String)> = match feature {
            Feature::Sticky if self.stickykeys_enable => vec![
                (
                    "sticky_keys_two_key_off",
                    on_off(self.stickykeys_two_key_off).into(),
                ),
                ("sticky_keys", on_off(true).into()),
            ],
            Feature::Sticky => vec![("sticky_keys", on_off(false).into())],
            Feature::Slow => vec![(
                "slow_keys",
                delay(self.slowkeys_enable, self.slowkeys_delay).to_string(),
            )],
            Feature::Bounce => vec![(
                "bounce_keys",
                delay(self.bouncekeys_enable, self.bouncekeys_delay).to_string(),
            )],
            Feature::Mouse if self.mousekeys_enable => vec![
                (
                    "mouse_keys_max_speed",
                    self.mousekeys_max_speed.max(1).to_string(),
                ),
                (
                    "mouse_keys_accel_time",
                    self.mousekeys_accel_time.max(0).to_string(),
                ),
                (
                    "mouse_keys_init_delay",
                    self.mousekeys_init_delay.max(0).to_string(),
                ),
                ("mouse_keys", on_off(true).into()),
            ],
            Feature::Mouse => vec![("mouse_keys", on_off(false).into())],
        };
        options
            .into_iter()
            .map(|(option, value)| format!("input type:keyboard {option} {value}"))
            .collect()
    }
}

/// Whether the running sway has the keyboard accessibility options. sway
/// before them rejects the commands as unknown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Support {
    Unknown,
    Yes,
    No,
}

/// Whether the sway that produced this `get_inputs` reply has the keyboard
/// accessibility options: it reports them on every keyboard. None when no
/// keyboard is attached to tell.
fn sway_has_a11y_options(inputs: &Value) -> Option<bool> {
    let mut keyboards = inputs
        .as_array()?
        .iter()
        .filter(|input| input["type"] == "keyboard")
        .peekable();
    keyboards.peek()?;
    Some(keyboards.any(|keyboard| keyboard.get("sticky_keys").is_some()))
}

/// sway's `get_inputs` reply, untyped: swayipc's `Input` drops the fields it
/// doesn't know about.
fn get_inputs_json() -> Result<Value, Box<dyn Error>> {
    let socket = env::var_os("I3SOCK")
        .or_else(|| env::var_os("SWAYSOCK"))
        .ok_or("SWAYSOCK is not set")?;
    let mut stream = UnixStream::connect(socket)?;
    stream.write_all(&CommandType::GetInputs.encode())?;
    let mut header = [0u8; 14];
    stream.read_exact(&mut header)?;
    if header[..6] != MAGIC {
        return Err("invalid reply from sway".into());
    }
    let len = u32::from_ne_bytes(header[6..10].try_into()?) as usize;
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload)?;
    Ok(serde_json::from_slice(&payload)?)
}

/// Mirrors GNOME's keyboard accessibility settings (the Typing section of
/// Settings > Accessibility: sticky keys, slow keys, bounce keys and mouse
/// keys) to sway's keyboard accessibility options, when the running sway
/// has them.
pub struct A11yKeyboardHandler {
    settings: Settings,
    sway_connection: SwayConnection,
    support: Support,
    /// Features the daemon has set in sway. A feature that is off in GNOME
    /// and was never set is left as the user's sway config has it.
    applied: [bool; 4],
}

impl A11yKeyboardHandler {
    pub fn new() -> A11yKeyboardHandler {
        let settings = Settings::new("org.gnome.desktop.a11y.keyboard");
        let sway_connection = SwayConnection::new().unwrap();
        A11yKeyboardHandler {
            settings,
            sway_connection,
            support: Support::Unknown,
            applied: [false; 4],
        }
    }

    fn sway_supports_a11y(&mut self) -> bool {
        if self.support == Support::Unknown {
            match get_inputs_json().map(|inputs| sway_has_a11y_options(&inputs)) {
                Ok(Some(true)) => self.support = Support::Yes,
                Ok(Some(false)) => {
                    info!("sway has no keyboard accessibility options, the Typing settings are not applied");
                    self.support = Support::No;
                }
                Ok(None) => {
                    debug!("No keyboard attached, cannot tell if sway has accessibility options")
                }
                Err(e) => debug!("Cannot query sway's inputs: {e}"),
            }
        }
        self.support == Support::Yes
    }

    fn apply_features(&mut self, features: &[Feature]) -> Result<(), Box<dyn Error>> {
        let settings = A11ySettings::from_gsettings(&self.settings);
        self.apply(&settings, features)
    }

    fn apply(
        &mut self,
        settings: &A11ySettings,
        features: &[Feature],
    ) -> Result<(), Box<dyn Error>> {
        let features: Vec<Feature> = features
            .iter()
            .copied()
            .filter(|&f| settings.enabled(f) || self.applied[f as usize])
            .collect();
        if features.is_empty() || !self.sway_supports_a11y() {
            return Ok(());
        }
        for feature in features {
            for cmd in settings.commands(feature) {
                info!("{cmd}");
                for outcome in self.sway_connection.run_command(&cmd)? {
                    if let Err(e) = outcome {
                        warn!("{cmd}: {e}");
                    }
                }
            }
            self.applied[feature as usize] = true;
        }
        Ok(())
    }
}

impl InputHandler for A11yKeyboardHandler {
    fn apply_changes(&mut self, key: &str) -> Result<(), Box<dyn Error>> {
        let Some(feature) = Feature::of_key(key) else {
            return Ok(());
        };
        info!("org.gnome.desktop.a11y.keyboard -> Key: {key} changed");
        let settings = A11ySettings::from_gsettings(&self.settings);
        // The delays and speeds of a feature that is off wait until it is
        // turned on.
        if settings.enabled(feature) || key.ends_with("-enable") {
            self.apply(&settings, &[feature])?;
        }
        Ok(())
    }
    fn settings(&self) -> &Settings {
        &self.settings
    }
    fn sway_connection(&mut self) -> &mut SwayConnection {
        &mut self.sway_connection
    }
    fn apply_all(&mut self) -> Result<(), Box<dyn Error>> {
        self.apply_features(&FEATURES)
    }
    fn sync_gsettings(&mut self, _: &Input) -> Result<(), Box<dyn Error>> {
        Ok(())
    }
}

unsafe impl Send for A11yKeyboardHandler {}

#[cfg(test)]
mod tests {
    use super::*;

    fn gnome_defaults() -> A11ySettings {
        // gsettings-desktop-schemas defaults
        A11ySettings {
            slowkeys_delay: 300,
            bouncekeys_delay: 300,
            mousekeys_max_speed: 10,
            mousekeys_accel_time: 300,
            mousekeys_init_delay: 300,
            ..Default::default()
        }
    }

    #[test]
    fn sticky_keys_map_to_sway_options() {
        let mut s = gnome_defaults();
        assert_eq!(
            s.commands(Feature::Sticky),
            ["input type:keyboard sticky_keys disabled"]
        );
        s.stickykeys_enable = true;
        assert_eq!(
            s.commands(Feature::Sticky),
            [
                "input type:keyboard sticky_keys_two_key_off disabled",
                "input type:keyboard sticky_keys enabled"
            ]
        );
        s.stickykeys_two_key_off = true;
        assert_eq!(
            s.commands(Feature::Sticky)[0],
            "input type:keyboard sticky_keys_two_key_off enabled"
        );
    }

    #[test]
    fn slow_and_bounce_keys_are_delays_zero_when_off() {
        let mut s = gnome_defaults();
        assert_eq!(
            s.commands(Feature::Slow),
            ["input type:keyboard slow_keys 0"]
        );
        assert_eq!(
            s.commands(Feature::Bounce),
            ["input type:keyboard bounce_keys 0"]
        );
        s.slowkeys_enable = true;
        s.bouncekeys_enable = true;
        s.bouncekeys_delay = -5;
        assert_eq!(
            s.commands(Feature::Slow),
            ["input type:keyboard slow_keys 300"]
        );
        assert_eq!(
            s.commands(Feature::Bounce),
            ["input type:keyboard bounce_keys 0"]
        );
    }

    #[test]
    fn mouse_keys_map_to_sway_options() {
        let mut s = gnome_defaults();
        assert_eq!(
            s.commands(Feature::Mouse),
            ["input type:keyboard mouse_keys disabled"]
        );
        s.mousekeys_enable = true;
        assert_eq!(
            s.commands(Feature::Mouse),
            [
                "input type:keyboard mouse_keys_max_speed 10",
                "input type:keyboard mouse_keys_accel_time 300",
                "input type:keyboard mouse_keys_init_delay 300",
                "input type:keyboard mouse_keys enabled"
            ]
        );
        // sway rejects a speed below 1 and negative times
        s.mousekeys_max_speed = 0;
        s.mousekeys_accel_time = -1;
        s.mousekeys_init_delay = -1;
        assert_eq!(
            s.commands(Feature::Mouse)[..3],
            [
                "input type:keyboard mouse_keys_max_speed 1",
                "input type:keyboard mouse_keys_accel_time 0",
                "input type:keyboard mouse_keys_init_delay 0"
            ]
        );
    }

    #[test]
    fn every_typing_key_has_a_feature() {
        for key in [
            "stickykeys-enable",
            "stickykeys-two-key-off",
            "slowkeys-enable",
            "slowkeys-delay",
            "bouncekeys-enable",
            "bouncekeys-delay",
            "mousekeys-enable",
            "mousekeys-max-speed",
            "mousekeys-accel-time",
            "mousekeys-init-delay",
        ] {
            assert!(Feature::of_key(key).is_some(), "{key}");
        }
        assert_eq!(Feature::of_key("togglekeys-enable"), None);
        assert_eq!(Feature::of_key("slowkeys-beep-press"), None);
    }

    #[test]
    fn support_is_read_from_keyboards_in_get_inputs() {
        let old: Value = serde_json::from_str(
            r#"[{"type": "pointer"}, {"type": "keyboard", "repeat_delay": 600}]"#,
        )
        .unwrap();
        let new: Value =
            serde_json::from_str(r#"[{"type": "keyboard", "sticky_keys": false, "slow_keys": 0}]"#)
                .unwrap();
        let none: Value = serde_json::from_str(r#"[{"type": "pointer"}]"#).unwrap();
        assert_eq!(sway_has_a11y_options(&old), Some(false));
        assert_eq!(sway_has_a11y_options(&new), Some(true));
        assert_eq!(sway_has_a11y_options(&none), None);
    }
}
