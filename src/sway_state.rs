use crate::utils;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use swayipc::Fallible;

/// An input device as sway reports it: identifier, type and libinput
/// settings.
#[derive(Clone)]
struct Device {
    identifier: String,
    input_type: String,
    libinput: Map<String, Value>,
}

/// A libinput setting that every device of a type now agrees on, and that
/// differs from what they agreed on before.
#[derive(Debug, PartialEq)]
pub struct Change {
    /// The devices' type, as sway reports it ("pointer", "touchpad").
    pub input_type: String,
    /// The setting, named as in sway's GET_INPUTS reply ("accel_speed").
    pub setting: String,
    pub value: Value,
}

/// The libinput settings of sway's input devices. Several devices can
/// share an identifier (a mouse with a relative and an absolute node, a
/// receiver for several devices), so they are kept in a list.
#[derive(Default)]
pub struct SwayState(Vec<Device>);

impl SwayState {
    pub fn read() -> Fallible<SwayState> {
        Ok(SwayState::from_inputs(utils::get_inputs_json()?))
    }

    /// The state of the devices in a GET_INPUTS reply.
    pub fn from_inputs(inputs: Vec<Value>) -> SwayState {
        let mut devices = Vec::new();
        for input in inputs {
            let (Some(identifier), Some(input_type)) =
                (input["identifier"].as_str(), input["type"].as_str())
            else {
                continue;
            };
            devices.push(Device {
                identifier: identifier.to_string(),
                input_type: input_type.to_string(),
                libinput: input["libinput"].as_object().cloned().unwrap_or_default(),
            });
        }
        SwayState(devices)
    }

    fn identifiers(&self, input_type: &str) -> Vec<&str> {
        let mut identifiers: Vec<&str> = self
            .0
            .iter()
            .filter(|d| d.input_type == input_type)
            .map(|d| d.identifier.as_str())
            .collect();
        identifiers.sort_unstable();
        identifiers
    }

    /// The settings on which all devices of `input_type` that have them
    /// agree. GSettings has one value for all devices of a type, so a
    /// setting that differs between devices has none. sway leaves out the
    /// settings a device lacks (an absolute pointer has no acceleration, a
    /// touchpad without tapping has no tap settings), so such a device
    /// doesn't count; a null is read the same way.
    fn common_settings(&self, input_type: &str) -> BTreeMap<&str, &Value> {
        let mut common: BTreeMap<&str, Option<&Value>> = BTreeMap::new();
        for device in self.0.iter().filter(|d| d.input_type == input_type) {
            for (setting, value) in device.libinput.iter().filter(|(_, v)| !v.is_null()) {
                common
                    .entry(setting)
                    .and_modify(|agreed| {
                        if *agreed != Some(value) {
                            *agreed = None;
                        }
                    })
                    .or_insert(Some(value));
            }
        }
        common
            .into_iter()
            .filter_map(|(setting, value)| Some((setting, value?)))
            .collect()
    }

    /// The settings, by device type, that every device of the type changed
    /// to between `self` and `new`: what a user does with `swaymsg input
    /// type:...` or by configuring their only device. Types whose devices
    /// were added or removed in between are left out, since a device that
    /// was just plugged in is no change the user made.
    pub fn changes(&self, new: &SwayState) -> Vec<Change> {
        let types: BTreeSet<&str> = new.0.iter().map(|d| d.input_type.as_str()).collect();
        let mut changes = Vec::new();
        for input_type in types {
            if self.identifiers(input_type) != new.identifiers(input_type) {
                continue;
            }
            let old = self.common_settings(input_type);
            for (setting, value) in new.common_settings(input_type) {
                if old.get(setting) != Some(&value) {
                    changes.push(Change {
                        input_type: input_type.to_string(),
                        setting: setting.to_string(),
                        value: value.clone(),
                    });
                }
            }
        }
        changes
    }

    /// Take the devices of every type whose devices were added or removed
    /// between `self` and `new` as they are in `new`.
    pub fn adopt_devices(&mut self, new: &SwayState) {
        let types: BTreeSet<String> = self
            .0
            .iter()
            .chain(&new.0)
            .map(|d| d.input_type.clone())
            .collect();
        for input_type in types {
            if self.identifiers(&input_type) != new.identifiers(&input_type) {
                self.0.retain(|d| d.input_type != input_type);
                self.0
                    .extend(new.0.iter().filter(|d| d.input_type == input_type).cloned());
            }
        }
    }

    /// Whether the devices of the change's type, the same ones as in
    /// `before`, still agree on its value.
    pub fn still_holds(&self, before: &SwayState, change: &Change) -> bool {
        let input_type = change.input_type.as_str();
        self.identifiers(input_type) == before.identifiers(input_type)
            && self
                .common_settings(input_type)
                .get(change.setting.as_str())
                == Some(&&change.value)
    }
}

#[cfg(test)]
mod tests {
    use super::{Change, SwayState};
    use serde_json::{json, Value};

    fn device(identifier: &str, input_type: &str, libinput: Value) -> Value {
        json!({"identifier": identifier, "type": input_type, "libinput": libinput})
    }

    fn state(devices: &[Value]) -> SwayState {
        SwayState::from_inputs(devices.to_vec())
    }

    fn change(input_type: &str, setting: &str, value: Value) -> Change {
        Change {
            input_type: input_type.to_string(),
            setting: setting.to_string(),
            value,
        }
    }

    #[test]
    fn a_change_to_every_device_of_a_type_is_reported() {
        let old = state(&[
            device(
                "1:1:Pad",
                "touchpad",
                json!({"tap": "disabled", "dwt": "enabled"}),
            ),
            device(
                "2:2:Mouse",
                "pointer",
                json!({"natural_scroll": "disabled"}),
            ),
        ]);
        let new = state(&[
            device(
                "1:1:Pad",
                "touchpad",
                json!({"tap": "enabled", "dwt": "enabled"}),
            ),
            device(
                "2:2:Mouse",
                "pointer",
                json!({"natural_scroll": "disabled"}),
            ),
        ]);
        assert_eq!(
            old.changes(&new),
            [change("touchpad", "tap", json!("enabled"))]
        );
        assert!(new.changes(&new).is_empty());
    }

    #[test]
    fn a_change_to_one_of_several_devices_is_not_reported() {
        let mice = |second: &str| {
            state(&[
                device("1:1:A", "pointer", json!({"left_handed": "disabled"})),
                device("2:2:B", "pointer", json!({"left_handed": second})),
            ])
        };
        assert!(mice("disabled").changes(&mice("enabled")).is_empty());
        // When the devices agree again, their common value is reported.
        assert_eq!(
            mice("enabled").changes(&mice("disabled")),
            [change("pointer", "left_handed", json!("disabled"))]
        );
    }

    /// Agreement is per type and per setting: two touchpads that disagree on
    /// tapping still have a common speed, and a mouse is no touchpad.
    #[test]
    fn agreement_is_per_type_and_setting() {
        let devices = |second_tap: &str, speed: f64| {
            state(&[
                device(
                    "1:1:Pad",
                    "touchpad",
                    json!({"tap": "enabled", "accel_speed": speed}),
                ),
                device(
                    "2:2:Pad",
                    "touchpad",
                    json!({"tap": second_tap, "accel_speed": speed}),
                ),
                device("3:3:Mouse", "pointer", json!({"accel_speed": 0.0})),
            ])
        };
        assert_eq!(
            devices("disabled", 0.0).changes(&devices("disabled", 0.5)),
            [change("touchpad", "accel_speed", json!(0.5))]
        );
    }

    #[test]
    fn devices_plugged_in_or_out_are_not_changes() {
        let one = state(&[device("1:1:A", "pointer", json!({"accel_speed": 0.5}))]);
        let two = state(&[
            device("1:1:A", "pointer", json!({"accel_speed": 0.5})),
            device("2:2:B", "pointer", json!({"accel_speed": 0.0})),
        ]);
        let other = state(&[device("2:2:B", "pointer", json!({"accel_speed": 0.0}))]);
        assert!(SwayState::default().changes(&one).is_empty());
        assert!(one.changes(&two).is_empty());
        assert!(two.changes(&other).is_empty());
        assert!(one.changes(&other).is_empty());
    }

    #[test]
    fn changes_after_adopting_plugged_in_devices_are_reported() {
        let one = state(&[device("1:1:A", "pointer", json!({"tap": "disabled"}))]);
        let two = |second: &str| {
            state(&[
                device("1:1:A", "pointer", json!({"tap": "disabled"})),
                device("2:2:B", "pointer", json!({"tap": second})),
            ])
        };
        let mut known = one;
        known.adopt_devices(&two("enabled"));
        assert!(known.changes(&two("enabled")).is_empty());
        assert_eq!(
            known.changes(&state(&[
                device("1:1:A", "pointer", json!({"tap": "enabled"})),
                device("2:2:B", "pointer", json!({"tap": "enabled"})),
            ])),
            [change("pointer", "tap", json!("enabled"))]
        );
    }

    #[test]
    fn devices_sharing_an_identifier_are_all_kept() {
        let mouse = |relative: &str, absolute: &str| {
            state(&[
                device(
                    "2:19:VMMouse",
                    "pointer",
                    json!({"natural_scroll": relative}),
                ),
                device(
                    "2:19:VMMouse",
                    "pointer",
                    json!({"natural_scroll": absolute}),
                ),
            ])
        };
        let one_node = state(&[device(
            "2:19:VMMouse",
            "pointer",
            json!({"natural_scroll": "enabled"}),
        )]);
        assert!(mouse("disabled", "disabled")
            .changes(&mouse("enabled", "disabled"))
            .is_empty());
        assert_eq!(
            mouse("disabled", "disabled").changes(&mouse("enabled", "enabled")),
            [change("pointer", "natural_scroll", json!("enabled"))]
        );
        assert!(mouse("disabled", "disabled").changes(&one_node).is_empty());
    }

    /// sway 1.11 leaves out what a device can't do: VMware's absolute mouse
    /// node has no `accel_speed`, while its relative node, with the same
    /// identifier, has one.
    #[test]
    fn devices_without_a_setting_do_not_count() {
        let pointers = |speed: f64| {
            state(&[
                device(
                    "2:19:VMMouse",
                    "pointer",
                    json!({"natural_scroll": "disabled"}),
                ),
                device(
                    "2:19:VMMouse",
                    "pointer",
                    json!({"accel_speed": speed, "natural_scroll": "disabled"}),
                ),
            ])
        };
        assert_eq!(
            pointers(0.0).changes(&pointers(0.5)),
            [change("pointer", "accel_speed", json!(0.5))]
        );
    }

    #[test]
    fn settings_reported_as_null_count_as_left_out() {
        let pointers = |speed: f64| {
            state(&[
                device("1:1:Mouse", "pointer", json!({"accel_speed": speed})),
                device("2:2:Absolute", "pointer", json!({"accel_speed": null})),
            ])
        };
        assert_eq!(
            pointers(0.0).changes(&pointers(0.25)),
            [change("pointer", "accel_speed", json!(0.25))]
        );
    }

    #[test]
    fn a_change_still_holds_until_the_devices_change_it_or_change() {
        let pads = |tap: &str| state(&[device("1:1:Pad", "touchpad", json!({"tap": tap}))]);
        let before = pads("enabled");
        let tap = change("touchpad", "tap", json!("enabled"));
        assert!(pads("enabled").still_holds(&before, &tap));
        assert!(!pads("disabled").still_holds(&before, &tap));
        let plugged = state(&[
            device("1:1:Pad", "touchpad", json!({"tap": "enabled"})),
            device("2:2:Pad", "touchpad", json!({"tap": "enabled"})),
        ]);
        assert!(!plugged.still_holds(&before, &tap));
    }

    #[test]
    fn devices_without_identifier_or_libinput_are_tolerated() {
        let new = state(&[
            json!({"name": "no identifier", "type": "keyboard"}),
            device("0:0:wlr_virtual_keyboard_v1", "keyboard", Value::Null),
        ]);
        assert!(SwayState::default().changes(&new).is_empty());
    }
}
