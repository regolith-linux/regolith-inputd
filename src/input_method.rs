//! The session's input method: IBus, kept running with its Wayland front end
//! and its X input method server, with the engine of the source in use.
//!
//! GNOME runs IBus in every session and selects the engine of the current
//! input source; sway runs no input method of its own. Without one, nothing
//! converts keystrokes for an IBus source, and GTK4 programs on Wayland,
//! which since GTK 4.20 leave dead keys and compose sequences to the input
//! method, lose accents even on plain layouts.
//!
//! So when IBus is installed and the session opts in (see below), the daemon
//! keeps it running:
//! - `ibus-daemon`, with `ibus-ui-gtk3 --enable-wayland-im` as its panel,
//!   which serves Wayland programs through sway's input method protocol.
//!   The daemon starts the panel itself, with `ibus-daemon --panel disable`,
//!   so that it can start it again;
//! - `ibus-x11`, which serves X11 programs on `DISPLAY` through XIM. It runs
//!   on its own rather than through `ibus-daemon --xim`, whose XIM server
//!   takes the whole input method down when an on-demand Xwayland exits.
//!
//! All are started again when they die. An `ibus-daemon` that already
//! serves the session without a panel of its own (one a previous instance
//! of this daemon started) is used instead of starting a second one; one
//! that runs its own panel (one an earlier version of this daemon or the
//! user started, or one that restarted with its default panel) is replaced.
//! Only one input method can serve sway's seat, and each panel shows its own
//! tray icon, so every other panel on the session's display is stopped:
//! before the daemon starts its own, and while its own runs, as
//! `ibus restart` starts a second one a few seconds after the first.
//!
//! The panel is also started again shortly after an output appears or is
//! turned on. IBus 1.5.32 to 1.5.34 reset their count of the compositor's
//! input method updates on every new Wayland global, a new `wl_output`
//! included, and sway then drops every text they commit: typing stops while
//! key bindings still work (ibus/ibus#2870, fixed in 1.5.35). A new panel
//! starts counting afresh. An output is told apart by its name and by the
//! ID sway gives it when it is created, so that one destroyed and created
//! again under the same name (a laptop's panel on resume) counts as new even
//! when the daemon never saw it gone. Outputs going away add no global and
//! are left alone.
//!
//! The source in use is the counted source (see
//! [`crate::input_sources::effective_sources`]) at the keyboard's active
//! layout index in sway. An IBus source selects its own engine; an xkb
//! source selects IBus's XKB engine for its layout, which passes keys
//! through as the layout gives them and composes dead keys. As in GNOME, the
//! engines of all counted sources are preloaded, and the engine of the source
//! in use is selected again when another IBus client (the panel as it starts)
//! selects a different one.
//!
//! The integration is experimental and off unless the trawl resource
//! `regolith.experimental.ibus` is `true`, read from trawld once as the
//! daemon starts.
//! IBus 1.5.32 to 1.5.34, the releases Debian trixie and recent Ubuntu ship,
//! stop committing text in Wayland programs once a monitor is connected or
//! disconnected (<https://github.com/ibus/ibus/issues/2870>, fixed in
//! 1.5.35). An IBus the user starts themselves is not affected by the
//! setting.

use crate::input_sources::{ibus_engines, EffectiveSource};
use crate::{sway, utils};
use gio::{
    BusType, Cancellable, DBusCallFlags, DBusConnection, DBusConnectionFlags, DBusSignalFlags,
};
use glib::ToVariant;
use log::{debug, info, warn};
use serde_json::Value;
use std::collections::{BTreeSet, HashMap};
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use swayipc::Fallible;

/// The XKB engine GNOME selects for xkb sources. Used when no installed XKB
/// engine matches a layout, and when no source is usable.
const DEFAULT_ENGINE: &str = "xkb:us::eng";
/// The trawl resource that turns the input method on.
const ENABLE_RESOURCE: &str = "regolith.experimental.ibus";
/// How often the input method's processes are checked.
const POLL_INTERVAL: Duration = Duration::from_millis(250);
/// The shortest time between two starts of the same program. For
/// `ibus-daemon` it doubles, up to `MAX_RESTART_DELAY`, while each new one
/// dies before it can be reached.
const RESTART_DELAY: Duration = Duration::from_secs(1);
const MAX_RESTART_DELAY: Duration = Duration::from_secs(16);
/// For the panel and `ibus-x11` the delay doubles while each one exits
/// within `SHORT_RUN`, as `ibus-x11` does while no X server answers, up to
/// `MAX_HELPER_RESTART_DELAY`: an on-demand Xwayland can start at any time.
const SHORT_RUN: Duration = Duration::from_secs(5);
const MAX_HELPER_RESTART_DELAY: Duration = Duration::from_secs(4);
/// How long a new `ibus-daemon` may take to become reachable.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a program may take to exit once asked to.
const STOP_TIMEOUT: Duration = Duration::from_secs(2);
/// How often the session's display is checked for panels this daemon didn't
/// start while its own runs.
const PANEL_CHECK_INTERVAL: Duration = Duration::from_secs(1);
/// How long after the last output appeared the panel is started again:
/// docking or a display profile can turn on several outputs in a row.
const OUTPUT_SETTLE: Duration = Duration::from_secs(1);
/// The panel's option to serve Wayland programs.
const WAYLAND_IM_OPTION: &str = "--enable-wayland-im";
/// The message type of sway's output events.
const OUTPUT_EVENT: u32 = 0x8000_0001;
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
    /// Whether an output changed since the supervisor last looked.
    outputs_changed: bool,
    /// The engine IBus last reported as selected, not yet seen by the
    /// supervisor.
    global_engine: Option<String>,
}

impl Shared {
    fn update(&self, change: impl FnOnce(&mut State)) {
        change(&mut self.state.lock().unwrap());
        self.changed.notify_one();
    }
}

impl InputMethod {
    /// Start keeping the input method running, in a Wayland session that
    /// opts in through `ENABLE_RESOURCE`.
    pub fn start() -> InputMethod {
        if env::var_os("WAYLAND_DISPLAY").is_none() {
            info!("WAYLAND_DISPLAY is not set, no input method is started");
            return InputMethod { shared: None };
        }
        if !enabled_by_resource() {
            info!("{ENABLE_RESOURCE} is not true, no input method is started");
            return InputMethod { shared: None };
        }
        let shared = Arc::new(Shared::default());
        let supervisor = Supervisor::new(shared.clone());
        thread::spawn(move || supervisor.run());
        let watched = shared.clone();
        thread::spawn(move || watch_sway(&watched));
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
    /// The installed IBus programs, if IBus is installed. The helper
    /// programs are looked for in `PATH` first, where they are not
    /// installed, so that a test can stand in for them.
    fn find() -> Option<Programs> {
        let path = env::var_os("PATH").unwrap_or_default();
        let bin_dirs: Vec<PathBuf> = env::split_paths(&path).collect();
        let mut libexec_dirs = bin_dirs.clone();
        libexec_dirs.extend(LIBEXEC_DIRS.iter().map(PathBuf::from));
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

    /// Ask the program to exit, kill it if it doesn't in time, and reap it.
    fn stop(mut self, name: &str) {
        signal(self.child.id(), libc::SIGTERM);
        let deadline = Instant::now() + STOP_TIMEOUT;
        while Instant::now() < deadline {
            if !matches!(self.child.try_wait(), Ok(None)) {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        warn!("{name} did not exit, killing it");
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The delay before a helper program that exited after running for `ran`
/// is started again, given the delay before its last start.
fn next_delay(delay: Duration, ran: Duration) -> Duration {
    if ran < SHORT_RUN {
        (delay * 2).min(MAX_HELPER_RESTART_DELAY)
    } else {
        RESTART_DELAY
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
    /// The `ibus-ui-gtk3` this supervisor started.
    panel: Option<Started>,
    /// The `ibus-x11` this supervisor started.
    xim: Option<Started>,
    next_daemon_start: Instant,
    daemon_delay: Duration,
    next_panel_start: Instant,
    panel_delay: Duration,
    next_xim_start: Instant,
    xim_delay: Duration,
    /// sway's active outputs; `None` until first read.
    active_outputs: Option<BTreeSet<ActiveOutput>>,
    /// When to start the panel again, after outputs appeared.
    panel_restart: Option<Instant>,
    /// When to check next for panels this supervisor didn't start.
    next_panel_check: Instant,
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
            panel: None,
            xim: None,
            next_daemon_start: now,
            daemon_delay: RESTART_DELAY,
            next_panel_start: now,
            panel_delay: RESTART_DELAY,
            next_xim_start: now,
            xim_delay: RESTART_DELAY,
            active_outputs: None,
            panel_restart: None,
            next_panel_check: now,
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
            if let Some(panel) = &mut self.panel {
                if panel.exited("ibus-ui-gtk3") {
                    self.panel_delay = next_delay(self.panel_delay, panel.at.elapsed());
                    self.next_panel_start = panel.at + self.panel_delay;
                    self.panel = None;
                }
            }
            if let Some(xim) = &mut self.xim {
                if xim.exited("ibus-x11") {
                    self.xim_delay = next_delay(self.xim_delay, xim.at.elapsed());
                    self.next_xim_start = xim.at + self.xim_delay;
                    self.xim = None;
                }
            }
            self.keep_daemon();
            self.recover_panel();
            self.keep_panel();
            self.stop_second_panels();
            self.preload_engines();
            self.select_engine();
            self.keep_xim();
        }
    }

    /// Wait for a change of the sources, keyboards or outputs, or for the
    /// next check.
    fn follow_changes(&mut self) {
        let shared = self.shared.clone();
        let state = shared.state.lock().unwrap();
        let (mut state, _) = shared
            .changed
            .wait_timeout_while(state, POLL_INTERVAL, |s| {
                s.sources.is_none()
                    && !s.keyboards_changed
                    && !s.outputs_changed
                    && s.global_engine.is_none()
            })
            .unwrap();
        let sources = state.sources.take();
        let keyboards_changed = std::mem::take(&mut state.keyboards_changed);
        let outputs_changed = std::mem::take(&mut state.outputs_changed);
        let global_engine = state.global_engine.take();
        drop(state);
        if outputs_changed {
            self.follow_outputs();
        }
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
                // Read again once a sway is back.
                Err(e) if sway::is_gone(&e) => debug!("{e}"),
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

    /// Note the outputs that appeared or were turned on since the last
    /// look, and have the panel started again once they settle.
    fn follow_outputs(&mut self) {
        let active = match utils::get_outputs_json() {
            Ok(outputs) => active_outputs(&outputs),
            // Read again once a sway is back.
            Err(e) if sway::is_gone(&e) => return debug!("{e}"),
            Err(e) => return warn!("Cannot read sway's outputs: {e}"),
        };
        if let Some(known) = &self.active_outputs {
            let new: Vec<&str> = active
                .difference(known)
                .map(|output| output.name.as_str())
                .collect();
            if !new.is_empty() {
                info!("Output turned on: {}", new.join(", "));
                self.panel_restart = Some(Instant::now() + OUTPUT_SETTLE);
            }
        }
        self.active_outputs = Some(active);
    }

    /// Stop the panel once outputs that appeared have settled, for
    /// `keep_panel` to start a new one right away.
    fn recover_panel(&mut self) {
        match self.panel_restart {
            Some(at) if Instant::now() >= at => (),
            _ => return,
        }
        self.panel_restart = None;
        if let Some(panel) = self.panel.take() {
            info!("Starting ibus-ui-gtk3 again, for the new outputs");
            panel.stop("ibus-ui-gtk3");
            self.panel_delay = RESTART_DELAY;
            self.next_panel_start = Instant::now();
        }
    }

    /// Make sure an `ibus-ui-gtk3` serves Wayland programs, once an
    /// `ibus-daemon` is connected.
    fn keep_panel(&mut self) {
        let now = Instant::now();
        if self.bus.is_none() || self.panel.is_some() || now < self.next_panel_start {
            return;
        }
        self.next_panel_start = now + RESTART_DELAY;
        let Some(programs) = Programs::find() else {
            return;
        };
        stop_other_panels();
        match Command::new(&programs.panel)
            .arg(WAYLAND_IM_OPTION)
            .stdin(Stdio::null())
            .spawn()
        {
            Ok(child) => {
                info!("Started ibus-ui-gtk3 ({})", child.id());
                self.panel = Some(Started {
                    child,
                    at: Instant::now(),
                });
            }
            Err(e) => warn!("Cannot start {}: {e}", programs.panel.display()),
        }
        self.next_panel_check = Instant::now() + PANEL_CHECK_INTERVAL;
    }

    /// Start the panel again, alone, when another program started one on
    /// the session's display while this supervisor's own runs: `ibus
    /// restart`, and the panel's own Restart item, which runs it, start one
    /// of their own a few seconds after they stop `ibus-daemon`, by which
    /// time this supervisor has started its own again. The panel of IBus
    /// 1.5.34 crashes when a second one on its bus exits, so this
    /// supervisor's is stopped first, and `keep_panel` stops the others
    /// before it starts a new one right away.
    fn stop_second_panels(&mut self) {
        let now = Instant::now();
        let Some(panel) = &self.panel else {
            return;
        };
        if now < self.next_panel_check {
            return;
        }
        self.next_panel_check = now + PANEL_CHECK_INTERVAL;
        let ours = panel.child.id();
        let others: Vec<String> = panels_on_display()
            .into_iter()
            .filter(|pid| *pid != ours)
            .map(|pid| pid.to_string())
            .collect();
        if others.is_empty() {
            return;
        }
        info!(
            "ibus-ui-gtk3 ({}) started next to this daemon's, starting its own again alone",
            others.join(", ")
        );
        if let Some(panel) = self.panel.take() {
            panel.stop("ibus-ui-gtk3");
        }
        self.panel_delay = RESTART_DELAY;
        self.next_panel_start = Instant::now();
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
        // An ibus-daemon that runs a panel of its own would start it again,
        // next to the one this supervisor starts: it is replaced below
        // (or, if this supervisor already started one, its own replaces it).
        let panel_daemons = daemons_running_a_panel();
        if let Some(bus) = connect(&programs).filter(|_| panel_daemons.is_empty()) {
            info!("Connected to the input method");
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
                        shared.update(|state| state.global_engine = Some(engine));
                    }
                },
            );
            self.bus = Some(bus);
            self.daemon_delay = RESTART_DELAY;
            return;
        }
        if self.daemon.is_none() && !panel_daemons.is_empty() {
            let pids: Vec<String> = panel_daemons.iter().map(u32::to_string).collect();
            info!(
                "ibus-daemon ({}) runs a panel of its own, replacing it",
                pids.join(", ")
            );
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
        // ibus-daemon exits with its parent. A shell waits for it instead, so
        // that a restart of this daemon leaves the input method running for
        // the next instance to use. --replace takes over from an ibus-daemon
        // that holds the session's address but no longer answers. The panel
        // is started by `keep_panel`, so that it can be started again.
        match Command::new("/bin/sh")
            .arg("-c")
            .arg("\"$0\" \"$@\"; exit $?")
            .arg(&programs.daemon)
            .arg("--replace")
            .arg("--panel")
            .arg("disable")
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

/// Whether the session's trawl resources turn the input method on. trawld
/// answers an unset resource with an empty value; without a reachable
/// trawld the input method stays off.
fn enabled_by_resource() -> bool {
    let reply = gio::bus_get_sync(BusType::Session, Cancellable::NONE).and_then(|bus| {
        bus.call_sync(
            Some("org.regolith.Trawl"),
            "/org/regolith/Trawl",
            "org.regolith.trawl1",
            "GetResource",
            Some(&(ENABLE_RESOURCE,).to_variant()),
            None,
            DBusCallFlags::NONE,
            5000,
            Cancellable::NONE,
        )
    });
    match reply {
        Ok(reply) => reply
            .get::<(String,)>()
            .is_some_and(|(value,)| is_true(&value)),
        Err(e) => {
            info!("Cannot read {ENABLE_RESOURCE} from trawld: {e}");
            false
        }
    }
}

/// Whether a resource value reads as true.
fn is_true(value: &str) -> bool {
    value.trim().eq_ignore_ascii_case("true")
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
    !own_processes(name).is_empty()
}

/// The live processes of this user that run the program `name`.
fn own_processes(name: &str) -> Vec<u32> {
    let Ok(me) = fs::metadata("/proc/self") else {
        return Vec::new();
    };
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        .filter(|pid| {
            let path = PathBuf::from(format!("/proc/{pid}"));
            fs::metadata(&path).is_ok_and(|m| m.uid() == me.uid())
                && fs::read_to_string(path.join("comm")).is_ok_and(|c| c.trim_end() == name)
                && alive(*pid)
        })
        .collect()
}

/// Whether process `pid` runs: an exited process waiting to be reaped
/// serves nothing.
fn alive(pid: u32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        stat.rsplit_once(") ")
            .is_some_and(|(_, rest)| !rest.starts_with(['Z', 'X']))
    })
}

fn signal(pid: u32, signal: libc::c_int) {
    if let Ok(pid) = libc::pid_t::try_from(pid) {
        // SAFETY: kill() only sends a signal; it touches no memory.
        unsafe { libc::kill(pid, signal) };
    }
}

/// The `ibus-daemon`s of this user on the session's display that run a
/// panel of their own.
fn daemons_running_a_panel() -> Vec<u32> {
    let display = env::var_os("WAYLAND_DISPLAY");
    own_processes("ibus-daemon")
        .into_iter()
        .filter(|pid| {
            let cmdline = read_proc(*pid, "cmdline");
            on_display(&read_proc(*pid, "environ"), display.as_deref())
                && runs_own_panel(cmdline.split(|b| *b == 0).skip(1))
        })
        .collect()
}

/// Whether an `ibus-daemon` with these arguments starts a panel: it does
/// unless told `--panel disable`.
fn runs_own_panel<'a>(args: impl IntoIterator<Item = &'a [u8]>) -> bool {
    let mut panel: Option<&[u8]> = None;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == b"--panel" || arg == b"-p" {
            panel = args.next();
        } else if let Some(value) = arg.strip_prefix(b"--panel=") {
            panel = Some(value);
        } else if let Some(value) = arg.strip_prefix(b"-p").filter(|v| !v.is_empty()) {
            panel = Some(value);
        }
    }
    panel != Some(b"disable".as_slice())
}

/// The contents of `/proc/<pid>/<file>`, empty if unreadable.
fn read_proc(pid: u32, file: &str) -> Vec<u8> {
    fs::read(format!("/proc/{pid}/{file}")).unwrap_or_default()
}

/// Whether a process with this environment (`/proc/<pid>/environ`) runs on
/// the Wayland display `display`.
fn on_display(environ: &[u8], display: Option<&OsStr>) -> bool {
    let Some(display) = display else {
        return false;
    };
    environ
        .split(|b| *b == 0)
        .find_map(|var| var.strip_prefix(b"WAYLAND_DISPLAY="))
        .is_some_and(|d| OsStr::from_bytes(d) == display)
}

/// The panels of this user on the session's display.
fn panels_on_display() -> Vec<u32> {
    let display = env::var_os("WAYLAND_DISPLAY");
    own_processes("ibus-ui-gtk3")
        .into_iter()
        .filter(|pid| on_display(&read_proc(*pid, "environ"), display.as_deref()))
        .collect()
}

/// Stop the panels on this session's display, which this supervisor didn't
/// start: the one an earlier version of this daemon had `ibus-daemon`
/// start, one started by hand or by `ibus restart`. Only one input method
/// can serve sway's seat, a panel that loses its D-Bus name to a new one
/// keeps holding the seat's input method, and each shows its own tray icon.
/// Returns once they have exited.
fn stop_other_panels() {
    let pids = panels_on_display();
    for pid in &pids {
        info!("Stopping ibus-ui-gtk3 ({pid}), which this daemon didn't start");
        signal(*pid, libc::SIGTERM);
    }
    let deadline = Instant::now() + STOP_TIMEOUT;
    while pids.iter().any(|pid| alive(*pid)) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    let killed: Vec<u32> = pids.into_iter().filter(|pid| alive(*pid)).collect();
    for pid in &killed {
        warn!("ibus-ui-gtk3 ({pid}) did not exit, killing it");
        signal(*pid, libc::SIGKILL);
    }
    let deadline = Instant::now() + STOP_TIMEOUT;
    while killed.iter().any(|pid| alive(*pid)) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
}

/// An active output, which has a `wl_output` global. sway gives each output
/// it creates a new ID, which it keeps while the output is turned off and on.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct ActiveOutput {
    name: String,
    id: Option<u64>,
}

/// The active outputs.
fn active_outputs(outputs: &[Value]) -> BTreeSet<ActiveOutput> {
    outputs
        .iter()
        .filter(|o| o["active"] == true)
        .filter_map(|o| {
            Some(ActiveOutput {
                name: o["name"].as_str()?.to_string(),
                id: o["id"].as_u64(),
            })
        })
        .collect()
}

/// The engine of each counted source.
fn engines_for(sources: &[EffectiveSource]) -> Vec<String> {
    let mut installed = None;
    sources
        .iter()
        .map(|source| match &source.engine {
            Some(engine) => engine.clone(),
            None => xkb_engine(
                installed.get_or_insert_with(ibus_engines),
                &source.layout,
                &source.variant,
            ),
        })
        .collect()
}

/// IBus's XKB engine for a layout and variant: the first by name of the
/// installed `xkb:` engines with that layout and variant.
fn xkb_engine(
    installed: &HashMap<String, (String, String)>,
    layout: &str,
    variant: &str,
) -> String {
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

/// Tell the supervisor whenever a keyboard or an output changes: sway sends
/// an input event for every layout switch and keymap change, and an output
/// event for every output added, removed or reconfigured. Each sway the
/// daemon reaches is followed in turn.
fn watch_sway(shared: &Shared) {
    loop {
        match follow_sway_events(shared) {
            // The owner of the handlers logs the loss and looks for sway.
            Err(e) if sway::is_gone(&e) => debug!("sway input and output events: {e}"),
            Err(e) => warn!("sway input and output events: {e}"),
            Ok(()) => (),
        }
        thread::sleep(RESTART_DELAY);
    }
}

fn follow_sway_events(shared: &Shared) -> Fallible<()> {
    let sway = sway::current().ok_or_else(sway::gone)?;
    let mut stream = sway.subscribe(r#"["input","output"]"#)?;
    // The keyboards and outputs may have changed before the subscription,
    // or belong to a sway that replaced the one followed so far.
    shared.update(|state| {
        state.keyboards_changed = true;
        state.outputs_changed = true;
    });
    loop {
        let (kind, payload) = match utils::receive_message(&mut stream) {
            Ok(message) => message,
            Err(e) => {
                sway.lose(&e);
                return Err(e);
            }
        };
        if kind == OUTPUT_EVENT {
            shared.update(|state| state.outputs_changed = true);
            continue;
        }
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
    use super::{active_layout, is_true, on_display, runs_own_panel, xkb_engine, DEFAULT_ENGINE};
    use serde_json::json;
    use std::collections::HashMap;
    use std::ffi::OsStr;

    fn args(cmdline: &str) -> Vec<&[u8]> {
        cmdline.split(' ').map(str::as_bytes).collect()
    }

    #[test]
    fn only_an_ibus_daemon_told_panel_disable_runs_no_panel() {
        assert!(!runs_own_panel(args("--replace --panel disable")));
        assert!(!runs_own_panel(args("--xim --panel=disable")));
        assert!(!runs_own_panel(args("-r -p disable -x")));
        assert!(!runs_own_panel(args("-pdisable")));
        // The last one counts, as for GLib's option parser.
        assert!(!runs_own_panel(args("--panel default --panel disable")));
        assert!(runs_own_panel(args("-drx")));
        assert!(runs_own_panel(args("")));
        assert!(runs_own_panel(args("--panel default")));
        // As an earlier version of this daemon started it.
        let panel: [&[u8]; 3] = [
            b"--replace",
            b"--panel",
            b"/usr/libexec/ibus-ui-gtk3 --enable-wayland-im",
        ];
        assert!(runs_own_panel(panel));
        assert!(runs_own_panel(args("--panel disable --panel default")));
        // An option without its value leaves the default panel.
        assert!(runs_own_panel(args("--panel")));
    }

    #[test]
    fn a_process_is_on_the_display_its_environment_names() {
        let environ = b"HOME=/home/u\0WAYLAND_DISPLAY=wayland-1\0DISPLAY=:0\0";
        assert!(on_display(environ, Some(OsStr::new("wayland-1"))));
        assert!(!on_display(environ, Some(OsStr::new("wayland-10"))));
        assert!(!on_display(environ, None));
        // One of an X11 session, or one that can't be read.
        assert!(!on_display(
            b"HOME=/home/u\0DISPLAY=:0\0",
            Some(OsStr::new("wayland-1"))
        ));
        assert!(!on_display(b"", Some(OsStr::new("wayland-1"))));
        assert!(!on_display(
            b"XWAYLAND_DISPLAY=wayland-1\0",
            Some(OsStr::new("wayland-1"))
        ));
    }

    #[test]
    fn only_a_true_resource_turns_the_input_method_on() {
        assert!(is_true("true"));
        assert!(is_true("True\n"));
        assert!(!is_true("false"));
        assert!(!is_true(""));
        assert!(!is_true("1"));
    }

    #[test]
    fn xkb_sources_select_the_xkb_engine_of_their_layout() {
        let installed: HashMap<String, (String, String)> = [
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

    #[test]
    fn only_active_outputs_count() {
        let outputs = vec![
            json!({"name": "eDP-1", "active": true}),
            json!({"name": "DP-1", "active": false}),
            json!({"name": "HDMI-A-1", "active": true}),
            json!({"active": true}),
        ];
        let names: Vec<String> = super::active_outputs(&outputs)
            .into_iter()
            .map(|o| o.name)
            .collect();
        assert_eq!(names, ["HDMI-A-1", "eDP-1"]);
    }

    #[test]
    fn an_output_created_again_under_its_name_is_a_new_output() {
        let before = super::active_outputs(&[json!({"name": "eDP-1", "id": 3, "active": true})]);
        let again = super::active_outputs(&[json!({"name": "eDP-1", "id": 9, "active": true})]);
        let same = super::active_outputs(&[json!({"name": "eDP-1", "id": 3, "active": true})]);
        assert_eq!(again.difference(&before).count(), 1);
        assert_eq!(same.difference(&before).count(), 0);
    }
}
