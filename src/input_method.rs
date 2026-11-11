//! The session's input method: IBus, kept running with its Wayland front end
//! and its X input method server, with the engine of the source in use.
//!
//! GNOME runs IBus in every session and selects the engine of the current
//! input source; sway runs no input method of its own. Without one, nothing
//! converts keystrokes for an IBus source, and GTK4 programs on Wayland,
//! which since GTK 4.20 leave dead keys and compose sequences to the input
//! method, lose accents even on plain layouts.
//!
//! So whenever IBus is installed the daemon keeps it running:
//! - `ibus-daemon`, with `ibus-ui-gtk3 --enable-wayland-im` as its panel,
//!   which serves Wayland programs through sway's input method protocol;
//! - `ibus-x11`, which serves X11 programs on `DISPLAY` through XIM. It runs
//!   on its own rather than through `ibus-daemon --xim`, whose XIM server
//!   takes the whole input method down when an on-demand Xwayland exits.
//!
//! Both are started again when they die. An `ibus-daemon` that already
//! serves the session (one a previous instance of this daemon started) is
//! used instead of starting a second one.
//!
//! Each time it connects to an `ibus-daemon`, the daemon reads the engines
//! that ibus-daemon lists: from then on they decide which IBus sources count
//! (see [`crate::ibus_engines`]), as in GNOME.
//!
//! The source in use is the counted source (see
//! [`crate::input_sources::effective_sources`]) at the keyboard's active
//! layout index in sway. An IBus source selects its own engine; an xkb
//! source selects IBus's XKB engine for its layout, which passes keys
//! through as the layout gives them and composes dead keys. As in GNOME, the
//! engines of all counted sources are preloaded, and the engine of the source
//! in use is selected again when another IBus client (the panel as it starts)
//! selects a different one.

use crate::ibus_engines::{self, Engines};
use crate::input_sources::EffectiveSource;
use crate::utils;
use gio::{Cancellable, DBusCallFlags, DBusConnection, DBusConnectionFlags, DBusSignalFlags};
use glib::{ToVariant, Variant};
use log::{debug, info, warn};
use serde_json::Value;
use std::env;
use std::fs;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use swayipc::{CommandType, Connection as SwayConnection, Error as SwayError, Fallible};

/// The XKB engine GNOME selects for xkb sources. Used when no installed XKB
/// engine matches a layout, and when no source is usable.
const DEFAULT_ENGINE: &str = "xkb:us::eng";
/// How often the input method's processes are checked.
const POLL_INTERVAL: Duration = Duration::from_millis(250);
/// The shortest time between two starts of the same program. For
/// `ibus-daemon` it doubles, up to `MAX_RESTART_DELAY`, while each new one
/// dies before it can be reached.
const RESTART_DELAY: Duration = Duration::from_secs(1);
const MAX_RESTART_DELAY: Duration = Duration::from_secs(16);
/// For `ibus-x11` the delay doubles while each one exits within
/// `XIM_SHORT_RUN`, as it does while no X server answers, up to
/// `MAX_XIM_RESTART_DELAY`: an on-demand Xwayland can start at any time.
const XIM_SHORT_RUN: Duration = Duration::from_secs(5);
const MAX_XIM_RESTART_DELAY: Duration = Duration::from_secs(4);
/// How long a new `ibus-daemon` may take to become reachable.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
/// How long to wait before checking again for programs that aren't installed.
const INSTALL_CHECK_DELAY: Duration = Duration::from_secs(5);
/// Where distributions install IBus's helper programs.
const LIBEXEC_DIRS: [&str; 4] = [
    "/usr/libexec",
    "/usr/libexec/ibus",
    "/usr/lib/ibus",
    "/usr/local/libexec",
];

/// A handle on the session's input method. Outside a Wayland session it
/// does nothing.
pub struct InputMethod {
    shared: Option<Arc<Shared>>,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

#[derive(Default)]
struct State {
    /// A new list of counted sources, not yet taken by the supervisor.
    sources: Option<Vec<EffectiveSource>>,
    /// Whether a keyboard changed since the supervisor last looked.
    keyboards_changed: bool,
    /// The engine IBus last reported as selected, not yet seen by the
    /// supervisor, with the number of the connection that reported it.
    global_engine: Option<(u64, String)>,
}

impl Shared {
    fn update(&self, change: impl FnOnce(&mut State)) {
        change(&mut self.state.lock().unwrap());
        self.changed.notify_one();
    }
}

impl InputMethod {
    /// Start keeping the input method running, in a Wayland session.
    pub fn start() -> InputMethod {
        if env::var_os("WAYLAND_DISPLAY").is_none() {
            info!("WAYLAND_DISPLAY is not set, no input method is started");
            return InputMethod { shared: None };
        }
        let shared = Arc::new(Shared::default());
        let supervisor = Supervisor::new(shared.clone());
        thread::spawn(move || supervisor.run());
        let watched = shared.clone();
        thread::spawn(move || watch_keyboards(&watched));
        InputMethod {
            shared: Some(shared),
        }
    }

    /// Follow a new list of counted sources, in list order.
    pub fn set_sources(&self, sources: Vec<EffectiveSource>) {
        if let Some(shared) = &self.shared {
            shared.update(|state| state.sources = Some(sources));
        }
    }
}

/// The IBus programs the input method is made of.
struct Programs {
    daemon: PathBuf,
    panel: PathBuf,
    xim: PathBuf,
    tool: PathBuf,
}

impl Programs {
    /// The installed IBus programs, if IBus is installed.
    fn find() -> Option<Programs> {
        let path = env::var_os("PATH").unwrap_or_default();
        let bin_dirs: Vec<PathBuf> = env::split_paths(&path).collect();
        let libexec_dirs: Vec<PathBuf> = LIBEXEC_DIRS.iter().map(PathBuf::from).collect();
        let find = |dirs: &[PathBuf], name: &str| {
            dirs.iter()
                .map(|dir| dir.join(name))
                .find(|path| fs::metadata(path).is_ok_and(|m| m.is_file() && m.mode() & 0o111 != 0))
        };
        Some(Programs {
            daemon: find(&bin_dirs, "ibus-daemon")?,
            panel: find(&libexec_dirs, "ibus-ui-gtk3")?,
            xim: find(&libexec_dirs, "ibus-x11")?,
            tool: find(&bin_dirs, "ibus")?,
        })
    }
}

/// A program the supervisor started, and when.
struct Started {
    child: Child,
    at: Instant,
}

impl Started {
    /// Whether the program has exited; it is reaped if so.
    fn exited(&mut self, name: &str) -> bool {
        match self.child.try_wait() {
            Ok(None) => false,
            Ok(Some(status)) => {
                info!("{name} exited: {status}");
                true
            }
            Err(e) => {
                warn!("{name}: {e}");
                true
            }
        }
    }
}

/// Keeps the input method running and its engine following the source in use.
struct Supervisor {
    shared: Arc<Shared>,
    /// The engine of each counted source; `None` until the sources are known.
    engines: Option<Vec<String>>,
    /// The keyboard's active layout index in sway.
    active_layout: Option<usize>,
    /// The `ibus-daemon` this supervisor started.
    daemon: Option<Started>,
    /// The connection to the running `ibus-daemon`, whoever started it.
    bus: Option<DBusConnection>,
    /// The engine selected through `bus`.
    selected: Option<String>,
    /// The engines preloaded through `bus`.
    preloaded: Option<Vec<String>>,
    /// Whether the engine list of the `ibus-daemon` behind `bus` was read.
    engines_listed: bool,
    /// How many connections to an `ibus-daemon` were made: the number of
    /// the one in `bus`.
    connections: u64,
    /// Since when an `ibus-daemon` this supervisor didn't start has been
    /// running without becoming reachable.
    foreign_daemon_since: Option<Instant>,
    /// The `ibus-x11` this supervisor started.
    xim: Option<Started>,
    next_daemon_start: Instant,
    daemon_delay: Duration,
    next_xim_start: Instant,
    xim_delay: Duration,
    /// The engine whose selection last failed; it is tried again each check.
    failed_selection: Option<String>,
    missing_reported: bool,
}

impl Supervisor {
    fn new(shared: Arc<Shared>) -> Supervisor {
        let now = Instant::now();
        Supervisor {
            shared,
            engines: None,
            active_layout: None,
            daemon: None,
            bus: None,
            selected: None,
            preloaded: None,
            engines_listed: false,
            connections: 0,
            foreign_daemon_since: None,
            xim: None,
            next_daemon_start: now,
            daemon_delay: RESTART_DELAY,
            next_xim_start: now,
            xim_delay: RESTART_DELAY,
            failed_selection: None,
            missing_reported: false,
        }
    }

    fn run(mut self) {
        loop {
            self.follow_changes();
            if let Some(daemon) = &mut self.daemon {
                if daemon.exited("ibus-daemon") {
                    self.daemon = None;
                }
            }
            if let Some(xim) = &mut self.xim {
                if xim.exited("ibus-x11") {
                    self.xim_delay = if xim.at.elapsed() < XIM_SHORT_RUN {
                        (self.xim_delay * 2).min(MAX_XIM_RESTART_DELAY)
                    } else {
                        RESTART_DELAY
                    };
                    self.next_xim_start = xim.at + self.xim_delay;
                    self.xim = None;
                }
            }
            self.keep_daemon();
            self.follow_engine_list();
            self.preload_engines();
            self.select_engine();
            self.keep_xim();
        }
    }

    /// Wait for a change of the sources or keyboards, or for the next check.
    fn follow_changes(&mut self) {
        let shared = self.shared.clone();
        let state = shared.state.lock().unwrap();
        let (mut state, _) = shared
            .changed
            .wait_timeout_while(state, POLL_INTERVAL, |s| {
                s.sources.is_none() && !s.keyboards_changed && s.global_engine.is_none()
            })
            .unwrap();
        let sources = state.sources.take();
        let keyboards_changed = std::mem::take(&mut state.keyboards_changed);
        // A report from an earlier ibus-daemon says nothing about this one.
        let global_engine = state
            .global_engine
            .take()
            .filter(|(connection, _)| *connection == self.connections)
            .map(|(_, engine)| engine);
        drop(state);
        // Another IBus client (the panel as it starts, its own switcher)
        // selected an engine: select the one of the source in use again.
        if global_engine.is_some() && global_engine != self.selected {
            info!(
                "IBus switched to {}",
                global_engine.as_deref().unwrap_or_default()
            );
            self.selected = global_engine;
        }
        if let Some(sources) = &sources {
            self.engines = Some(engines_for(sources));
        }
        if sources.is_some() || keyboards_changed {
            let count = self.engines.as_ref().map_or(0, Vec::len);
            match utils::get_inputs_json() {
                Ok(inputs) => {
                    if let Some(index) = active_layout(&inputs, count) {
                        self.active_layout = Some(index);
                    }
                }
                Err(e) => warn!("Cannot read sway's keyboards: {e}"),
            }
        }
    }

    /// The engine of the source in use, once the sources are known.
    fn wanted_engine(&self) -> Option<&str> {
        let engines = self.engines.as_ref()?;
        let engine = self
            .active_layout
            .and_then(|index| engines.get(index))
            .or_else(|| engines.first());
        Some(engine.map_or(DEFAULT_ENGINE, String::as_str))
    }

    /// Make sure an `ibus-daemon` serves the session and is connected.
    fn keep_daemon(&mut self) {
        if let Some(bus) = &self.bus {
            if !bus.is_closed() {
                return;
            }
            info!("The input method stopped, starting it again");
            self.bus = None;
            self.selected = None;
            self.preloaded = None;
        }
        let now = Instant::now();
        if self.daemon.is_none() && now < self.next_daemon_start {
            return;
        }
        let Some(programs) = Programs::find() else {
            if !self.missing_reported {
                info!("IBus is not installed, no input method is started");
                self.missing_reported = true;
            }
            self.next_daemon_start = now + INSTALL_CHECK_DELAY;
            return;
        };
        self.missing_reported = false;
        if let Some(bus) = connect(&programs) {
            info!("Connected to the input method");
            self.connections += 1;
            let connection = self.connections;
            let shared = self.shared.clone();
            bus.signal_subscribe(
                None,
                Some("org.freedesktop.IBus"),
                Some("GlobalEngineChanged"),
                Some("/org/freedesktop/IBus"),
                None,
                DBusSignalFlags::NONE,
                move |_, _, _, _, _, parameters| {
                    if let Some((engine,)) = parameters.get::<(String,)>() {
                        shared.update(|state| state.global_engine = Some((connection, engine)));
                    }
                },
            );
            self.bus = Some(bus);
            // Nothing is selected or preloaded in this ibus-daemon yet,
            // whatever an earlier one reported, and its engines are read.
            self.selected = None;
            self.preloaded = None;
            self.engines_listed = false;
            self.foreign_daemon_since = None;
            self.daemon_delay = RESTART_DELAY;
            return;
        }
        if let Some(daemon) = &mut self.daemon {
            if daemon.at.elapsed() < STARTUP_TIMEOUT {
                return;
            }
            warn!("ibus-daemon did not become reachable, starting it again");
            let _ = daemon.child.kill();
            let _ = daemon.child.wait();
            self.daemon = None;
        }
        if now < self.next_daemon_start {
            return;
        }
        // An ibus-daemon someone else started (an IBus tool, the user
        // restarting IBus) may still be starting: it is given the time a new
        // one gets to become reachable before it is replaced.
        if self.daemon.is_none() && own_process_running("ibus-daemon") {
            let since = *self.foreign_daemon_since.get_or_insert(now);
            if now.duration_since(since) < STARTUP_TIMEOUT {
                return;
            }
        }
        self.foreign_daemon_since = None;
        let mut panel = programs.panel.into_os_string();
        panel.push(" --enable-wayland-im");
        // ibus-daemon exits with its parent. A shell waits for it instead, so
        // that a restart of this daemon leaves the input method running for
        // the next instance to use. --replace takes over from an ibus-daemon
        // that holds the session's address but no longer answers.
        match Command::new("/bin/sh")
            .arg("-c")
            .arg("\"$0\" \"$@\"; exit $?")
            .arg(&programs.daemon)
            .arg("--replace")
            .arg("--panel")
            .arg(panel)
            .stdin(Stdio::null())
            .spawn()
        {
            Ok(child) => {
                info!("Started ibus-daemon ({})", child.id());
                self.daemon = Some(Started { child, at: now });
            }
            Err(e) => warn!("Cannot start {}: {e}", programs.daemon.display()),
        }
        self.next_daemon_start = now + self.daemon_delay;
        self.daemon_delay = (self.daemon_delay * 2).min(MAX_RESTART_DELAY);
    }

    /// Read the engine list of the `ibus-daemon` just connected to, until it
    /// answers. From its answer on, IBus decides which engines exist; the
    /// list stays in force until the next `ibus-daemon` answers.
    fn follow_engine_list(&mut self) {
        let Some(bus) = &self.bus else {
            return;
        };
        if self.engines_listed {
            return;
        }
        // An ibus-daemon still starting may not answer yet: it is asked
        // again at the next check.
        if let Some(engines) = list_engines(bus) {
            self.engines_listed = true;
            ibus_engines::follow_ibus(engines);
        }
    }

    /// Have IBus start the engines of every counted source ahead of their
    /// first use, as GNOME does.
    fn preload_engines(&mut self) {
        let (Some(bus), Some(engines)) = (&self.bus, &self.engines) else {
            return;
        };
        let mut wanted: Vec<String> = Vec::new();
        for engine in engines {
            if !wanted.contains(engine) {
                wanted.push(engine.clone());
            }
        }
        if self.preloaded.as_ref() == Some(&wanted) {
            return;
        }
        if let Err(e) = preload_engines(bus, &wanted) {
            warn!("Cannot preload the input method engines: {e}");
        }
        self.preloaded = Some(wanted);
    }

    /// Select the engine of the source in use, if it isn't selected yet.
    fn select_engine(&mut self) {
        let (Some(bus), Some(engine)) = (&self.bus, self.wanted_engine()) else {
            return;
        };
        if self.selected.as_deref() == Some(engine) {
            return;
        }
        let engine = engine.to_string();
        match set_global_engine(bus, &engine) {
            Ok(()) => {
                info!("Input method engine: {engine}");
                self.selected = Some(engine);
                self.failed_selection = None;
            }
            // A new ibus-daemon can cancel a selection while it starts up.
            Err(e) if self.failed_selection.as_ref() != Some(&engine) => {
                warn!("Cannot select the input method engine {engine}, trying again: {e}");
                self.failed_selection = Some(engine);
            }
            Err(_) => (),
        }
    }

    /// Make sure an `ibus-x11` serves X11 programs on the session's X display.
    fn keep_xim(&mut self) {
        let now = Instant::now();
        if self.bus.is_none()
            || self.xim.is_some()
            || now < self.next_xim_start
            || env::var_os("DISPLAY").is_none()
        {
            return;
        }
        self.next_xim_start = now + RESTART_DELAY;
        // One left running by a previous instance of the daemon still serves
        // the display; it exits with the X server or with ibus-daemon.
        if own_process_running("ibus-x11") {
            return;
        }
        let Some(programs) = Programs::find() else {
            return;
        };
        match Command::new(&programs.xim).stdin(Stdio::null()).spawn() {
            Ok(child) => {
                info!("Started ibus-x11 ({})", child.id());
                self.xim = Some(Started { child, at: now });
            }
            Err(e) => warn!("Cannot start {}: {e}", programs.xim.display()),
        }
    }
}

/// Connect to the `ibus-daemon` serving the session, if one is running.
fn connect(programs: &Programs) -> Option<DBusConnection> {
    let output = Command::new(&programs.tool)
        .arg("address")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let address = String::from_utf8(output.stdout).ok()?;
    let address = address.trim();
    // `ibus address` prints "(null)" while no ibus-daemon is running.
    if !output.status.success() || !address.contains(':') {
        return None;
    }
    DBusConnection::for_address_sync(
        address,
        DBusConnectionFlags::AUTHENTICATION_CLIENT | DBusConnectionFlags::MESSAGE_BUS_CONNECTION,
        None,
        Cancellable::NONE,
    )
    .ok()
}

/// The engines `ibus-daemon` lists (its `Engines` property), if it answers.
fn list_engines(bus: &DBusConnection) -> Option<Engines> {
    let reply = bus
        .call_sync(
            Some("org.freedesktop.IBus"),
            "/org/freedesktop/IBus",
            "org.freedesktop.DBus.Properties",
            "Get",
            Some(&("org.freedesktop.IBus", "Engines").to_variant()),
            None,
            DBusCallFlags::NONE,
            5000,
            Cancellable::NONE,
        )
        .map_err(|e| debug!("The input method does not list its engines: {e}"))
        .ok()?;
    engine_descriptions(&reply.try_child_value(0)?.as_variant()?)
}

/// The engines of an `Engines` value: an array of serialized
/// `IBusEngineDesc` objects, each a variant holding its type name, its
/// attachments, then its fields in order (name, long name, description,
/// language, license, author, icon, layout, rank, hotkeys, symbol, setup,
/// layout variant, layout option, …).
fn engine_descriptions(list: &Variant) -> Option<Engines> {
    const NAME: usize = 2;
    const LAYOUT: usize = 9;
    const LAYOUT_VARIANT: usize = 14;
    if !list.is_container() {
        return None;
    }
    let mut engines = Engines::new();
    for desc in list.iter() {
        let desc = desc.as_variant().unwrap_or(desc);
        if !desc.is_container() || desc.n_children() <= LAYOUT_VARIANT {
            continue;
        }
        let field = |index| {
            desc.try_child_value(index)
                .and_then(|v| v.str().map(String::from))
        };
        if field(0).as_deref() != Some("IBusEngineDesc") {
            continue;
        }
        let (Some(name), Some(layout), Some(variant)) =
            (field(NAME), field(LAYOUT), field(LAYOUT_VARIANT))
        else {
            continue;
        };
        engines.insert(name, ibus_engines::engine_layout(&layout, &variant));
    }
    Some(engines)
}

fn preload_engines(bus: &DBusConnection, engines: &[String]) -> Result<(), glib::Error> {
    bus.call_sync(
        Some("org.freedesktop.IBus"),
        "/org/freedesktop/IBus",
        "org.freedesktop.DBus.Properties",
        "Set",
        Some(
            &(
                "org.freedesktop.IBus",
                "PreloadEngines",
                engines.to_variant(),
            )
                .to_variant(),
        ),
        None,
        DBusCallFlags::NONE,
        5000,
        Cancellable::NONE,
    )
    .map(|_| ())
}

fn set_global_engine(bus: &DBusConnection, engine: &str) -> Result<(), glib::Error> {
    bus.call_sync(
        Some("org.freedesktop.IBus"),
        "/org/freedesktop/IBus",
        "org.freedesktop.IBus",
        "SetGlobalEngine",
        Some(&(engine,).to_variant()),
        None,
        DBusCallFlags::NONE,
        5000,
        Cancellable::NONE,
    )
    .map(|_| ())
}

/// Whether a live process of this user runs the program `name`.
fn own_process_running(name: &str) -> bool {
    let Ok(me) = fs::metadata("/proc/self") else {
        return false;
    };
    let Ok(entries) = fs::read_dir("/proc") else {
        return false;
    };
    entries.flatten().any(|entry| {
        let path = entry.path();
        entry
            .file_name()
            .to_string_lossy()
            .bytes()
            .all(|b| b.is_ascii_digit())
            && fs::metadata(&path).is_ok_and(|m| m.uid() == me.uid())
            && fs::read_to_string(path.join("comm")).is_ok_and(|c| c.trim_end() == name)
            && fs::read_to_string(path.join("stat")).is_ok_and(|stat| {
                // An exited process waiting to be reaped serves nothing.
                stat.rsplit_once(") ")
                    .is_some_and(|(_, rest)| !rest.starts_with(['Z', 'X']))
            })
    })
}

/// The engine of each counted source.
fn engines_for(sources: &[EffectiveSource]) -> Vec<String> {
    let mut installed = None;
    sources
        .iter()
        .map(|source| match &source.engine {
            Some(engine) => engine.clone(),
            None => xkb_engine(
                installed.get_or_insert_with(ibus_engines::installed),
                &source.layout,
                &source.variant,
            ),
        })
        .collect()
}

/// IBus's XKB engine for a layout and variant: the first by name of the
/// installed `xkb:` engines with that layout and variant.
fn xkb_engine(installed: &Engines, layout: &str, variant: &str) -> String {
    installed
        .iter()
        .filter(|(name, (l, v))| name.starts_with("xkb:") && l == layout && v == variant)
        .map(|(name, _)| name)
        .min()
        .cloned()
        .unwrap_or_else(|| String::from(DEFAULT_ENGINE))
}

/// The keyboard's active layout index: that of the first keyboard with one
/// layout per counted source, else that of the first keyboard. Virtual
/// keyboards (the input method's own, a remote desktop's) take their state
/// from the program driving them, so they count only when nothing else does.
fn active_layout(inputs: &[Value], count: usize) -> Option<usize> {
    let (virtual_keyboards, keyboards): (Vec<&Value>, Vec<&Value>) = inputs
        .iter()
        .filter(|i| i["type"] == "keyboard" && i["xkb_active_layout_index"].is_u64())
        .partition(|i| {
            i["identifier"]
                .as_str()
                .is_some_and(|id| id.contains("virtual_keyboard"))
        });
    let layouts = |i: &&&Value| i["xkb_layout_names"].as_array().map_or(0, Vec::len);
    let keyboard = keyboards
        .iter()
        .find(|i| layouts(i) == count.max(1))
        .or_else(|| keyboards.first())
        .or_else(|| virtual_keyboards.first())?;
    keyboard["xkb_active_layout_index"]
        .as_u64()
        .map(|index| index as usize)
}

/// Tell the supervisor whenever a keyboard changes: sway sends an input event
/// for every layout switch and keymap change.
fn watch_keyboards(shared: &Shared) {
    loop {
        if let Err(e) = follow_input_events(shared) {
            warn!("sway input events: {e}");
        }
        thread::sleep(RESTART_DELAY);
    }
}

fn follow_input_events(shared: &Shared) -> Fallible<()> {
    let mut stream: UnixStream = SwayConnection::new()?.into();
    stream.write_all(&CommandType::Subscribe.encode_with(r#"["input"]"#))?;
    let (_, reply) = utils::receive_message(&mut stream)?;
    let reply: Value = serde_json::from_slice(&reply)?;
    if reply["success"] != true {
        return Err(SwayError::SubscriptionFailed(String::from("input")));
    }
    // The keyboards may have changed before the subscription.
    shared.update(|state| state.keyboards_changed = true);
    loop {
        let (_, payload) = utils::receive_message(&mut stream)?;
        let Ok(event) = serde_json::from_slice::<Value>(&payload) else {
            continue;
        };
        if event["input"]["type"] == "keyboard" {
            shared.update(|state| state.keyboards_changed = true);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{active_layout, engine_descriptions, xkb_engine, DEFAULT_ENGINE};
    use crate::ibus_engines::Engines;
    use glib::{ToVariant, Variant};
    use serde_json::json;

    #[test]
    fn xkb_sources_select_the_xkb_engine_of_their_layout() {
        let installed: Engines = [
            ("xkb:us::eng", "us", ""),
            ("xkb:us:intl:eng", "us", "intl"),
            ("xkb:us:altgr-intl:fra", "us", "altgr-intl"),
            ("xkb:us:altgr-intl:eng", "us", "altgr-intl"),
            ("anthy", "jp", ""),
            ("xkb:jp::jpn", "jp", ""),
        ]
        .into_iter()
        .map(|(n, l, v)| (n.to_string(), (l.to_string(), v.to_string())))
        .collect();
        assert_eq!(xkb_engine(&installed, "us", "intl"), "xkb:us:intl:eng");
        assert_eq!(
            xkb_engine(&installed, "us", "altgr-intl"),
            "xkb:us:altgr-intl:eng"
        );
        assert_eq!(xkb_engine(&installed, "jp", ""), "xkb:jp::jpn");
        assert_eq!(xkb_engine(&installed, "xx", ""), DEFAULT_ENGINE);
    }

    #[test]
    fn the_active_layout_is_read_from_the_keyboard_with_every_layout() {
        let inputs = vec![
            json!({"type": "pointer"}),
            json!({"identifier": "0:0:wlr_virtual_keyboard_v1", "type": "keyboard",
                   "xkb_layout_names": ["German", "English (US)"], "xkb_active_layout_index": 0}),
            json!({"identifier": "1:1:AT_keyboard", "type": "keyboard",
                   "xkb_layout_names": ["English (US)"], "xkb_active_layout_index": 0}),
            json!({"identifier": "0:0:x11-keyboard", "type": "keyboard",
                   "xkb_layout_names": ["German", "English (US)"], "xkb_active_layout_index": 1}),
        ];
        assert_eq!(active_layout(&inputs, 2), Some(1));
        assert_eq!(active_layout(&inputs, 3), Some(0));
        assert_eq!(active_layout(&inputs[..2], 2), Some(0));
        assert_eq!(active_layout(&inputs[..1], 2), None);
    }

    /// A serialized `IBusEngineDesc`, as ibus-daemon sends it.
    fn engine_desc(name: &str, layout: &str, variant: &str) -> Variant {
        let attachments = Variant::array_from_iter_with_type(
            glib::VariantTy::new("{sv}").unwrap(),
            Vec::<Variant>::new(),
        );
        let mut fields = vec!["IBusEngineDesc".to_variant(), attachments];
        for text in [name, "long", "desc", "ja", "GPL", "author", "icon", layout] {
            fields.push(text.to_variant());
        }
        fields.push(80u32.to_variant());
        for text in ["", "symbol", "", variant, "option", "1.0", "domain", ""] {
            fields.push(text.to_variant());
        }
        Variant::tuple_from_iter(fields)
    }

    #[test]
    fn ibus_engine_lists_are_decoded() {
        let list = Variant::array_from_iter_with_type(
            glib::VariantTy::VARIANT,
            [
                Variant::from_variant(&engine_desc("anthy", "default", "")),
                Variant::from_variant(&engine_desc("hangul", "kr", "kr104")),
                Variant::from_variant(&engine_desc(
                    "m17n:hi:inscript",
                    "default[lv3:ralt_switch]",
                    "default",
                )),
                Variant::from_variant(&"not an engine".to_variant()),
            ],
        );
        let engines = engine_descriptions(&list).unwrap();
        assert_eq!(engines.len(), 3);
        assert_eq!(engines["anthy"], ("us".to_string(), String::new()));
        assert_eq!(engines["hangul"], ("kr".to_string(), "kr104".to_string()));
        assert_eq!(
            engines["m17n:hi:inscript"],
            ("us".to_string(), String::new())
        );
        assert_eq!(engine_descriptions(&"x".to_variant()), None);
    }
}
