//! Applies GNOME's graphics tablet settings to sway.
//!
//! GNOME keeps the settings of each tablet under the relocatable schema
//! `org.gnome.desktop.peripherals.tablet`, at
//! `/org/gnome/desktop/peripherals/tablets/VVVV:PPPP/`, keyed by the tablet's
//! USB ids in lowercase hex. Sway reports the same tablet as a `tablet_tool`
//! input (the pen and its other tools) and a `tablet_pad` input, with the ids
//! in decimal. Each key becomes a sway input option, following mutter's
//! `meta-input-settings.c` and `meta-input-mapper.c`:
//!
//! - `mapping`: `tool_mode * absolute|relative`, on tools.
//! - `output`: `map_to_output <connector>|*`, on tools.
//! - `keep-aspect` and `area`: `map_from_region`, on tools. `area` holds
//!   calibration margins and only applies to screen tablets.
//! - `left-handed`: `left_handed`, on the tools and pad of external tablets.
//!
//! Sway has nothing for the stylus and pad-button settings, so they are left
//! alone. Whether a tablet is a screen tablet, and its size, come from the
//! libwacom database.
//!
//! Unlike the other handlers there is no single settings object: the settings
//! of each tablet are watched from the moment sway first reports it. The
//! handler never writes to GSettings and ignores the `libinput_config` input
//! events its own commands cause, so it needs none of the feedback guards the
//! other handlers use.

use crate::libwacom::{TabletDatabase, TabletInfo};
use crate::utils;
use gio::{prelude::*, Settings, SettingsSchemaSource};
use glib::SignalHandlerId;
use log::{debug, info, warn};
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use swayipc::{Connection as SwayConnection, Input, InputChange, InputEvent, Output};

const TABLET_SCHEMA: &str = "org.gnome.desktop.peripherals.tablet";
const TABLET_TOOL: &str = "tablet_tool";
const TABLET_PAD: &str = "tablet_pad";

/// Whether an input is part of a graphics tablet.
pub fn is_tablet(input: &Input) -> bool {
    matches!(input.input_type.as_str(), TABLET_TOOL | TABLET_PAD)
}

/// A tablet's USB vendor and product id, which key its GNOME settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct TabletId {
    vendor: u16,
    product: u16,
}

impl TabletId {
    fn of(input: &Input) -> Option<TabletId> {
        if !is_tablet(input) {
            return None;
        }
        Some(TabletId {
            vendor: u16::try_from(input.vendor).ok()?,
            product: u16::try_from(input.product).ok()?,
        })
    }

    fn settings_path(self) -> String {
        format!("/org/gnome/desktop/peripherals/tablets/{self}/")
    }
}

impl fmt::Display for TabletId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04x}:{:04x}", self.vendor, self.product)
    }
}

/// GNOME's `mapping` key: Tablet Mode on (absolute) or off (relative).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ToolMode {
    Absolute,
    Relative,
}

impl ToolMode {
    fn as_sway(self) -> &'static str {
        match self {
            ToolMode::Absolute => "absolute",
            ToolMode::Relative => "relative",
        }
    }
}

/// GNOME's `output` key: the monitor a tablet is mapped to.
#[derive(Debug, Clone, PartialEq, Eq)]
enum MonitorSetting {
    Automatic,
    /// The monitor's vendor, product and serial, as the display settings
    /// report them (sway's make, model and serial), and optionally its
    /// connector, which only tells identical monitors apart.
    Monitor {
        vendor: String,
        product: String,
        serial: String,
        connector: Option<String>,
    },
}

impl MonitorSetting {
    fn parse(values: &[String]) -> MonitorSetting {
        match values {
            [vendor, product, serial, rest @ ..]
                if !(vendor.is_empty() && product.is_empty() && serial.is_empty()) =>
            {
                MonitorSetting::Monitor {
                    vendor: vendor.clone(),
                    product: product.clone(),
                    serial: serial.clone(),
                    connector: rest.first().filter(|c| !c.is_empty()).cloned(),
                }
            }
            _ => MonitorSetting::Automatic,
        }
    }
}

/// The settings GNOME stores for one tablet.
#[derive(Debug, Clone, PartialEq)]
struct TabletSettings {
    tool_mode: ToolMode,
    monitor: MonitorSetting,
    keep_aspect: bool,
    /// Calibration margins: left, right, top and bottom.
    area: Vec<f64>,
    left_handed: bool,
}

impl TabletSettings {
    fn read(settings: &Settings) -> TabletSettings {
        let output: Vec<String> = settings.get("output");
        TabletSettings {
            tool_mode: match settings.string("mapping").as_str() {
                "relative" => ToolMode::Relative,
                _ => ToolMode::Absolute,
            },
            monitor: MonitorSetting::parse(&output),
            keep_aspect: settings.boolean("keep-aspect"),
            area: settings.get("area"),
            left_handed: settings.boolean("left-handed"),
        }
    }
}

/// The part of the tablet surface in use, in fractions of its width and
/// height: sway's `map_from_region`.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Region {
    x1: f64,
    y1: f64,
    x2: f64,
    y2: f64,
}

impl Region {
    const FULL: Region = Region {
        x1: 0.0,
        y1: 0.0,
        x2: 1.0,
        y2: 1.0,
    };

    /// The region left inside GNOME's calibration margins
    /// `[left, right, top, bottom]`, or `None` if they leave nothing.
    fn from_margins(margins: &[f64]) -> Option<Region> {
        let &[left, right, top, bottom] = margins else {
            return None;
        };
        let valid = |margin: f64| (0.0..1.0).contains(&margin);
        if ![left, right, top, bottom].into_iter().all(valid)
            || left + right >= 1.0
            || top + bottom >= 1.0
        {
            return None;
        }
        Some(Region {
            x1: left,
            y1: top,
            x2: 1.0 - right,
            y2: 1.0 - bottom,
        })
    }

    /// Uses only as much of this region as gives a tablet of
    /// `tablet_aspect` (width over height) the aspect of the area it maps
    /// to. As in mutter, the crop keeps the top left corner and applies
    /// within the calibrated region.
    fn crop_to_aspect(self, tablet_aspect: f64, target_aspect: f64) -> Region {
        let (width, height) = if tablet_aspect > target_aspect {
            (target_aspect / tablet_aspect, 1.0)
        } else {
            (1.0, tablet_aspect / target_aspect)
        };
        Region {
            x2: self.x1 + (self.x2 - self.x1) * width,
            y2: self.y1 + (self.y2 - self.y1) * height,
            ..self
        }
    }
}

impl fmt::Display for Region {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let fraction = |value: f64| {
            let text = format!("{value:.4}");
            text.trim_end_matches('0').trim_end_matches('.').to_string()
        };
        write!(
            f,
            "{}x{} {}x{}",
            fraction(self.x1),
            fraction(self.y1),
            fraction(self.x2),
            fraction(self.y2)
        )
    }
}

/// Where the movement of a tablet's tools goes.
#[derive(Debug)]
enum Mapping<'a> {
    Output(&'a Output),
    Desktop,
    /// Left to sway's own automatic mapping.
    SwayDefault,
}

fn active_outputs(outputs: &[Output]) -> impl Iterator<Item = &Output> {
    outputs
        .iter()
        .filter(|o| o.active && o.rect.width > 0 && o.rect.height > 0)
}

/// The monitor GNOME would pick for a configured vendor, product and
/// serial: the connector only decides between identical monitors.
fn find_monitor<'a>(
    outputs: &'a [Output],
    vendor: &str,
    product: &str,
    serial: &str,
    connector: Option<&str>,
) -> Option<&'a Output> {
    let matches: Vec<&Output> = active_outputs(outputs)
        .filter(|o| o.make == vendor && o.model == product && o.serial == serial)
        .collect();
    match (matches.as_slice(), connector) {
        ([only], _) => Some(only),
        (twins, Some(connector)) => twins.iter().copied().find(|o| o.name == connector),
        (twins, None) => twins.first().copied(),
    }
}

fn resolve_mapping<'a>(
    setting: &MonitorSetting,
    outputs: &'a [Output],
    screen_tablet: bool,
) -> Mapping<'a> {
    match setting {
        MonitorSetting::Automatic if screen_tablet => Mapping::SwayDefault,
        MonitorSetting::Automatic => Mapping::Desktop,
        MonitorSetting::Monitor {
            vendor,
            product,
            serial,
            connector,
        } => find_monitor(outputs, vendor, product, serial, connector.as_deref())
            .map_or(Mapping::Desktop, Mapping::Output),
    }
}

/// The output sway maps a tablet to on its own, as its `seat.c` does: a
/// tablet built into the computer goes to the built-in panel, if there is
/// exactly one. Other tablets cover the whole desktop.
fn sway_default_output<'a>(outputs: &'a [Output], info: Option<&TabletInfo>) -> Option<&'a Output> {
    if !info.is_some_and(|info| info.integrated_in_system) {
        return None;
    }
    let mut panels = active_outputs(outputs).filter(|o| {
        ["eDP-", "LVDS-", "DSI-"]
            .iter()
            .any(|prefix| o.name.starts_with(prefix))
    });
    match (panels.next(), panels.next()) {
        (Some(panel), None) => Some(panel),
        _ => None,
    }
}

/// Width and height of the whole desktop: the bounding box of the outputs,
/// in sway's layout coordinates (rotated and scaled).
fn desktop_size(outputs: &[Output]) -> Option<(f64, f64)> {
    let (x1, y1, x2, y2) = active_outputs(outputs).fold(None, |bounds, o| {
        let (x, y) = (o.rect.x, o.rect.y);
        let (right, bottom) = (x + o.rect.width, y + o.rect.height);
        Some(match bounds {
            None => (x, y, right, bottom),
            Some((x1, y1, x2, y2)) => (x.min(x1), y.min(y1), right.max(x2), bottom.max(y2)),
        })
    })?;
    Some(((x2 - x1) as f64, (y2 - y1) as f64))
}

fn output_size(output: &Output) -> (f64, f64) {
    (output.rect.width as f64, output.rect.height as f64)
}

/// Size of the area a tablet's tools are mapped to.
fn mapped_size(
    mapping: &Mapping,
    outputs: &[Output],
    info: Option<&TabletInfo>,
) -> Option<(f64, f64)> {
    match mapping {
        Mapping::Output(output) => Some(output_size(output)),
        Mapping::Desktop => desktop_size(outputs),
        Mapping::SwayDefault => sway_default_output(outputs, info)
            .map(output_size)
            .or_else(|| desktop_size(outputs)),
    }
}

/// The region of the tablet in use: the calibrated area of a screen tablet,
/// cropped to the aspect of the area it maps to when Keep Aspect Ratio is on.
fn active_region(
    settings: &TabletSettings,
    info: Option<&TabletInfo>,
    mapped_size: Option<(f64, f64)>,
) -> Region {
    let calibrated = if info.is_some_and(TabletInfo::is_screen_tablet) {
        Region::from_margins(&settings.area).unwrap_or_else(|| {
            warn!("Ignoring invalid tablet calibration {:?}", settings.area);
            Region::FULL
        })
    } else {
        Region::FULL
    };
    if !settings.keep_aspect {
        return calibrated;
    }
    match (info.and_then(TabletInfo::aspect_ratio), mapped_size) {
        (Some(tablet_aspect), Some((width, height))) => {
            calibrated.crop_to_aspect(tablet_aspect, width / height)
        }
        _ => {
            debug!("Tablet size or mapped area unknown, keeping the whole surface");
            calibrated
        }
    }
}

/// Writes `value` as one argument of a sway command that sway reads back as
/// exactly `value`, or returns `None` when sway can't receive it.
///
/// Over IPC, sway removes the quotes of an argument that starts with one but
/// keeps every backslash, where a backslash still escapes the character
/// after it. It then replaces `$name` with its variables in every argument
/// but the command name. So the value is written as adjacent quoted runs:
/// the parts with a `"` in single quotes, the rest in double quotes, which
/// leaves plain values as they always were. A run never ends on a backslash,
/// which would escape its closing quote, so a value that ends in an odd
/// number of backslashes can't be written. Neither can a line feed, which
/// sway turns into a `;`. Each `$` is doubled, except one right after exactly
/// one backslash, which sway leaves alone.
fn sway_argument(value: &str) -> Option<String> {
    let chars: Vec<char> = value.chars().collect();
    let trailing_backslashes = chars.iter().rev().take_while(|&&c| c == '\\').count();
    if value.contains(['\n', '\0']) || trailing_backslashes % 2 == 1 {
        return None;
    }
    // Runs as (quote, first char, end), each without its own quote char.
    let mut runs = Vec::new();
    let mut start = 0;
    let mut quote = None;
    for (i, &c) in chars.iter().enumerate() {
        let needed = match c {
            '"' => '\'',
            '\'' => '"',
            _ => continue,
        };
        match quote {
            Some(q) if q != needed => {
                // The backslashes before `c` move to the next run, where
                // they precede a literal quote rather than the closing one.
                // The run holds the quote that chose `q`, so it stays
                // non-empty.
                let mut end = i;
                while chars[end - 1] == '\\' {
                    end -= 1;
                }
                runs.push((q, start, end));
                start = end;
                quote = Some(needed);
            }
            Some(_) => {}
            None => quote = Some(needed),
        }
    }
    runs.push((quote.unwrap_or('"'), start, chars.len()));
    let mut argument = String::with_capacity(value.len() + 2 * runs.len());
    for (q, start, end) in runs {
        argument.push(q);
        for i in start..end {
            argument.push(chars[i]);
            let skipped_by_sway =
                i >= 1 && chars[i - 1] == '\\' && (i == 1 || chars[i - 2] != '\\');
            if chars[i] == '$' && !skipped_by_sway {
                argument.push('$');
            }
        }
        argument.push(q);
    }
    Some(argument)
}

/// Whether sway reads `command` as a single command. It splits a list of
/// commands at each `;` and `,` outside quotes that a backslash doesn't
/// escape, and when splitting, a backslash also escapes the quotes that
/// directly follow the character after it. Arguments written by
/// `sway_argument` can trip that up only when they contain a `;` or `,`.
fn is_one_command(command: &str) -> bool {
    let (mut in_double, mut in_single, mut escaped) = (false, false, false);
    let mut next = 0;
    for (i, c) in command.char_indices() {
        if !matches!(c, '"' | '\'' | '\\' | ';' | ',') {
            continue;
        }
        if escaped && i != next {
            escaped = false;
        }
        match c {
            '"' if !in_single && !escaped => in_double = !in_double,
            '\'' if !in_double && !escaped => in_single = !in_single,
            '\\' => escaped = !escaped,
            ';' | ',' if !in_double && !in_single && !escaped => return false,
            _ => {}
        }
        next = i + 1;
    }
    true
}

/// `input <identifier> <option>`, or `None` with a warning when sway can't
/// receive the identifier intact.
fn input_command(identifier: &str, option: &str) -> Option<String> {
    let command = sway_argument(identifier)
        .map(|id| format!("input {id} {option}"))
        .filter(|command| is_one_command(command));
    if command.is_none() {
        warn!("Not sending {option:?} to input {identifier:?}: sway can't read that identifier");
    }
    command
}

/// Which settings an apply sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Changes {
    tool_mode: bool,
    mapping: bool,
    region: bool,
    left_handed: bool,
}

impl Changes {
    const ALL: Changes = Changes {
        tool_mode: true,
        mapping: true,
        region: true,
        left_handed: true,
    };
    /// What the outputs affect.
    const OUTPUTS: Changes = Changes {
        tool_mode: false,
        mapping: true,
        region: true,
        left_handed: false,
    };
    const NONE: Changes = Changes {
        tool_mode: false,
        mapping: false,
        region: false,
        left_handed: false,
    };

    /// What a change of a key of the tablet schema affects.
    fn for_key(key: &str) -> Changes {
        match key {
            "mapping" => Changes {
                tool_mode: true,
                ..Changes::NONE
            },
            "output" => Changes::OUTPUTS,
            "keep-aspect" | "area" => Changes {
                region: true,
                ..Changes::NONE
            },
            "left-handed" => Changes {
                left_handed: true,
                ..Changes::NONE
            },
            _ => Changes::NONE,
        }
    }

    fn needs_outputs(self) -> bool {
        self.mapping || self.region
    }
}

/// The sway commands that apply a tablet's settings to one of its devices.
///
/// `was_mapped` tells whether the daemon has mapped this tool to an output
/// since sway last loaded its config: sway has no way to unset a mapping,
/// so going back to its automatic one means mapping the tool where sway
/// would. Returns the commands and whether the tool is now mapped by the
/// daemon.
fn device_commands(
    device: &Input,
    info: Option<&TabletInfo>,
    settings: &TabletSettings,
    outputs: &[Output],
    changes: Changes,
    was_mapped: bool,
) -> (Vec<String>, bool) {
    let id = device.identifier.as_str();
    let screen_tablet = info.is_some_and(TabletInfo::is_screen_tablet);
    let mut commands = Vec::new();
    let mut mapped = was_mapped;
    if sway_argument(id).is_none() {
        warn!("Skipping tablet device {id:?}: sway can't read its identifier");
        return (commands, mapped);
    }
    if device.input_type == TABLET_TOOL {
        if changes.tool_mode {
            let mode = settings.tool_mode.as_sway();
            commands.extend(input_command(id, &format!("tool_mode * {mode}")));
        }
        let mapping = resolve_mapping(&settings.monitor, outputs, screen_tablet);
        if changes.mapping {
            let target = match &mapping {
                Mapping::Output(output) => Some(output.name.as_str()),
                Mapping::Desktop => Some("*"),
                Mapping::SwayDefault if was_mapped => {
                    Some(sway_default_output(outputs, info).map_or("*", |o| o.name.as_str()))
                }
                Mapping::SwayDefault => None,
            };
            if let Some(target) = target {
                let command = match sway_argument(target) {
                    Some(output) => input_command(id, &format!("map_to_output {output}")),
                    None => {
                        warn!("Not mapping {id:?} to output {target:?}: sway can't read its name");
                        None
                    }
                };
                if let Some(command) = command {
                    commands.push(command);
                    mapped = true;
                }
            }
        }
        if changes.region {
            let size = mapped_size(&mapping, outputs, info);
            let region = active_region(settings, info, size);
            commands.extend(input_command(id, &format!("map_from_region {region}")));
        }
    }
    // Turning a screen tablet around makes no sense, so GNOME only applies
    // left-handed mode to external tablets.
    if changes.left_handed && !screen_tablet {
        let state = if settings.left_handed {
            "enabled"
        } else {
            "disabled"
        };
        commands.extend(input_command(id, &format!("left_handed {state}")));
    }
    (commands, mapped)
}

/// A tablet whose settings are watched.
struct WatchedTablet {
    settings: Settings,
    changed_handler: Option<SignalHandlerId>,
}

impl Drop for WatchedTablet {
    fn drop(&mut self) {
        if let Some(handler) = self.changed_handler.take() {
            self.settings.disconnect(handler);
        }
    }
}

struct TabletState {
    sway_connection: SwayConnection,
    /// Read when the first tablet appears.
    database: Option<TabletDatabase>,
    watched: BTreeMap<TabletId, WatchedTablet>,
    /// Tools the daemon has mapped to an output since sway last loaded its
    /// config, by identifier.
    mapped_tools: BTreeSet<String>,
}

// SAFETY: GSettings objects are thread-safe; their signal handlers run on
// the main context, and all other access goes through the handler's mutex.
unsafe impl Send for TabletState {}

/// Applies the settings of every tablet sway reports. Cheap to clone: the
/// clones share their state.
#[derive(Clone)]
pub struct TabletHandler {
    state: Arc<Mutex<TabletState>>,
}

impl TabletHandler {
    /// Returns `None` when the tablet schema is not installed.
    pub fn new() -> Option<TabletHandler> {
        // GSettings aborts the process when asked for a missing schema.
        SettingsSchemaSource::default()?.lookup(TABLET_SCHEMA, true)?;
        Some(TabletHandler {
            state: Arc::new(Mutex::new(TabletState {
                sway_connection: SwayConnection::new().unwrap(),
                database: None,
                watched: BTreeMap::new(),
                mapped_tools: BTreeSet::new(),
            })),
        })
    }

    fn lock_state(&self) -> MutexGuard<'_, TabletState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Applies every setting to every tablet, at startup and after sway
    /// reloaded its config (`reloaded`), which undid all of them.
    pub fn apply_all(&self, reloaded: bool) -> Result<(), Box<dyn Error>> {
        let mut state = self.lock_state();
        if reloaded {
            state.mapped_tools.clear();
        }
        let devices: Vec<Input> = utils::get_inputs()?.into_iter().filter(is_tablet).collect();
        for device in &devices {
            self.watch(&mut state, device);
        }
        state.apply(&devices, Changes::ALL)
    }

    /// Follows tablets being plugged in and out.
    pub fn input_changed(&self, event: &InputEvent) -> Result<(), Box<dyn Error>> {
        let mut state = self.lock_state();
        let device = &event.input;
        match event.change {
            InputChange::Added => {
                info!("Tablet device {} added", device.identifier);
                self.watch(&mut state, device);
                state.apply(std::slice::from_ref(device), Changes::ALL)
            }
            InputChange::Removed => {
                info!("Tablet device {} removed", device.identifier);
                state.mapped_tools.remove(&device.identifier);
                state.forget_unplugged()
            }
            // Among others, the libinput_config events caused by the
            // daemon's own commands.
            _ => Ok(()),
        }
    }

    /// Re-applies what depends on the outputs after they changed.
    pub fn outputs_changed(&self) -> Result<(), Box<dyn Error>> {
        let mut state = self.lock_state();
        let devices = state.devices(None)?;
        if !devices.is_empty() {
            debug!("Outputs changed, updating tablet mappings");
            state.apply(&devices, Changes::OUTPUTS)?;
        }
        Ok(())
    }

    fn settings_changed(&self, tablet: TabletId, key: &str) -> Result<(), Box<dyn Error>> {
        let changes = Changes::for_key(key);
        if changes == Changes::NONE {
            return Ok(());
        }
        info!("{TABLET_SCHEMA} {tablet} -> Key: {key} changed");
        let mut state = self.lock_state();
        let devices = state.devices(Some(tablet))?;
        state.apply(&devices, changes)
    }

    /// Starts watching the settings of a device's tablet.
    fn watch(&self, state: &mut TabletState, device: &Input) {
        let Some(tablet) = TabletId::of(device) else {
            return;
        };
        if state.watched.contains_key(&tablet) {
            return;
        }
        let settings = Settings::with_path(TABLET_SCHEMA, &tablet.settings_path());
        let handler: Weak<Mutex<TabletState>> = Arc::downgrade(&self.state);
        let changed_handler = settings.connect_changed(None, move |_, key| {
            if let Some(state) = handler.upgrade() {
                if let Err(e) = (TabletHandler { state }).settings_changed(tablet, key) {
                    warn!("Failed to apply tablet setting {key} for {tablet}: {e}");
                }
            }
        });
        debug!("Watching the settings of tablet {tablet}");
        state.watched.insert(
            tablet,
            WatchedTablet {
                settings,
                changed_handler: Some(changed_handler),
            },
        );
    }
}

impl TabletState {
    /// The tablet devices sway reports, of one tablet or of all.
    fn devices(&mut self, tablet: Option<TabletId>) -> Result<Vec<Input>, Box<dyn Error>> {
        Ok(utils::get_inputs()?
            .into_iter()
            .filter(|input| {
                TabletId::of(input).is_some_and(|id| tablet.is_none() || tablet == Some(id))
            })
            .collect())
    }

    /// Stops watching tablets that are no longer plugged in.
    fn forget_unplugged(&mut self) -> Result<(), Box<dyn Error>> {
        let present: BTreeSet<TabletId> = self
            .devices(None)?
            .iter()
            .filter_map(TabletId::of)
            .collect();
        self.watched.retain(|tablet, _| present.contains(tablet));
        Ok(())
    }

    fn info(&mut self, device: &Input, tablet: TabletId) -> Option<TabletInfo> {
        let database = self.database.get_or_insert_with(TabletDatabase::load);
        let info = database.lookup(tablet.vendor, tablet.product, &device.name);
        if info.is_none() {
            debug!("{} is not in the libwacom database", device.identifier);
        }
        info.cloned()
    }

    fn apply(&mut self, devices: &[Input], changes: Changes) -> Result<(), Box<dyn Error>> {
        if devices.is_empty() {
            return Ok(());
        }
        let outputs = if changes.needs_outputs() {
            self.sway_connection.get_outputs()?
        } else {
            Vec::new()
        };
        for device in devices {
            let Some(tablet) = TabletId::of(device) else {
                continue;
            };
            let Some(watched) = self.watched.get(&tablet) else {
                continue;
            };
            let settings = TabletSettings::read(&watched.settings);
            let info = self.info(device, tablet);
            let was_mapped = self.mapped_tools.contains(&device.identifier);
            let (commands, mapped) = device_commands(
                device,
                info.as_ref(),
                &settings,
                &outputs,
                changes,
                was_mapped,
            );
            if mapped {
                self.mapped_tools.insert(device.identifier.clone());
            }
            for command in commands {
                self.run(&command)?;
            }
        }
        Ok(())
    }

    fn run(&mut self, command: &str) -> Result<(), Box<dyn Error>> {
        info!("{command}");
        for outcome in self.sway_connection.run_command(command)? {
            if let Err(e) = outcome {
                warn!("sway rejected {command:?}: {e}");
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn device(identifier: &str, input_type: &str) -> Input {
        serde_json::from_value(json!({
            "identifier": identifier,
            "name": identifier,
            "vendor": 1386,
            "product": 855,
            "type": input_type,
        }))
        .unwrap()
    }

    fn output(name: &str, serial: &str, rect: (i32, i32, i32, i32)) -> Output {
        serde_json::from_value(json!({
            "name": name, "make": "ACME", "model": "Wide", "serial": serial,
            "active": true, "dpms": true, "primary": false,
            "rect": {"x": rect.0, "y": rect.1, "width": rect.2, "height": rect.3},
        }))
        .unwrap()
    }

    fn info(size: (f64, f64), display: bool, system: bool) -> TabletInfo {
        TabletInfo {
            name: "Tablet".to_string(),
            size: Some(size),
            integrated_in_display: display,
            integrated_in_system: system,
        }
    }

    fn settings() -> TabletSettings {
        TabletSettings {
            tool_mode: ToolMode::Absolute,
            monitor: MonitorSetting::Automatic,
            keep_aspect: false,
            area: vec![0.0; 4],
            left_handed: false,
        }
    }

    fn monitor(serial: &str, connector: Option<&str>) -> MonitorSetting {
        MonitorSetting::Monitor {
            vendor: "ACME".to_string(),
            product: "Wide".to_string(),
            serial: serial.to_string(),
            connector: connector.map(String::from),
        }
    }

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| v.to_string()).collect()
    }

    #[test]
    fn tablet_ids_are_lowercase_hex() {
        let pen = device("1386:855:Wacom_Intuos_Pro_M_Pen", TABLET_TOOL);
        let id = TabletId::of(&pen).unwrap();
        assert_eq!(id.to_string(), "056a:0357");
        assert_eq!(
            id.settings_path(),
            "/org/gnome/desktop/peripherals/tablets/056a:0357/"
        );
        assert_eq!(TabletId::of(&device("1386:855:Wacom_Touch", "touch")), None);
    }

    #[test]
    fn parses_the_output_key() {
        assert_eq!(
            MonitorSetting::parse(&strings(&["", "", ""])),
            MonitorSetting::Automatic
        );
        assert_eq!(
            MonitorSetting::parse(&strings(&["ACME"])),
            MonitorSetting::Automatic
        );
        assert_eq!(
            MonitorSetting::parse(&strings(&["ACME", "Wide", "A1"])),
            monitor("A1", None)
        );
        assert_eq!(
            MonitorSetting::parse(&strings(&["ACME", "Wide", "A1", "DP-2"])),
            monitor("A1", Some("DP-2"))
        );
    }

    #[test]
    fn connector_only_tells_identical_monitors_apart() {
        let outputs = [
            output("DP-1", "TWIN", (0, 0, 1920, 1080)),
            output("DP-2", "TWIN", (1920, 0, 1920, 1080)),
            output("HDMI-1", "SOLO", (3840, 0, 1920, 1080)),
        ];
        let name = |setting: &MonitorSetting| match resolve_mapping(setting, &outputs, false) {
            Mapping::Output(o) => o.name.clone(),
            other => format!("{other:?}"),
        };
        assert_eq!(name(&monitor("TWIN", Some("DP-2"))), "DP-2");
        assert_eq!(name(&monitor("TWIN", None)), "DP-1");
        assert_eq!(name(&monitor("TWIN", Some("HDMI-9"))), "Desktop");
        assert_eq!(name(&monitor("SOLO", Some("HDMI-9"))), "HDMI-1");
        assert_eq!(name(&monitor("GONE", Some("HDMI-1"))), "Desktop");
    }

    #[test]
    fn automatic_maps_external_tablets_to_the_desktop_only() {
        let outputs = [output("HDMI-1", "A", (0, 0, 1920, 1080))];
        assert!(matches!(
            resolve_mapping(&MonitorSetting::Automatic, &outputs, false),
            Mapping::Desktop
        ));
        assert!(matches!(
            resolve_mapping(&MonitorSetting::Automatic, &outputs, true),
            Mapping::SwayDefault
        ));
    }

    #[test]
    fn desktop_is_the_bounding_box_of_the_outputs() {
        let outputs = [
            output("HDMI-1", "A", (0, 0, 1920, 1080)),
            output("DP-2", "B", (-1600, -200, 1280, 1024)),
        ];
        assert_eq!(desktop_size(&outputs), Some((3520.0, 1280.0)));
        assert_eq!(desktop_size(&[]), None);
    }

    #[test]
    fn sway_maps_built_in_tablets_to_the_built_in_panel() {
        let laptop = info((9.0, 5.0), true, true);
        let cintiq = info((12.0, 7.0), true, false);
        let outputs = [
            output("eDP-1", "A", (0, 0, 1920, 1200)),
            output("HDMI-1", "B", (1920, 0, 1920, 1080)),
        ];
        assert_eq!(
            sway_default_output(&outputs, Some(&laptop)).map(|o| o.name.as_str()),
            Some("eDP-1")
        );
        assert!(sway_default_output(&outputs, Some(&cintiq)).is_none());
        assert_eq!(
            mapped_size(&Mapping::SwayDefault, &outputs, Some(&laptop)),
            Some((1920.0, 1200.0))
        );
        assert_eq!(
            mapped_size(&Mapping::SwayDefault, &outputs, Some(&cintiq)),
            Some((3840.0, 1200.0))
        );
    }

    #[test]
    fn calibration_margins_become_a_region() {
        assert_eq!(
            Region::from_margins(&[0.1, 0.2, 0.05, 0.15]),
            Some(Region {
                x1: 0.1,
                y1: 0.05,
                x2: 0.8,
                y2: 0.85
            })
        );
        assert_eq!(Region::from_margins(&[0.0; 4]), Some(Region::FULL));
        assert_eq!(Region::from_margins(&[0.6, 0.5, 0.0, 0.0]), None);
        assert_eq!(Region::from_margins(&[-0.1, 0.0, 0.0, 0.0]), None);
        assert_eq!(Region::from_margins(&[0.1, 0.1]), None);
    }

    #[test]
    fn keep_aspect_crops_from_the_top_left() {
        // A 3:2 tablet on a 16:9 monitor uses all its width.
        let region = Region::FULL.crop_to_aspect(1.5, 16.0 / 9.0);
        assert_eq!(region.to_string(), "0x0 1x0.8438");
        // On a portrait monitor it uses part of its width.
        let region = Region::FULL.crop_to_aspect(1.5, 9.0 / 16.0);
        assert_eq!(region.to_string(), "0x0 0.375x1");
        // Within calibration margins.
        let calibrated = Region::from_margins(&[0.1, 0.1, 0.0, 0.0]).unwrap();
        assert_eq!(
            calibrated.crop_to_aspect(1.5, 0.75).to_string(),
            "0.1x0 0.5x1"
        );
    }

    #[test]
    fn calibration_only_applies_to_screen_tablets() {
        let mut stored = settings();
        stored.area = vec![0.1, 0.2, 0.05, 0.15];
        let size = Some((1920.0, 1080.0));
        let cintiq = info((12.0, 7.0), true, false);
        let intuos = info((9.0, 6.0), false, false);
        assert_eq!(
            active_region(&stored, Some(&cintiq), size).to_string(),
            "0.1x0.05 0.8x0.85"
        );
        assert_eq!(active_region(&stored, Some(&intuos), size), Region::FULL);
        assert_eq!(active_region(&stored, None, size), Region::FULL);
        stored.keep_aspect = true;
        assert_eq!(
            active_region(&stored, Some(&intuos), size).to_string(),
            "0x0 1x0.8438"
        );
        assert_eq!(active_region(&stored, Some(&intuos), None), Region::FULL);
    }

    #[test]
    fn keys_map_to_the_settings_they_change() {
        assert_eq!(
            Changes::for_key("mapping"),
            Changes {
                tool_mode: true,
                ..Changes::NONE
            }
        );
        assert_eq!(Changes::for_key("output"), Changes::OUTPUTS);
        assert_eq!(Changes::for_key("area"), Changes::for_key("keep-aspect"));
        assert_eq!(Changes::for_key("pressure-curve"), Changes::NONE);
    }

    #[test]
    fn external_tablet_tool_and_pad_commands() {
        let pen = device("1386:855:Wacom_Intuos_Pro_M_Pen", TABLET_TOOL);
        let pad = device("1386:855:Wacom_Intuos_Pro_M_Pad", TABLET_PAD);
        let intuos = info((9.0, 6.0), false, false);
        let outputs = [output("HDMI-1", "A1", (0, 0, 1920, 1080))];
        let mut stored = settings();
        stored.tool_mode = ToolMode::Relative;
        stored.monitor = monitor("A1", None);
        stored.keep_aspect = true;
        stored.left_handed = true;
        let (commands, mapped) =
            device_commands(&pen, Some(&intuos), &stored, &outputs, Changes::ALL, false);
        assert_eq!(
            commands,
            [
                r#"input "1386:855:Wacom_Intuos_Pro_M_Pen" tool_mode * relative"#,
                r#"input "1386:855:Wacom_Intuos_Pro_M_Pen" map_to_output "HDMI-1""#,
                r#"input "1386:855:Wacom_Intuos_Pro_M_Pen" map_from_region 0x0 1x0.8438"#,
                r#"input "1386:855:Wacom_Intuos_Pro_M_Pen" left_handed enabled"#,
            ]
        );
        assert!(mapped);
        let (commands, mapped) =
            device_commands(&pad, Some(&intuos), &stored, &outputs, Changes::ALL, false);
        assert_eq!(
            commands,
            [r#"input "1386:855:Wacom_Intuos_Pro_M_Pad" left_handed enabled"#]
        );
        assert!(!mapped);
    }

    #[test]
    fn screen_tablets_keep_sways_mapping_and_handedness() {
        let pen = device("1386:772:Wacom_Cintiq_13HD_Pen", TABLET_TOOL);
        let cintiq = info((12.0, 7.0), true, false);
        let outputs = [output("HDMI-1", "A1", (0, 0, 1920, 1080))];
        let mut stored = settings();
        stored.left_handed = true;
        let (commands, mapped) =
            device_commands(&pen, Some(&cintiq), &stored, &outputs, Changes::ALL, false);
        assert_eq!(
            commands,
            [
                r#"input "1386:772:Wacom_Cintiq_13HD_Pen" tool_mode * absolute"#,
                r#"input "1386:772:Wacom_Cintiq_13HD_Pen" map_from_region 0x0 1x1"#,
            ]
        );
        assert!(!mapped);
        // After the daemon mapped it, automatic maps it where sway would.
        let (commands, mapped) = device_commands(
            &pen,
            Some(&cintiq),
            &stored,
            &outputs,
            Changes::OUTPUTS,
            true,
        );
        assert_eq!(
            commands[0],
            r#"input "1386:772:Wacom_Cintiq_13HD_Pen" map_to_output "*""#
        );
        assert!(mapped);
    }

    /// Sway variables, longest name first as sway sorts them.
    const VARIABLES: [(&str, &str); 4] = [
        ("$wm.layout", "de"),
        ("$mod", "Mod4"),
        ("$a", "VAR"),
        ("$", "DOLLAR"),
    ];

    /// The commands sway 1.11 runs for an IPC command list, as the
    /// arguments each handler gets: a port of `execute_command`
    /// (sway/commands.c), with `argsep`, `split_args` and `strip_quotes`
    /// (common/stringop.c) and `do_var_replacement` (sway/config.c).
    fn sway_reads(payload: &str) -> Vec<Vec<String>> {
        // Line feeds and criteria aren't ported.
        assert!(!payload.contains('\n') && !payload.starts_with('['));
        let mut commands = Vec::new();
        let mut head = Some(payload);
        while let Some(rest) = head {
            let (command, next) = argsep(rest.trim_start());
            head = next;
            let command = command.trim_start();
            if command.is_empty() {
                continue;
            }
            let mut argv = split_args(command);
            let strips = !matches!(argv[0].as_str(), "exec" | "exec_always" | "mode");
            for arg in argv.iter_mut().skip(1) {
                if strips && arg.starts_with(['"', '\'']) {
                    *arg = strip_quotes(arg);
                }
                *arg = var_replacement(arg);
            }
            commands.push(argv);
        }
        commands
    }

    const SWAY_WHITESPACE: [char; 6] = [' ', '\x0c', '\n', '\r', '\t', '\x0b'];

    /// One command of a list, and the rest of the list.
    fn argsep(list: &str) -> (&str, Option<&str>) {
        let b = list.as_bytes();
        let (mut start, mut end) = (0, 0);
        let (mut in_string, mut in_char, mut escaped) = (false, false, false);
        while let Some(offset) = b[end..].iter().position(|c| b"\"'\\;,".contains(c)) {
            let i = end + offset;
            if escaped && i != end {
                escaped = false;
            }
            match b[i] {
                b'"' if !in_char && !escaped => in_string = !in_string,
                b'\'' if !in_string && !escaped => in_char = !in_char,
                b'\\' => escaped = !escaped,
                b';' | b',' if !in_string && !in_char && !escaped => {
                    if i > start {
                        return (&list[start..i], Some(&list[i + 1..]));
                    }
                    start = i + 1;
                }
                _ => {}
            }
            end = i + 1;
        }
        (&list[start..], None)
    }

    fn split_args(command: &str) -> Vec<String> {
        let b = command.as_bytes();
        let is_space = |c: u8| SWAY_WHITESPACE.contains(&(c as char));
        let mut args = Vec::new();
        let (mut start, mut end) = (0, 0);
        let (mut in_token, mut in_string, mut in_char) = (false, false, false);
        let (mut in_brackets, mut escaped) = (false, false);
        while start < b.len() {
            if !in_token {
                while end < b.len() && is_space(b[end]) {
                    end += 1;
                }
                start = end;
                in_token = true;
            }
            let c = b.get(end).copied().unwrap_or(0);
            if c == b'"' && !in_char && !escaped {
                in_string = !in_string;
            } else if c == b'\'' && !in_string && !escaped {
                in_char = !in_char;
            } else if c == b'[' && !in_string && !in_char && !in_brackets && !escaped {
                in_brackets = true;
            } else if c == b']' && !in_string && !in_char && in_brackets && !escaped {
                in_brackets = false;
            } else if c == b'\\' {
                escaped = !escaped;
            } else if c == 0 || (!in_string && !in_char && !in_brackets && !escaped && is_space(c))
            {
                if end > start {
                    args.push(command[start..end].to_string());
                }
                in_token = false;
                escaped = false;
                continue;
            }
            if c != b'\\' {
                escaped = false;
            }
            end += 1;
        }
        args
    }

    fn strip_quotes(arg: &str) -> String {
        let (mut in_str, mut in_chr, mut escaped) = (false, false, false);
        let mut out = String::new();
        for c in arg.chars() {
            if c == '\'' && !in_str && !escaped {
                in_chr = !in_chr;
            } else if c == '"' && !in_chr && !escaped {
                in_str = !in_str;
            } else if c == '\\' {
                escaped = !escaped;
                out.push(c);
            } else {
                escaped = false;
                out.push(c);
            }
        }
        out
    }

    fn var_replacement(arg: &str) -> String {
        let mut s = arg.as_bytes().to_vec();
        let mut find = 0;
        while let Some(offset) = s[find..].iter().position(|&c| c == b'$') {
            find += offset;
            if find > 0
                && s[find - 1] == b'\\'
                && (find == 1 || !(find > 1 && s[find - 2] == b'\\'))
            {
                find += 1;
                continue;
            }
            if s.get(find + 1) == Some(&b'$') {
                s.remove(find);
                find += 1;
                continue;
            }
            match VARIABLES
                .iter()
                .find(|(name, _)| s[find..].starts_with(name.as_bytes()))
            {
                Some((name, value)) => {
                    s.splice(find..find + name.len(), value.bytes());
                    find += value.len();
                }
                None => find += 1,
            }
        }
        String::from_utf8(s).unwrap()
    }

    /// What sway reads from the command that sets left-handed mode.
    fn left_handed_reads(identifier: &str) -> Option<Vec<Vec<String>>> {
        input_command(identifier, "left_handed enabled").map(|c| sway_reads(&c))
    }

    fn expected(identifier: &str) -> Vec<Vec<String>> {
        vec![strings(&["input", identifier, "left_handed", "enabled"])]
    }

    #[test]
    fn the_sway_port_reads_what_sway_1_11_reads() {
        // Checked with headless sway 1.11 and `sway -d`.
        for (payload, argument) in [
            (r#"input "1:2:Pen" left_handed enabled"#, "1:2:Pen"),
            (r#"input "x\"y" left_handed enabled"#, r#"x\"y"#),
            (r#"input "a'b"'"c' left_handed enabled"#, r#"a'b"c"#),
            (
                r#"input '$a $$a \$a \\$a' left_handed enabled"#,
                r#"VAR $a \$a \\VAR"#,
            ),
            (r#"input 'a\$b' left_handed enabled"#, r#"a\$b"#),
            (r#"input 'a\\$$b' left_handed enabled"#, r#"a\\$b"#),
            (r#"input '$wm.layout' left_handed enabled"#, "de"),
        ] {
            assert_eq!(
                sway_reads(payload),
                vec![strings(&["input", argument, "left_handed", "enabled"])],
                "{payload}"
            );
        }
        assert_eq!(
            sway_reads(r#"input "1:2:Pad";exec_x;"" left_handed enabled"#),
            vec![
                strings(&["input", "1:2:Pad"]),
                strings(&["exec_x"]),
                strings(&["\"\"", "left_handed", "enabled"]),
            ]
        );
    }

    #[test]
    fn plain_identifiers_are_written_as_before() {
        assert_eq!(
            sway_argument("1386:855:Wacom_Intuos_Pro_M_Pen").as_deref(),
            Some(r#""1386:855:Wacom_Intuos_Pro_M_Pen""#)
        );
        assert_eq!(sway_argument("*").as_deref(), Some(r#""*""#));
        assert_eq!(sway_argument("").as_deref(), Some(r#""""#));
    }

    #[test]
    fn odd_identifiers_reach_sway_intact() {
        for id in [
            r#"1:2:Pen_"odd""#,
            "1:2:it's",
            r#"1:2:a'b"c'd"e"#,
            r#"1:2:a\b\\c"#,
            r#"1:2:a\"b"#,
            r#"1:2:a\'b"#,
            r#"1:2:a'\"b"#,
            r#"1:2:a"\'b"#,
            r#"1:2:a"\\\'b"#,
            r#"\'"#,
            r#"\""#,
            r#"1:2:ends\\"#,
            "1:2:$mod",
            "1:2:$wm.layout_$a$a",
            "1:2:$$",
            "1:2:$",
            r#"1:2:\$a"#,
            r#"1:2:\\$a"#,
            r#"1:2:\\\$a"#,
            r#"1:2:\$$a"#,
            r#"1:2:Pad";exec_touch_/tmp/x;""#,
            "1:2:a;b,c",
            "1:2:[x]",
            "1:2:with space\tand tab",
            " leading and trailing ",
        ] {
            assert_eq!(left_handed_reads(id), Some(expected(id)), "{id}");
        }
    }

    #[test]
    fn identifiers_sway_cannot_read_are_skipped() {
        for id in [
            r#"1:2:ends\"#,
            r#"1:2:ends\\\"#,
            "1:2:line\nfeed",
            "1:2:nul\0",
        ] {
            assert_eq!(sway_argument(id), None, "{id:?}");
            let pen = device(id, TABLET_TOOL);
            let (commands, mapped) =
                device_commands(&pen, None, &settings(), &[], Changes::ALL, false);
            assert!(commands.is_empty(), "{id:?}: {commands:?}");
            assert!(!mapped);
        }
    }

    #[test]
    fn identifiers_sway_would_split_are_skipped() {
        // Quoted right for each argument, but when splitting the command
        // list, the backslash also escapes the quotes after the `'`, so the
        // second `"` looks like it ends a quote and the `;` like a separator.
        let id = r#"\'"a";"#;
        let argument = sway_argument(id).unwrap();
        assert_eq!(argument, r#""\'"'"a";'"#);
        let command = format!("input {argument} left_handed enabled");
        assert_eq!(sway_reads(&command).len(), 2);
        assert_eq!(input_command(id, "left_handed enabled"), None);
    }

    #[test]
    fn every_short_identifier_reaches_sway_intact_or_is_skipped() {
        let alphabet = ['a', '\'', '"', '\\', '$', ';', ' '];
        let mut values = vec![String::new()];
        let mut skipped = 0;
        for _ in 0..5 {
            values = values
                .iter()
                .flat_map(|v| alphabet.iter().map(move |c| format!("{v}{c}")))
                .collect();
            for value in &values {
                match left_handed_reads(value) {
                    Some(read) => assert_eq!(read, expected(value), "{value:?}"),
                    None => {
                        let trailing = value.chars().rev().take_while(|&c| c == '\\').count();
                        // Only a `;` or `,` can trip up how sway splits
                        // command lists.
                        assert!(
                            trailing % 2 == 1 || value.contains([';', ',']),
                            "{value:?} skipped"
                        );
                        skipped += usize::from(trailing % 2 == 0);
                    }
                }
            }
        }
        // Splitting quirks rule out very few.
        assert!(skipped < 50, "{skipped} skipped");
    }

    #[test]
    fn output_names_are_written_like_identifiers() {
        let pen = device(r#"1386:855:Pen_"x""#, TABLET_TOOL);
        let outputs = [output("HDMI-1", "A1", (0, 0, 1920, 1080))];
        let mut stored = settings();
        stored.monitor = monitor("A1", None);
        let changes = Changes {
            mapping: true,
            ..Changes::NONE
        };
        let (commands, mapped) = device_commands(&pen, None, &stored, &outputs, changes, false);
        assert_eq!(
            commands,
            [r#"input '1386:855:Pen_"x"' map_to_output "HDMI-1""#]
        );
        assert!(mapped);
        assert_eq!(
            sway_reads(&commands[0]),
            vec![strings(&[
                "input",
                r#"1386:855:Pen_"x""#,
                "map_to_output",
                "HDMI-1"
            ])]
        );
    }
}
