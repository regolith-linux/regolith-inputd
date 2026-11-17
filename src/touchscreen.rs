//! Maps touchscreens to the monitor GNOME's touchscreen settings name.
//!
//! Each touchscreen has its own settings, at a path made of its vendor and
//! product ids, so devices with the same ids share them. The `output` key
//! names a monitor by make, model and serial, optionally followed by its
//! connector to tell identical monitors apart; fewer than three values, or
//! three empty ones, leave the device to sway's automatic mapping.
//!
//! GNOME names a monitor from its EDID differently from sway: the make is
//! the three-letter manufacturer code (`DEL`) where sway prints the name
//! hwdata has for it (`Dell Inc.`), and generated models and serials are in
//! lowercase hexadecimal. A value matches a monitor under either set of
//! names, so the value the X11 session uses works here too.

use crate::utils;
use gio::{prelude::*, Settings, SettingsSchemaSource};
use glib::SignalHandlerId;
use log::{info, warn};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

const TOUCHSCREEN_SCHEMA: &str = "org.gnome.desktop.peripherals.touchscreen";
const TOUCHSCREENS_PATH: &str = "/org/gnome/desktop/peripherals/touchscreens/";

/// Where the kernel lists display connectors, as `card<N>-<connector>`.
const DRM_CLASS: &str = "/sys/class/drm";

/// The settings path of touchscreens with these (decimal) ids: both as four
/// lowercase hexadecimal digits, as GNOME writes them.
fn settings_path(vendor: i64, product: i64) -> String {
    format!("{TOUCHSCREENS_PATH}{vendor:04x}:{product:04x}/")
}

/// The manufacturer code of an EDID: three letters packed in bytes 8 and 9,
/// decoded as GNOME does.
fn edid_manufacturer(edid: &[u8]) -> Option<String> {
    const HEADER: [u8; 8] = [0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00];
    if edid.len() < 128 || edid[..8] != HEADER {
        return None;
    }
    let code = u16::from_be_bytes([edid[8], edid[9]]);
    Some(
        [10, 5, 0]
            .iter()
            .map(|shift| char::from(b'@' + ((code >> shift) & 0x1f) as u8))
            .collect(),
    )
}

/// The manufacturer code of the monitor on `connector`, from the EDID the
/// kernel read from it, or `None` if there is no single such connector with
/// an EDID (outputs of sway's headless, X11 and Wayland backends have none).
fn connector_manufacturer(drm_class: &Path, connector: &str) -> Option<String> {
    let mut connectors = fs::read_dir(drm_class).ok()?.flatten().filter(|entry| {
        entry
            .file_name()
            .to_str()
            .and_then(|name| name.split_once('-'))
            .is_some_and(|(card, name)| card.starts_with("card") && name == connector)
    });
    let (Some(found), None) = (connectors.next(), connectors.next()) else {
        return None;
    };
    edid_manufacturer(&fs::read(found.path().join("edid")).ok()?)
}

/// A generated model (`hex_digits` 4) or serial (8), as sway writes it
/// (`0x1A2B`), in GNOME's lowercase; any other value unchanged.
fn gnome_hex(value: &str, hex_digits: usize) -> String {
    match value.strip_prefix("0x") {
        Some(hex) if hex.len() == hex_digits && hex.bytes().all(|b| b.is_ascii_hexdigit()) => {
            value.to_ascii_lowercase()
        }
        _ => value.to_string(),
    }
}

/// GNOME's make, model and serial for a monitor sway reports with `make`,
/// `model` and `serial`, given the manufacturer code from its EDID.
fn gnome_names(make: &str, model: &str, serial: &str, manufacturer: Option<String>) -> [String; 3] {
    const UNKNOWN: &str = "Unknown";
    if make == UNKNOWN {
        // No EDID: sway and GNOME both name nothing, in their own words.
        let name = |value: &str| match value {
            UNKNOWN => "unknown".to_string(),
            value => value.to_string(),
        };
        return [name(make), name(model), name(serial)];
    }
    [
        manufacturer.unwrap_or_else(|| make.to_string()),
        gnome_hex(model, 4),
        // sway has no serial for an EDID without a serial string whose
        // serial number is 0; GNOME prints that number.
        match serial {
            UNKNOWN => "0x00000000".to_string(),
            serial => gnome_hex(serial, 8),
        },
    ]
}

/// An active monitor as GET_OUTPUTS reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Monitor {
    /// The connector.
    name: String,
    /// Make, model and serial, as sway names them.
    sway_names: [String; 3],
    /// The same, as GNOME names them.
    gnome_names: [String; 3],
}

impl Monitor {
    /// `None` for an output that is turned off: sway can't map to it.
    fn from_json(output: &Value, drm_class: &Path) -> Option<Monitor> {
        if output["active"] != true {
            return None;
        }
        let name = utils::str_field(output, "name");
        let [make, model, serial] =
            ["make", "model", "serial"].map(|f| utils::str_field(output, f));
        Some(Monitor {
            name: name.to_string(),
            sway_names: [make, model, serial].map(String::from),
            gnome_names: gnome_names(make, model, serial, connector_manufacturer(drm_class, name)),
        })
    }

    /// Whether make, model and serial are `names`, as sway or GNOME name
    /// them.
    fn is(&self, names: &[String]) -> bool {
        self.sway_names == names || self.gnome_names == names
    }
}

/// Whether an `output` value leaves the mapping to sway.
fn is_automatic(output: &[String]) -> bool {
    output.len() < 3 || output[..3].iter().all(String::is_empty)
}

/// The monitor a (non-automatic) `output` value chooses among the active
/// `monitors`, in sway's order: the one whose make, model and serial are the
/// first three values. If several are, a fourth value picks the one with
/// that connector, and none if no connector matches; without a fourth value
/// the first of them wins.
fn chosen_output<'a>(output: &[String], monitors: &'a [Monitor]) -> Option<&'a Monitor> {
    let matching: Vec<&Monitor> = monitors.iter().filter(|m| m.is(&output[..3])).collect();
    match (matching.as_slice(), output.get(3)) {
        ([only], _) => Some(only),
        (several, Some(connector)) => several.iter().copied().find(|m| &m.name == connector),
        (several, None) => several.first().copied(),
    }
}

/// The command mapping a touchscreen to `output`, or to the whole desktop.
fn map_command(identifier: &str, output: Option<&Monitor>) -> String {
    match output {
        Some(monitor) => format!("input \"{identifier}\" map_to_output \"{}\"", monitor.name),
        None => format!("input \"{identifier}\" map_to_output *"),
    }
}

/// A touchscreen sway reports.
struct Touchscreen {
    identifier: String,
    path: String,
}

impl Touchscreen {
    fn from_json(device: &Value) -> Option<Touchscreen> {
        if utils::str_field(device, "type") != "touch" {
            return None;
        }
        Some(Touchscreen {
            identifier: utils::str_field(device, "identifier").to_string(),
            path: settings_path(device["vendor"].as_i64()?, device["product"].as_i64()?),
        })
    }
}

fn touchscreens() -> Result<Vec<Touchscreen>, Box<dyn Error>> {
    Ok(utils::get_inputs_json()?
        .iter()
        .filter_map(Touchscreen::from_json)
        .collect())
}

struct Watched {
    settings: Settings,
    changed_handler: Option<SignalHandlerId>,
}

impl Drop for Watched {
    fn drop(&mut self) {
        if let Some(handler) = self.changed_handler.take() {
            self.settings.disconnect(handler);
        }
    }
}

struct TouchscreenState {
    /// The settings of each touchscreen path seen, watched for changes.
    watched: BTreeMap<String, Watched>,
    /// Touchscreens this daemon has mapped since sway last loaded its config.
    mapped: BTreeSet<String>,
}

pub struct TouchscreenHandler {
    state: Mutex<TouchscreenState>,
    /// This handler, for the settings callbacks: they end with it.
    this: Weak<TouchscreenHandler>,
}

impl TouchscreenHandler {
    /// `None` where the installed schemas have no touchscreen settings.
    pub fn new() -> Option<Arc<TouchscreenHandler>> {
        SettingsSchemaSource::default()?.lookup(TOUCHSCREEN_SCHEMA, true)?;
        Some(Arc::new_cyclic(|this| TouchscreenHandler {
            state: Mutex::new(TouchscreenState {
                watched: BTreeMap::new(),
                mapped: BTreeSet::new(),
            }),
            this: this.clone(),
        }))
    }

    fn lock_state(&self) -> MutexGuard<'_, TouchscreenState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The `output` value at `path`, watching the path from now on.
    fn output_setting(&self, state: &mut TouchscreenState, path: &str) -> Vec<String> {
        let watched = state.watched.entry(path.to_string()).or_insert_with(|| {
            let settings = Settings::with_path(TOUCHSCREEN_SCHEMA, path);
            let handler = self.this.clone();
            let changed_path = path.to_string();
            let changed_handler = settings.connect_changed(Some("output"), move |_, _| {
                let Some(handler) = handler.upgrade() else {
                    return;
                };
                if let Err(e) = handler.setting_changed(&changed_path) {
                    warn!("Failed to map touchscreens to their output: {e}");
                }
            });
            Watched {
                settings,
                changed_handler: Some(changed_handler),
            }
        });
        watched
            .settings
            .strv("output")
            .into_iter()
            .map(String::from)
            .collect()
    }

    /// Map `devices` as their settings say. An automatic setting leaves a
    /// device alone, unless `setting_changed` and the daemon mapped it
    /// before: then it goes back to the whole desktop, until sway's own
    /// mapping returns with its next config load.
    fn apply(&self, devices: &[Touchscreen], setting_changed: bool) -> Result<(), Box<dyn Error>> {
        let mut state = self.lock_state();
        let mut monitors: Option<Vec<Monitor>> = None;
        let mut commands = Vec::new();
        for device in devices {
            let output = self.output_setting(&mut state, &device.path);
            let command = if is_automatic(&output) {
                if !(setting_changed && state.mapped.contains(&device.identifier)) {
                    continue;
                }
                map_command(&device.identifier, None)
            } else {
                if monitors.is_none() {
                    let outputs = utils::get_outputs_json()?;
                    let drm_class = Path::new(DRM_CLASS);
                    monitors = Some(
                        outputs
                            .iter()
                            .filter_map(|o| Monitor::from_json(o, drm_class))
                            .collect(),
                    );
                }
                let chosen = chosen_output(&output, monitors.as_deref().unwrap_or_default());
                map_command(&device.identifier, chosen)
            };
            state.mapped.insert(device.identifier.clone());
            commands.push(command);
        }
        drop(state);
        utils::run_commands(&commands)?;
        Ok(())
    }

    /// Map every touchscreen, at startup.
    pub fn apply_all(&self) -> Result<(), Box<dyn Error>> {
        self.apply(&touchscreens()?, false)
    }

    /// sway loaded its config again, which dropped the daemon's mappings.
    pub fn reload(&self) -> Result<(), Box<dyn Error>> {
        self.lock_state().mapped.clear();
        self.apply_all()
    }

    /// Outputs were connected, disconnected or changed.
    pub fn outputs_changed(&self) -> Result<(), Box<dyn Error>> {
        self.apply_all()
    }

    /// A device appeared: map it if it is a touchscreen.
    pub fn device_added(&self, device: &Value) -> Result<(), Box<dyn Error>> {
        match Touchscreen::from_json(device) {
            Some(touchscreen) => self.apply(&[touchscreen], false),
            None => Ok(()),
        }
    }

    /// The setting at `path` changed: remap the touchscreens it governs.
    fn setting_changed(&self, path: &str) -> Result<(), Box<dyn Error>> {
        info!("{TOUCHSCREEN_SCHEMA}:{path} -> Key: output changed");
        let devices: Vec<Touchscreen> = touchscreens()?
            .into_iter()
            .filter(|d| d.path == path)
            .collect();
        self.apply(&devices, true)
    }
}

// The GSettings objects are only read, and only under the state lock.
unsafe impl Send for TouchscreenHandler {}
unsafe impl Sync for TouchscreenHandler {}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    fn monitor(name: &str, make: &str, model: &str, serial: &str) -> Monitor {
        Monitor {
            name: name.into(),
            sway_names: [make, model, serial].map(String::from),
            gnome_names: gnome_names(make, model, serial, None),
        }
    }

    fn setting(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| v.to_string()).collect()
    }

    fn monitors() -> Vec<Monitor> {
        vec![
            monitor("eDP-1", "BOE", "NV140", "B1"),
            monitor("DP-1", "Twin", "T", "S"),
            monitor("DP-2", "Twin", "T", "S"),
        ]
    }

    fn chosen(values: &[&str]) -> Option<String> {
        chosen_output(&setting(values), &monitors()).map(|m| m.name.clone())
    }

    /// An EDID block with `manufacturer` packed as EDIDs do.
    fn edid(manufacturer: &str) -> Vec<u8> {
        let mut edid = vec![0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00];
        let code = manufacturer
            .bytes()
            .fold(0_u16, |code, letter| code << 5 | u16::from(letter - b'@'));
        edid.extend_from_slice(&code.to_be_bytes());
        edid.resize(256, 0);
        edid
    }

    /// A directory laid out like /sys/class/drm, with an `edid` file for
    /// each of `connectors` (`card<N>-<connector>`).
    fn drm_class(test: &str, connectors: &[(&str, &[u8])]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("inputd-drm-{}-{test}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("card1")).unwrap();
        for (connector, edid) in connectors {
            fs::create_dir_all(dir.join(connector)).unwrap();
            fs::write(dir.join(connector).join("edid"), edid).unwrap();
        }
        dir
    }

    #[test]
    fn path_uses_four_lowercase_hex_digits() {
        assert_eq!(
            settings_path(255, 16),
            "/org/gnome/desktop/peripherals/touchscreens/00ff:0010/"
        );
        assert_eq!(
            settings_path(43981, 61185),
            "/org/gnome/desktop/peripherals/touchscreens/abcd:ef01/"
        );
    }

    #[test]
    fn short_or_empty_values_are_automatic() {
        assert!(is_automatic(&setting(&[])));
        assert!(is_automatic(&setting(&["BOE", "NV140"])));
        assert!(is_automatic(&setting(&["", "", ""])));
        assert!(is_automatic(&setting(&["", "", "", "DP-2"])));
        assert!(!is_automatic(&setting(&["", "NV140", ""])));
    }

    #[test]
    fn make_model_and_serial_choose_the_monitor() {
        assert_eq!(chosen(&["BOE", "NV140", "B1"]).as_deref(), Some("eDP-1"));
        assert_eq!(
            chosen(&["BOE", "NV140", "B1", "HDMI-A-1"]).as_deref(),
            Some("eDP-1")
        );
        assert_eq!(chosen(&["boe", "NV140", "B1"]), None);
        assert_eq!(chosen(&["NV140", "BOE", "B1"]), None);
    }

    #[test]
    fn identical_monitors_are_told_apart_by_connector_or_order() {
        assert_eq!(chosen(&["Twin", "T", "S"]).as_deref(), Some("DP-1"));
        assert_eq!(chosen(&["Twin", "T", "S", "DP-2"]).as_deref(), Some("DP-2"));
        assert_eq!(chosen(&["Twin", "T", "S", "eDP-1"]), None);
        assert_eq!(chosen(&["Twin", "T", "S", ""]), None);
    }

    #[test]
    fn edid_manufacturer_is_the_three_letter_code() {
        // A Dell EDID starts 00 ff ff ff ff ff ff 00 10 ac.
        let mut dell = edid("DEL");
        assert_eq!(dell[8..10], [0x10, 0xac]);
        assert_eq!(edid_manufacturer(&dell).as_deref(), Some("DEL"));
        assert_eq!(edid_manufacturer(&edid("BOE")).as_deref(), Some("BOE"));
        assert_eq!(edid_manufacturer(&dell[..127]), None);
        dell[0] = 0xff;
        assert_eq!(edid_manufacturer(&dell), None);
        assert_eq!(edid_manufacturer(&[]), None);
    }

    #[test]
    fn connector_manufacturer_reads_the_connectors_edid() {
        let dir = drm_class(
            "connectors",
            &[
                ("card1-eDP-1", &edid("BOE")),
                ("card1-DP-1", &edid("DEL")),
                ("card1-HDMI-A-1", &[]),
                ("card0-DP-2", &edid("GSM")),
                ("card1-DP-2", &edid("SAM")),
            ],
        );
        assert_eq!(
            connector_manufacturer(&dir, "eDP-1").as_deref(),
            Some("BOE")
        );
        assert_eq!(connector_manufacturer(&dir, "DP-1").as_deref(), Some("DEL"));
        // Nothing connected: the kernel's edid file is empty.
        assert_eq!(connector_manufacturer(&dir, "HDMI-A-1"), None);
        // The same connector name on two cards: which is sway's is unknown.
        assert_eq!(connector_manufacturer(&dir, "DP-2"), None);
        assert_eq!(connector_manufacturer(&dir, "HEADLESS-1"), None);
        assert_eq!(connector_manufacturer(&dir.join("missing"), "DP-1"), None);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn gnome_names_follow_gnomes_edid_parsing() {
        let names = |make, model, serial, code: Option<&str>| {
            gnome_names(make, model, serial, code.map(String::from))
        };
        assert_eq!(
            names("Dell Inc.", "DELL U2720Q", "8LXMZ13", Some("DEL")),
            ["DEL", "DELL U2720Q", "8LXMZ13"]
        );
        // Generated from the product code and serial number.
        assert_eq!(
            names("BOE", "0x0A1F", "0x0001E2F0", Some("BOE")),
            ["BOE", "0x0a1f", "0x0001e2f0"]
        );
        assert_eq!(
            names("BOE", "0x0A1F", "Unknown", Some("BOE")),
            ["BOE", "0x0a1f", "0x00000000"]
        );
        // Strings from the EDID that merely look generated are kept.
        assert_eq!(
            names("Dell Inc.", "0xABC", "0xZZZZZZZZ", None),
            ["Dell Inc.", "0xABC", "0xZZZZZZZZ"]
        );
        assert_eq!(
            names("Unknown", "Unknown", "Unknown", None),
            ["unknown", "unknown", "unknown"]
        );
    }

    #[test]
    fn a_value_written_in_either_session_chooses_the_monitor() {
        let dir = drm_class(
            "outputs",
            &[("card1-eDP-1", &edid("BOE")), ("card1-DP-1", &edid("DEL"))],
        );
        let outputs = [
            json!({"name": "eDP-1", "active": true, "make": "BOE", "model": "0x0A1F",
                   "serial": "Unknown"}),
            json!({"name": "DP-1", "active": true, "make": "Dell Inc.",
                   "model": "DELL U2720Q", "serial": "8LXMZ13"}),
            json!({"name": "DP-2", "active": false, "make": "Goldstar Company Ltd",
                   "model": "LG HDR 4K", "serial": "0x0001E2F0"}),
        ];
        let monitors: Vec<Monitor> = outputs
            .iter()
            .filter_map(|o| Monitor::from_json(o, &dir))
            .collect();
        let chosen =
            |values: &[&str]| chosen_output(&setting(values), &monitors).map(|m| m.name.clone());
        // As GNOME writes it, in the X11 session, and as sway prints it.
        assert_eq!(
            chosen(&["DEL", "DELL U2720Q", "8LXMZ13"]).as_deref(),
            Some("DP-1")
        );
        assert_eq!(
            chosen(&["Dell Inc.", "DELL U2720Q", "8LXMZ13"]).as_deref(),
            Some("DP-1")
        );
        assert_eq!(
            chosen(&["BOE", "0x0a1f", "0x00000000"]).as_deref(),
            Some("eDP-1")
        );
        assert_eq!(
            chosen(&["BOE", "0x0A1F", "Unknown"]).as_deref(),
            Some("eDP-1")
        );
        // Names from both sets don't add up to a monitor.
        assert_eq!(chosen(&["DEL", "DELL U2720Q", "8lxmz13"]), None);
        assert_eq!(chosen(&["BOE", "0x0a1f", "Unknown"]), None);
        // A monitor that is turned off is not a mapping target.
        assert_eq!(
            chosen(&["Goldstar Company Ltd", "LG HDR 4K", "0x0001E2F0"]),
            None
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn mapping_commands() {
        let edp = monitor("eDP-1", "BOE", "NV140", "B1");
        assert_eq!(
            map_command("1:2:Touch", Some(&edp)),
            "input \"1:2:Touch\" map_to_output \"eDP-1\""
        );
        assert_eq!(
            map_command("1:2:Touch", None),
            "input \"1:2:Touch\" map_to_output *"
        );
    }
}
