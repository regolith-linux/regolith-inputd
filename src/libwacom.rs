//! Tablet facts from the libwacom database: a tablet's physical size and
//! whether it is built into a display or a computer.
//!
//! Only a few keys of each `.tablet` file are needed, so the data files
//! (GKeyFiles) are read directly instead of linking libwacom.

use glib::{KeyFile, KeyFileFlags};
use log::{debug, warn};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Where libwacom looks for tablet descriptions, in order of precedence.
const DATA_DIRS: [&str; 2] = ["/etc/libwacom", "/usr/share/libwacom"];

/// What the daemon needs to know about a tablet model.
#[derive(Debug, Clone, PartialEq)]
pub struct TabletInfo {
    pub name: String,
    /// Width and height of the active area in inches, when known.
    pub size: Option<(f64, f64)>,
    /// Built into a display, like a Cintiq.
    pub integrated_in_display: bool,
    /// Built into the computer, like a laptop's or tablet PC's screen.
    pub integrated_in_system: bool,
}

impl TabletInfo {
    /// Whether the pen works on a screen. GNOME applies calibration only to
    /// these tablets, and left-handed mode only to the others.
    pub fn is_screen_tablet(&self) -> bool {
        self.integrated_in_display || self.integrated_in_system
    }

    /// Width over height of the active area.
    pub fn aspect_ratio(&self) -> Option<f64> {
        self.size.map(|(width, height)| width / height)
    }
}

/// One `DeviceMatch` entry, `bus|vendor|product[|name[|uniq]]`.
#[derive(Debug, PartialEq)]
struct DeviceMatch {
    vendor: u16,
    product: u16,
    name: Option<String>,
}

/// Parses a `DeviceMatch` value. Entries that need the device's firmware id
/// (`uniq`) are dropped: sway does not report it, so they can't be checked.
fn parse_device_matches(value: &str) -> Vec<DeviceMatch> {
    value
        .split(';')
        .filter_map(|entry| {
            let fields: Vec<&str> = entry.trim().split('|').collect();
            if fields.len() < 3 || fields.get(4).is_some_and(|uniq| !uniq.is_empty()) {
                return None;
            }
            Some(DeviceMatch {
                vendor: u16::from_str_radix(fields[1], 16).ok()?,
                product: u16::from_str_radix(fields[2], 16).ok()?,
                name: fields
                    .get(3)
                    .filter(|name| !name.is_empty())
                    .map(|name| name.to_string()),
            })
        })
        .collect()
}

/// Parses a size in inches, ignoring a trailing comment.
fn parse_inches(value: &str) -> Option<f64> {
    let value = value.split('#').next()?.trim();
    value
        .parse::<f64>()
        .ok()
        .filter(|inches| inches.is_finite() && *inches > 0.0)
}

/// Reads the `[Device]` group of a `.tablet` file.
fn parse_tablet(keyfile: &KeyFile) -> Option<(Vec<DeviceMatch>, TabletInfo)> {
    let string = |key| keyfile.string("Device", key).ok().map(String::from);
    let matches = parse_device_matches(&string("DeviceMatch")?);
    let integrated_in = string("IntegratedIn").unwrap_or_default();
    let integrated_in = |place| integrated_in.split(';').any(|p| p.trim() == place);
    let width = string("Width").as_deref().and_then(parse_inches);
    let height = string("Height").as_deref().and_then(parse_inches);
    let info = TabletInfo {
        name: string("Name").unwrap_or_default(),
        size: width.zip(height),
        integrated_in_display: integrated_in("Display"),
        integrated_in_system: integrated_in("System"),
    };
    Some((matches, info))
}

/// A tablet description, for one device name or for any.
#[derive(Debug)]
struct Entry {
    name: Option<String>,
    info: TabletInfo,
}

/// The tablet descriptions libwacom knows, by USB vendor and product id.
#[derive(Debug, Default)]
pub struct TabletDatabase {
    tablets: HashMap<(u16, u16), Vec<Entry>>,
}

impl TabletDatabase {
    /// Reads the system's libwacom database.
    pub fn load() -> TabletDatabase {
        let mut database = TabletDatabase::default();
        for dir in DATA_DIRS {
            database.load_dir(Path::new(dir));
        }
        debug!(
            "Read {} tablet ids from the libwacom database",
            database.tablets.len()
        );
        database
    }

    fn load_dir(&mut self, dir: &Path) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        let mut files: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "tablet"))
            .collect();
        files.sort();
        for file in files {
            let keyfile = KeyFile::new();
            match keyfile.load_from_file(&file, KeyFileFlags::NONE) {
                Ok(()) => {
                    if let Some((matches, info)) = parse_tablet(&keyfile) {
                        self.add(matches, info);
                    }
                }
                Err(e) => warn!("Skipping {}: {e}", file.display()),
            }
        }
    }

    fn add(&mut self, matches: Vec<DeviceMatch>, info: TabletInfo) {
        for m in matches {
            self.tablets
                .entry((m.vendor, m.product))
                .or_default()
                .push(Entry {
                    name: m.name,
                    info: info.clone(),
                });
        }
    }

    /// The description of a device. As in libwacom, an entry for the
    /// device's name wins over one for any device with its ids, and earlier
    /// data directories win over later ones.
    pub fn lookup(&self, vendor: u16, product: u16, name: &str) -> Option<&TabletInfo> {
        let entries = self.tablets.get(&(vendor, product))?;
        entries
            .iter()
            .find(|entry| entry.name.as_deref() == Some(name))
            .or_else(|| entries.iter().find(|entry| entry.name.is_none()))
            .map(|entry| &entry.info)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn database(files: &[&str]) -> TabletDatabase {
        let mut database = TabletDatabase::default();
        for data in files {
            let keyfile = KeyFile::new();
            keyfile.load_from_data(data, KeyFileFlags::NONE).unwrap();
            let (matches, info) = parse_tablet(&keyfile).unwrap();
            database.add(matches, info);
        }
        database
    }

    const INTUOS: &str = "[Device]\nName=Wacom Intuos Pro M\n\
        DeviceMatch=usb|056a|0357;bluetooth|056a|0360;\n\
        Width=9\nHeight=6\nIntegratedIn=\n";
    const CINTIQ: &str = "[Device]\nName=Wacom Cintiq 13HD\nDeviceMatch=usb|056a|0304\n\
        Width=12\nHeight=7\nIntegratedIn=Display\n";
    const LAPTOP: &str = "# Panasonic CF-20-2\n[Device]\nName=Wacom ISDv4 0148\n\
        DeviceMatch=usb|056a|0148\nWidth=9\nHeight=5\nIntegratedIn=Display;System\n";
    const HUION: &str = "[Device]\nName=Huion H640P\n\
        DeviceMatch=usb|256c|006d|HUION Huion Tablet_H640P Pad;usb|256c|006d|HUION Huion Tablet_H640P Pen\n\
        Width=6\nHeight=4\n";
    const HUION_FIRMWARE: &str = "[Device]\nName=Huion Other\n\
        DeviceMatch=usb|256c|006d||HUION_T167;usb|256c|006d|HUION Huion Tablet Pen\n\
        Width=10\nHeight=6\n";

    #[test]
    fn parses_device_matches() {
        assert_eq!(
            parse_device_matches("usb|056a|0357;bluetooth|056a|0360;"),
            [
                DeviceMatch {
                    vendor: 0x056a,
                    product: 0x0357,
                    name: None
                },
                DeviceMatch {
                    vendor: 0x056a,
                    product: 0x0360,
                    name: None
                },
            ]
        );
        assert_eq!(
            parse_device_matches("usb|256c|006d|HUION Pen;usb|256c|006d||HUION_T167;generic"),
            [DeviceMatch {
                vendor: 0x256c,
                product: 0x006d,
                name: Some("HUION Pen".to_string())
            }]
        );
    }

    #[test]
    fn parses_sizes() {
        assert_eq!(parse_inches("9"), Some(9.0));
        assert_eq!(parse_inches(" 8.5 # measured"), Some(8.5));
        assert_eq!(parse_inches(""), None);
        assert_eq!(parse_inches("0"), None);
    }

    #[test]
    fn looks_up_external_and_screen_tablets() {
        let database = database(&[INTUOS, CINTIQ, LAPTOP]);
        let intuos = database
            .lookup(0x056a, 0x0357, "Wacom Intuos Pro M Pen")
            .unwrap();
        assert_eq!(intuos.size, Some((9.0, 6.0)));
        assert_eq!(intuos.aspect_ratio(), Some(1.5));
        assert!(!intuos.is_screen_tablet());
        let cintiq = database
            .lookup(0x056a, 0x0304, "Wacom Cintiq 13HD Pad")
            .unwrap();
        assert!(cintiq.integrated_in_display && !cintiq.integrated_in_system);
        assert!(cintiq.is_screen_tablet());
        let laptop = database
            .lookup(0x056a, 0x0148, "Wacom ISDv4 148 Pen")
            .unwrap();
        assert!(laptop.integrated_in_display && laptop.integrated_in_system);
        assert_eq!(database.lookup(0x056a, 0x0001, "Unknown"), None);
    }

    #[test]
    fn named_matches_need_the_device_name() {
        let database = database(&[HUION_FIRMWARE, HUION]);
        let pen = database
            .lookup(0x256c, 0x006d, "HUION Huion Tablet_H640P Pen")
            .unwrap();
        assert_eq!(pen.name, "Huion H640P");
        assert_eq!(pen.size, Some((6.0, 4.0)));
        assert_eq!(
            database
                .lookup(0x256c, 0x006d, "HUION Huion Tablet Pen")
                .map(|info| info.name.as_str()),
            Some("Huion Other")
        );
        assert_eq!(database.lookup(0x256c, 0x006d, "HUION Unknown Pen"), None);
    }
}
