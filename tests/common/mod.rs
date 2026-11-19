//! Shared harness for the integration tests: run the real daemon against a
//! fake sway IPC socket and a keyfile GSettings backend, and record which
//! commands it sends to sway.

#![allow(dead_code)]

use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const MAGIC: &[u8; 6] = b"i3-ipc";
const RUN_COMMAND: u32 = 0;
const SUBSCRIBE: u32 = 2;
const GET_OUTPUTS: u32 = 3;
const GET_TREE: u32 = 4;
const SEND_TICK: u32 = 10;
const GET_INPUTS: u32 = 100;
const WORKSPACE_EVENT: u32 = 0x8000_0000;
const OUTPUT_EVENT: u32 = 0x8000_0001;
const WINDOW_EVENT: u32 = 0x8000_0003;
const TICK_EVENT: u32 = 0x8000_0007;
const INPUT_EVENT: u32 = 0x8000_0015;

/// What sway reports for the attached devices: a keyboard, and a clickpad
/// whose libinput default is software buttons in areas.
const INPUTS: &str = r#"[
  {"identifier": "1:1:AT_Translated_Set_2_keyboard", "name": "AT Translated Set 2 keyboard",
   "vendor": 1, "product": 1, "type": "keyboard",
   "xkb_active_layout_name": "English (US)", "xkb_layout_names": ["English (US)"],
   "xkb_active_layout_index": 0, "libinput": {"send_events": "enabled"}},
  {"identifier": "1267:12445:ELAN0001:00_04F3:309D_Touchpad", "name": "ELAN0001:00 04F3:309D Touchpad",
   "vendor": 1267, "product": 12445, "type": "touchpad",
   "libinput": {"send_events": "enabled", "tap": "disabled", "tap_button_map": "lrm",
                "tap_drag": "enabled", "tap_drag_lock": "disabled", "accel_speed": 0.0,
                "accel_profile": "adaptive", "natural_scroll": "disabled", "left_handed": "disabled",
                "click_method": "button_areas", "middle_emulation": "disabled",
                "scroll_method": "two_finger", "dwt": "enabled"}}
]"#;

/// What sway reports for the outputs: a laptop's panel.
const OUTPUTS: &str = r#"[{"name": "eDP-1", "active": true, "power": true}]"#;

/// A minimal sway IPC server that records every command it is asked to run.
///
/// It keeps the devices' libinput settings as sway does: `input` commands
/// change them, and each device a command changes is reported to the
/// subscribed clients in a "libinput_config" input event.
///
/// It also keeps the keyboards' layouts as sway does: `xkb_layout`,
/// `xkb_variant` and `xkb_options` commands compile a new keymap, which
/// starts on the first layout, and `xkb_switch_layout` switches layouts.
/// Each change is sent to the subscribed clients as an input event, and
/// `send_tick` requests come back as tick events.
pub struct FakeSway {
    commands: Arc<Mutex<Vec<String>>>,
    subscribed: Arc<Mutex<bool>>,
    state: Arc<Mutex<State>>,
    socket: PathBuf,
    /// Set once the fake sway has exited (see [`FakeSway::stop`]).
    stopped: Arc<AtomicBool>,
}

struct State {
    inputs: Value,
    outputs: Value,
    /// Subscribed clients, with the events they subscribed to.
    subscribers: Vec<(UnixStream, Vec<String>)>,
    /// Every connection a client made, to close when sway exits.
    connections: Vec<UnixStream>,
    /// How many connections are open: accepted, and their end not read.
    open: usize,
    /// Whether requests go unanswered, as with a sway that hangs.
    silent: bool,
    /// How many times a client asked for the tree.
    tree_requests: usize,
    /// How many times a client asked for the outputs.
    output_requests: usize,
}

/// The libinput setting, as GET_INPUTS names it, that a sway `input`
/// subcommand sets.
fn libinput_setting(subcommand: &str) -> Option<&'static str> {
    Some(match subcommand {
        "pointer_accel" => "accel_speed",
        "accel_profile" => "accel_profile",
        "natural_scroll" => "natural_scroll",
        "left_handed" => "left_handed",
        "tap" => "tap",
        "drag" => "tap_drag",
        "drag_lock" => "tap_drag_lock",
        "dwt" => "dwt",
        "middle_emulation" => "middle_emulation",
        "events" => "send_events",
        "scroll_method" => "scroll_method",
        "click_method" => "click_method",
        _ => return None,
    })
}

impl State {
    /// Send an event to the clients subscribed to `name`.
    fn emit(&mut self, kind: u32, name: &str, payload: &str) {
        let msg = message(kind, payload);
        self.subscribers.retain_mut(|(stream, events)| {
            !events.iter().any(|e| e == name) || stream.write_all(&msg).is_ok()
        });
    }

    fn keyboards(&mut self) -> impl Iterator<Item = &mut Value> {
        self.inputs
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .filter(|i| i["type"] == "keyboard")
    }

    fn input_event(&mut self, change: &str, input: &Value) {
        let event = json!({"change": change, "input": input}).to_string();
        self.emit(INPUT_EVENT, "input", &event);
    }

    /// Run the libinput part of an `input` command, as sway does: set the
    /// setting on every matching device that has it, and report the devices
    /// it changed. Like sway, a device lists only the settings it supports.
    fn run_libinput(&mut self, cmd: &str) {
        let Some(rest) = cmd.strip_prefix("input ") else {
            return;
        };
        let Some((target, rest)) = rest.rsplit_once(' ').and_then(|(head, value)| {
            let (target, subcommand) = head.rsplit_once(' ')?;
            Some((target, (subcommand, value)))
        }) else {
            return;
        };
        let (subcommand, value) = rest;
        let Some(setting) = libinput_setting(subcommand) else {
            return;
        };
        let value = match setting {
            // sway keeps the speed as a float.
            "accel_speed" => json!(value.parse::<f32>().unwrap() as f64),
            _ => json!(value),
        };
        let mut changed = Vec::new();
        for input in self.inputs.as_array_mut().unwrap() {
            let matches = match target.strip_prefix("type:") {
                Some(input_type) => input["type"] == input_type,
                None => input["identifier"] == target.trim_matches('"'),
            };
            let libinput = &mut input["libinput"];
            if matches && libinput.get(setting).is_some() && libinput[setting] != value {
                libinput[setting] = value.clone();
                changed.push(input.clone());
            }
        }
        for input in changed {
            self.input_event("libinput_config", &input);
        }
    }

    /// Put every keyboard with more than `index` layouts on `index`.
    fn switch_layout(&mut self, index: usize) {
        let mut changed = Vec::new();
        for kbd in self.keyboards() {
            let names = kbd["xkb_layout_names"].as_array().unwrap().clone();
            if index < names.len() && kbd["xkb_active_layout_index"] != index {
                kbd["xkb_active_layout_index"] = index.into();
                kbd["xkb_active_layout_name"] = names[index].clone();
                changed.push(kbd.clone());
            }
        }
        for kbd in changed {
            let event = json!({"change": "xkb_layout", "input": kbd}).to_string();
            self.emit(INPUT_EVENT, "input", &event);
        }
    }

    /// Run the keyboard part of a sway command.
    fn run(&mut self, cmd: &str) {
        self.run_libinput(cmd);
        let Some(arg) = cmd.strip_prefix("input type:keyboard ") else {
            return;
        };
        if let Some(index) = arg.strip_prefix("xkb_switch_layout ") {
            self.switch_layout(index.parse().unwrap());
            return;
        }
        let layouts = arg
            .strip_prefix("xkb_layout '")
            .map(|l| l.trim_end_matches('\''));
        if layouts.is_none() && !arg.starts_with("xkb_variant ") && !arg.starts_with("xkb_options ")
        {
            return;
        }
        let mut changed = Vec::new();
        for kbd in self.keyboards() {
            if let Some(layouts) = layouts {
                kbd["xkb_layout_names"] = layouts.split(',').collect::<Vec<_>>().into();
            }
            kbd["xkb_active_layout_index"] = 0.into();
            kbd["xkb_active_layout_name"] = kbd["xkb_layout_names"][0].clone();
            changed.push(kbd.clone());
        }
        for kbd in changed {
            let event = json!({"change": "xkb_keymap", "input": kbd}).to_string();
            self.emit(INPUT_EVENT, "input", &event);
        }
    }
}

impl FakeSway {
    fn start(socket: &Path, inputs: &'static str) -> FakeSway {
        let listener = UnixListener::bind(socket).unwrap();
        let commands = Arc::new(Mutex::new(Vec::new()));
        let subscribed = Arc::new(Mutex::new(false));
        let stopped = Arc::new(AtomicBool::new(false));
        let state = Arc::new(Mutex::new(State {
            inputs: serde_json::from_str(inputs).unwrap(),
            outputs: serde_json::from_str(OUTPUTS).unwrap(),
            subscribers: Vec::new(),
            connections: Vec::new(),
            open: 0,
            silent: false,
            tree_requests: 0,
            output_requests: 0,
        }));
        let (recorded, flag, shared) = (commands.clone(), subscribed.clone(), state.clone());
        let exited = stopped.clone();
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                if exited.load(Ordering::SeqCst) {
                    return;
                }
                {
                    let mut state = shared.lock().unwrap();
                    state.open += 1;
                    if let Ok(registered) = stream.try_clone() {
                        state.connections.push(registered);
                    }
                }
                let (recorded, flag, shared) = (recorded.clone(), flag.clone(), shared.clone());
                thread::spawn(move || {
                    serve(stream, recorded, flag, shared.clone());
                    shared.lock().unwrap().open -= 1;
                });
            }
        });
        FakeSway {
            commands,
            subscribed,
            state,
            socket: socket.to_path_buf(),
            stopped,
        }
    }

    /// A fresh fake sway listening on `socket`, as a sway started (again) at
    /// that path.
    pub fn start_at(socket: &Path) -> FakeSway {
        FakeSway::start(socket, INPUTS)
    }

    /// Exit, as sway does: every connection is closed and the socket goes
    /// away.
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        // Wake the listener up, so that it sees it has to stop.
        let _ = UnixStream::connect(&self.socket);
        let _ = fs::remove_file(&self.socket);
        let mut state = self.state.lock().unwrap();
        for stream in state.connections.drain(..) {
            let _ = stream.shutdown(Shutdown::Both);
        }
        state.subscribers.clear();
    }

    /// Close the connections clients subscribed to events on, and only
    /// those, as sway does with a subscriber it gives up on.
    pub fn drop_subscribers(&self) {
        let mut state = self.state.lock().unwrap();
        for (stream, _) in state.subscribers.drain(..) {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }

    /// Stop answering requests, or answer them again, as a sway that hangs
    /// and recovers. Requests made in the meantime are never answered.
    pub fn set_silent(&self, silent: bool) {
        self.state.lock().unwrap().silent = silent;
    }

    /// How many connections clients have open to this sway: the ones whose
    /// end has not been read yet.
    pub fn open_connections(&self) -> usize {
        self.state.lock().unwrap().open
    }

    /// How many clients are subscribed to events.
    pub fn subscribers(&self) -> usize {
        self.state.lock().unwrap().subscribers.len()
    }

    /// Send a tick event to the daemon, as `swaymsg -t send_tick` does. The
    /// Regolith sway config sends `{"status": "reload_done"}` on every
    /// config load.
    pub fn send_tick(&self, payload: &str) {
        let event = json!({"first": false, "payload": payload}).to_string();
        let mut state = self.state.lock().unwrap();
        assert!(!state.subscribers.is_empty(), "daemon has not subscribed");
        state.emit(TICK_EVENT, "tick", &event);
    }

    /// Change the outputs to `outputs` (GET_OUTPUTS JSON), as plugging,
    /// unplugging or configuring a monitor does, and send the output event
    /// sway sends for it.
    pub fn set_outputs(&self, outputs: &str) {
        let mut state = self.state.lock().unwrap();
        state.outputs = serde_json::from_str(outputs).unwrap();
        state.emit(OUTPUT_EVENT, "output", r#"{"change": "unspecified"}"#);
    }

    /// Send the workspace "reload" event sway sends once it has loaded its
    /// configuration again.
    pub fn send_workspace_reload(&self) {
        let event = json!({"change": "reload", "current": null, "old": null}).to_string();
        let mut state = self.state.lock().unwrap();
        state.emit(WORKSPACE_EVENT, "workspace", &event);
    }

    /// Run an `input` command the way the user does with swaymsg, or the
    /// way a reload resets a device: in sway, not from the daemon.
    pub fn user_command(&self, cmd: &str) {
        self.state.lock().unwrap().run(cmd);
    }

    /// Send an input event about `input`, as sway does for a device that is
    /// added or removed (`change` "added", "removed") or was configured
    /// ("libinput_config"). The devices sway reports don't change.
    pub fn send_input_event(&self, change: &str, input: Value) {
        self.state.lock().unwrap().input_event(change, &input);
    }

    /// Plug in a device, as GET_INPUTS reports it.
    pub fn add_device(&self, input: Value) {
        let mut state = self.state.lock().unwrap();
        state.inputs.as_array_mut().unwrap().push(input.clone());
        state.input_event("added", &input);
    }

    /// Unplug the device `identifier`.
    pub fn remove_device(&self, identifier: &str) {
        let mut state = self.state.lock().unwrap();
        let inputs = state.inputs.as_array_mut().unwrap();
        let index = inputs
            .iter()
            .position(|i| i["identifier"] == identifier)
            .unwrap();
        let input = inputs.remove(index);
        state.input_event("removed", &input);
    }

    /// The libinput `setting` of device `identifier`, as sway reports it.
    pub fn libinput(&self, identifier: &str, setting: &str) -> Value {
        let state = self.state.lock().unwrap();
        let inputs = state.inputs.as_array().unwrap();
        let input = inputs.iter().find(|i| i["identifier"] == identifier);
        input.unwrap()["libinput"][setting].clone()
    }

    /// Switch the keyboards' layout the way the user's key binding does:
    /// in sway itself, not through a command from the daemon.
    pub fn user_switch(&self, index: usize) {
        self.state.lock().unwrap().switch_layout(index);
    }

    /// Focus window `id`, or close it, as sway reports it.
    pub fn window_event(&self, change: &str, id: i64) {
        let rect = json!({"x": 0, "y": 0, "width": 640, "height": 480});
        let container = json!({
            "id": id, "name": "foot", "type": "con", "border": "pixel",
            "current_border_width": 2, "layout": "none", "percent": 1.0,
            "rect": rect, "window_rect": rect, "deco_rect": rect, "geometry": rect,
            "urgent": false, "focused": change == "focus", "focus": [], "nodes": [],
            "floating_nodes": [], "sticky": false, "app_id": "foot", "pid": 1000 + id,
        });
        let event = json!({"change": change, "container": container}).to_string();
        self.state
            .lock()
            .unwrap()
            .emit(WINDOW_EVENT, "window", &event);
    }

    /// The first keyboard's layouts and the index of the active one.
    pub fn layouts(&self) -> (Vec<String>, usize) {
        let mut state = self.state.lock().unwrap();
        let kbd = state.keyboards().next().unwrap();
        let names = serde_json::from_value(kbd["xkb_layout_names"].clone()).unwrap();
        (
            names,
            kbd["xkb_active_layout_index"].as_u64().unwrap() as usize,
        )
    }

    /// Wait until the first keyboard is on layout `index` of `layouts`.
    pub fn wait_for_layout(&self, layouts: &[&str], index: usize) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let (names, active) = self.layouts();
            if names == layouts && active == index {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "keyboard on {active} of {names:?}, expected {index} of {layouts:?}; sway got {:?}",
                self.commands()
            );
            thread::sleep(Duration::from_millis(50));
        }
    }

    /// Wait until `n` clients have subscribed to sway's events.
    pub fn wait_for_subscribers(&self, n: usize) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let have = self.state.lock().unwrap().subscribers.len();
            if have >= n {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "{have} clients subscribed to sway's events, expected {n}"
            );
            thread::sleep(Duration::from_millis(50));
        }
    }

    /// Wait until a client has asked for the tree.
    pub fn wait_for_get_tree(&self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.state.lock().unwrap().tree_requests == 0 {
            assert!(Instant::now() < deadline, "no client asked for the tree");
            thread::sleep(Duration::from_millis(50));
        }
    }

    /// Wait until a client has asked for the outputs.
    pub fn wait_for_get_outputs(&self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.state.lock().unwrap().output_requests == 0 {
            assert!(Instant::now() < deadline, "no client asked for the outputs");
            thread::sleep(Duration::from_millis(50));
        }
    }

    pub fn commands(&self) -> Vec<String> {
        self.commands.lock().unwrap().clone()
    }

    /// Wait for a command starting with `prefix`, ignoring the first `skip`.
    pub fn wait_for(&self, skip: usize, prefix: &str) -> Option<String> {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Some(cmd) = self
                .commands()
                .iter()
                .skip(skip)
                .find(|c| c.starts_with(prefix))
            {
                return Some(cmd.clone());
            }
            thread::sleep(Duration::from_millis(50));
        }
        None
    }

    /// Wait until the daemon has applied its settings at startup: it
    /// subscribes to sway's input events only after that.
    pub fn wait_for_startup(&self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !*self.subscribed.lock().unwrap() {
            assert!(
                Instant::now() < deadline,
                "daemon did not finish starting; sway got {:?}",
                self.commands()
            );
            thread::sleep(Duration::from_millis(50));
        }
    }
}

fn serve(
    mut stream: UnixStream,
    recorded: Arc<Mutex<Vec<String>>>,
    subscribed: Arc<Mutex<bool>>,
    state: Arc<Mutex<State>>,
) {
    loop {
        let mut header = [0u8; 14];
        if stream.read_exact(&mut header).is_err() {
            return;
        }
        assert_eq!(&header[..6], MAGIC);
        let len = u32::from_ne_bytes(header[6..10].try_into().unwrap()) as usize;
        let kind = u32::from_ne_bytes(header[10..14].try_into().unwrap());
        let mut payload = vec![0u8; len];
        if stream.read_exact(&mut payload).is_err() {
            return;
        }
        let payload = String::from_utf8(payload).unwrap();
        // Replies are written with the state locked, so that a client never
        // gets the events a command caused before the command's reply
        // (which is what a fence relies on).
        let mut state = state.lock().unwrap();
        if state.silent {
            continue;
        }
        let reply = match kind {
            RUN_COMMAND => {
                recorded.lock().unwrap().push(payload.clone());
                state.run(&payload);
                r#"[{"success": true}]"#.to_string()
            }
            SUBSCRIBE => {
                let events: Vec<String> = serde_json::from_str(&payload).unwrap();
                if let Ok(events_stream) = stream.try_clone() {
                    state.subscribers.push((events_stream, events));
                }
                *subscribed.lock().unwrap() = true;
                r#"{"success": true}"#.to_string()
            }
            SEND_TICK => {
                let event = json!({"first": false, "payload": payload}).to_string();
                state.emit(TICK_EVENT, "tick", &event);
                r#"{"success": true}"#.to_string()
            }
            GET_TREE => {
                state.tree_requests += 1;
                "[]".to_string()
            }
            GET_INPUTS => state.inputs.to_string(),
            GET_OUTPUTS => {
                state.output_requests += 1;
                state.outputs.to_string()
            }
            _ => "[]".to_string(),
        };
        if stream.write_all(&message(kind, &reply)).is_err() {
            return;
        }
    }
}

/// An IPC message: a reply, or an event to a subscribed client.
fn message(kind: u32, payload: &str) -> Vec<u8> {
    let mut msg = Vec::from(&MAGIC[..]);
    msg.extend_from_slice(&(payload.len() as u32).to_ne_bytes());
    msg.extend_from_slice(&kind.to_ne_bytes());
    msg.extend_from_slice(payload.as_bytes());
    msg
}

/// The daemon process, killed when dropped.
pub struct Daemon {
    child: Child,
    dir: PathBuf,
    /// The private D-Bus daemon standing in for IBus's bus and the session
    /// bus, if any.
    bus: Option<Child>,
    /// The resources the stand-in trawld was asked for, in order.
    trawl_calls: Arc<Mutex<Vec<String>>>,
    /// The Wayland display the daemon is told it runs on.
    wayland_display: String,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(bus) = &mut self.bus {
            let _ = bus.kill();
            let _ = bus.wait();
        }
        // The stand-in IBus programs exit once their directory is gone.
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// What trawld knows of the resource `regolith.experimental.ibus`.
#[derive(Default)]
pub enum Trawl {
    /// The resource isn't set: trawld answers with an empty value.
    #[default]
    Unset,
    /// The resource is set to this value.
    Value(&'static str),
    /// trawld answers with an error.
    Fails,
    /// No trawld runs on the session bus.
    Missing,
}

/// How the stand-in IBus is set up for a test.
#[derive(Default)]
pub struct Ibus {
    /// What trawld answers when the daemon asks for the resource.
    pub trawl: Trawl,
    /// Whether a Wayland panel the daemon didn't start already runs, as
    /// one an earlier version of the daemon left does.
    pub foreign_panel: bool,
    /// The arguments of an `ibus-daemon` the daemon didn't start that
    /// already serves the session, if any.
    pub foreign_daemon: Option<&'static str>,
}

impl Ibus {
    /// A user who opted in to IBus.
    pub fn opted_in() -> Ibus {
        Ibus {
            trawl: Trawl::Value("true"),
            ..Ibus::default()
        }
    }
}

/// The stand-in IBus programs. Each logs its start, with its arguments, to
/// `ibus.log`, and runs until it is stopped or the test's directory goes.
/// `ibus address` gives the private bus's address once `ibus-daemon` runs;
/// `ibus-daemon --replace` stops the one running first. A panel started
/// with `FOREIGN` set logs itself as a foreign panel.
const FAKE_IBUS: [(&str, &str); 4] = [
    (
        "ibus-daemon",
        "echo \"daemon $$ $*\" >>\"$D/ibus.log\"\n\
         case \" $* \" in *' --replace '*) [ -f \"$D/daemon.pid\" ] && kill \"$(cat \"$D/daemon.pid\")\" 2>/dev/null\n\
         for _ in $(seq 50); do [ -e \"$D/address\" ] || break; sleep 0.05; done;; esac\n\
         echo $$ >\"$D/daemon.pid\"; cp \"$D/bus-address\" \"$D/address\"\n\
         trap 'echo \"stopped $$\" >>\"$D/ibus.log\"; rm -f \"$D/address\"; exit 0' TERM\n",
    ),
    (
        "ibus",
        "[ \"$1\" = address ] || exit 1\ncat \"$D/address\" 2>/dev/null || echo '(null)'\nexit 0\n",
    ),
    (
        "ibus-ui-gtk3",
        "echo \"${FOREIGN:+foreign }panel $$ $*\" >>\"$D/ibus.log\"\n\
         trap 'echo \"stopped $$\" >>\"$D/ibus.log\"; exit 0' TERM\n",
    ),
    ("ibus-x11", "echo \"xim $$ $*\" >>\"$D/ibus.log\"\n"),
];

impl Daemon {
    /// Whether the daemon process is still running.
    pub fn is_running(&mut self) -> bool {
        self.child.try_wait().unwrap().is_none()
    }

    /// The daemon's process id.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Write `keyfile` as the user's GSettings and start the daemon on a
    /// fresh fake sway.
    pub fn start(name: &str, keyfile: &str) -> (FakeSway, Daemon) {
        Daemon::start_with_inputs(name, keyfile, INPUTS)
    }

    /// Like `start`, with sway reporting `inputs` (GET_INPUTS JSON).
    pub fn start_with_inputs(
        name: &str,
        keyfile: &str,
        inputs: &'static str,
    ) -> (FakeSway, Daemon) {
        let (sway, daemon) = Daemon::spawn(name, keyfile, Some(inputs), None);
        (sway.unwrap(), daemon)
    }

    /// Like `start`, in a Wayland session with stand-in IBus programs in
    /// `PATH` and a private bus for them, so that the session's own IBus,
    /// if any, is left alone.
    pub fn start_with_ibus(name: &str, keyfile: &str, ibus: Ibus) -> (FakeSway, Daemon) {
        let (sway, daemon) = Daemon::spawn(name, keyfile, Some(INPUTS), Some(ibus));
        (sway.unwrap(), daemon)
    }

    /// Like `start`, with no sway listening yet at the daemon's `SWAYSOCK`
    /// (see [`Daemon::socket`]).
    pub fn start_without_sway(name: &str, keyfile: &str) -> Daemon {
        Daemon::spawn(name, keyfile, None, None).1
    }

    /// The socket the daemon's `SWAYSOCK` names.
    pub fn socket(&self) -> PathBuf {
        self.dir.join("sway.sock")
    }

    fn spawn(
        name: &str,
        keyfile: &str,
        inputs: Option<&'static str>,
        ibus: Option<Ibus>,
    ) -> (Option<FakeSway>, Daemon) {
        let dir = std::env::temp_dir().join(format!("inputd-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let settings_dir = dir.join("config/glib-2.0/settings");
        fs::create_dir_all(&settings_dir).unwrap();
        fs::write(settings_dir.join("keyfile"), keyfile).unwrap();

        let socket = dir.join("sway.sock");
        let sway = inputs.map(|inputs| FakeSway::start(&socket, inputs));
        let mut command = Command::new(env!("CARGO_BIN_EXE_regolith-inputd"));
        command
            .env("SWAYSOCK", &socket)
            .env("GSETTINGS_BACKEND", "keyfile")
            .env("XDG_CONFIG_HOME", dir.join("config"))
            .env("IBUS_COMPONENT_PATH", dir.join("ibus"))
            .env("RUST_LOG", "info")
            .env_remove("DISPLAY");
        let trawl_calls = Arc::new(Mutex::new(Vec::new()));
        let bus = match ibus {
            // Without a Wayland display the daemon starts no input method, so
            // tests run from a desktop session leave its IBus alone. Without
            // a session bus it can't learn the session's own sway from the
            // user manager, and leaves that sway alone too.
            None => {
                command
                    .env_remove("WAYLAND_DISPLAY")
                    .env("DBUS_SESSION_BUS_ADDRESS", "unix:path=/nonexistent");
                None
            }
            Some(ibus) => Some(fake_ibus(&dir, name, ibus, &trawl_calls, &mut command)),
        };
        let child = command.spawn().unwrap();
        let daemon = Daemon {
            child,
            dir,
            bus,
            trawl_calls,
            wayland_display: test_display(name),
        };
        (sway, daemon)
    }

    /// The resources the daemon asked trawld for, in order.
    pub fn trawl_calls(&self) -> Vec<String> {
        self.trawl_calls.lock().unwrap().clone()
    }

    /// Wait until the daemon asked trawld for a resource.
    pub fn wait_for_trawl(&self) -> Vec<String> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let calls = self.trawl_calls();
            if !calls.is_empty() {
                return calls;
            }
            assert!(Instant::now() < deadline, "the daemon did not ask trawld");
            thread::sleep(Duration::from_millis(50));
        }
    }

    /// What the stand-in IBus programs logged, one line per event.
    pub fn ibus_log(&self) -> Vec<String> {
        fs::read_to_string(self.dir.join("ibus.log"))
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect()
    }

    /// The process IDs of the Wayland panels started so far, in order.
    pub fn panels_started(&self) -> Vec<u32> {
        self.ibus_log()
            .iter()
            .filter_map(|line| line.strip_prefix("panel "))
            .map(|rest| {
                assert!(
                    rest.ends_with(" --enable-wayland-im"),
                    "panel started without the Wayland input method: {rest}"
                );
                rest.split(' ').next().unwrap().parse().unwrap()
            })
            .collect()
    }

    /// The process IDs of the programs that were asked to stop, in order.
    pub fn stopped(&self) -> Vec<u32> {
        self.ibus_log()
            .iter()
            .filter_map(|line| line.strip_prefix("stopped "))
            .map(|pid| pid.parse().unwrap())
            .collect()
    }

    /// Start a Wayland panel on the daemon's display, as `ibus restart` or
    /// the user does, with `args`; returns its process ID once it runs.
    pub fn start_foreign_panel(&self, args: &[&str]) -> u32 {
        let mut panel = Command::new(self.dir.join("ibus-bin/ibus-ui-gtk3"))
            .args(args)
            .env("FOREIGN", "1")
            .env("WAYLAND_DISPLAY", &self.wayland_display)
            .spawn()
            .unwrap();
        let pid = panel.id();
        // Reaped once the daemon stops it.
        thread::spawn(move || panel.wait());
        let started = format!("foreign panel {pid}");
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self.ibus_log().iter().any(|l| l.starts_with(&started)) {
            assert!(Instant::now() < deadline, "the foreign panel did not start");
            thread::sleep(Duration::from_millis(20));
        }
        pid
    }

    /// Wait until `n` Wayland panels were started, and return their IDs.
    pub fn wait_for_panels(&self, n: usize) -> Vec<u32> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let panels = self.panels_started();
            if panels.len() >= n {
                return panels;
            }
            assert!(
                Instant::now() < deadline,
                "{} panels started, expected {n}; IBus log: {:?}",
                panels.len(),
                self.ibus_log()
            );
            thread::sleep(Duration::from_millis(50));
        }
    }

    /// Install an IBus component description, as an input method package
    /// does. The daemon reads them when it applies a list with IBus sources.
    pub fn install_ibus_component(&self, name: &str, xml: &str) {
        let dir = self.dir.join("ibus");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(format!("{name}.xml")), xml).unwrap();
    }

    /// Read a key the way `gsettings get` prints it.
    pub fn gsettings_get(&self, schema: &str, key: &str) -> String {
        let output = Command::new("gsettings")
            .args(["get", schema, key])
            .env("GSETTINGS_BACKEND", "keyfile")
            .env("XDG_CONFIG_HOME", self.dir.join("config"))
            .output()
            .unwrap();
        assert!(output.status.success(), "gsettings get {schema} {key}");
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    /// Change a key the way a Settings panel does: through GSettings, here
    /// with the `gsettings` tool on the daemon's keyfile backend.
    pub fn gsettings_set(&self, schema: &str, key: &str, value: &str) {
        let status = Command::new("gsettings")
            .args(["set", schema, key, value])
            .env("GSETTINGS_BACKEND", "keyfile")
            .env("XDG_CONFIG_HOME", self.dir.join("config"))
            .status()
            .unwrap();
        assert!(status.success(), "gsettings set {schema} {key} {value}");
    }
}

/// Write the stand-in IBus programs into `dir`, start the private bus with a
/// stand-in trawld on it, and set up `command` to run the daemon with them.
/// Returns the bus process.
fn fake_ibus(
    dir: &Path,
    name: &str,
    ibus: Ibus,
    trawl_calls: &Arc<Mutex<Vec<String>>>,
    command: &mut Command,
) -> Child {
    use std::io::BufRead;
    use std::os::unix::fs::PermissionsExt;

    let bin = dir.join("ibus-bin");
    fs::create_dir_all(&bin).unwrap();
    let quoted = format!("'{}'", dir.display());
    // The stand-ins find the tools they use even when the daemon runs
    // without them in its PATH.
    let path = std::env::var_os("PATH").unwrap_or_default();
    let tools = format!("'{}'", path.to_string_lossy());
    for (program, body) in FAKE_IBUS {
        let body = match program {
            "ibus" => body.to_string(),
            _ => format!("{body}while [ -d \"$D\" ]; do sleep 0.1; done\n"),
        };
        let path = bin.join(program);
        fs::write(
            &path,
            format!("#!/bin/sh\nD={quoted}\nPATH={tools}\n{body}"),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let config = dir.join("bus.conf");
    fs::write(
        &config,
        format!(
            "<busconfig><type>session</type><listen>unix:path={}</listen>\
             <auth>EXTERNAL</auth><policy context=\"default\">\
             <allow send_destination=\"*\" eavesdrop=\"true\"/><allow eavesdrop=\"true\"/>\
             <allow own=\"*\"/></policy></busconfig>",
            dir.join("bus").display()
        ),
    )
    .unwrap();
    let mut bus = Command::new("dbus-daemon")
        .arg(format!("--config-file={}", config.display()))
        .args(["--nofork", "--print-address=1"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("dbus-daemon is needed to stand in for IBus's bus");
    let mut address = String::new();
    std::io::BufReader::new(bus.stdout.take().unwrap())
        .read_line(&mut address)
        .unwrap();
    let address = address.trim();
    fs::write(dir.join("bus-address"), address).unwrap();
    if !matches!(ibus.trawl, Trawl::Missing) {
        fake_trawld(address, ibus.trawl, trawl_calls.clone());
    }

    let mut dirs = vec![bin.clone()];
    dirs.extend(std::env::split_paths(&path));
    let wayland_display = test_display(name);
    if let Some(args) = ibus.foreign_daemon {
        let mut daemon = Command::new(bin.join("ibus-daemon"))
            .args(args.split_whitespace())
            .env("WAYLAND_DISPLAY", &wayland_display)
            .spawn()
            .unwrap();
        // Reaped once it is replaced.
        thread::spawn(move || daemon.wait());
        let deadline = Instant::now() + Duration::from_secs(10);
        while !dir.join("address").exists() {
            assert!(
                Instant::now() < deadline,
                "the foreign ibus-daemon did not start"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }
    if ibus.foreign_panel {
        let mut panel = Command::new(bin.join("ibus-ui-gtk3"))
            .arg("--enable-wayland-im")
            .env("WAYLAND_DISPLAY", &wayland_display)
            .spawn()
            .unwrap();
        // Reaped once the daemon stops it.
        thread::spawn(move || panel.wait());
        let log = dir.join("ibus.log");
        let deadline = Instant::now() + Duration::from_secs(10);
        while !fs::read_to_string(&log).is_ok_and(|l| l.starts_with("panel ")) {
            assert!(Instant::now() < deadline, "the foreign panel did not start");
            thread::sleep(Duration::from_millis(20));
        }
    }
    command
        .env("PATH", std::env::join_paths(dirs).unwrap())
        // The private bus is the daemon's session bus too, so that a trawld
        // of the session running the tests can't answer.
        .env("DBUS_SESSION_BUS_ADDRESS", address)
        // No such display exists: the stand-ins don't need one, and the
        // daemon stops only panels on its own display.
        .env("WAYLAND_DISPLAY", wayland_display);
    bus
}

/// The Wayland display a test's daemon is told it runs on. No such display
/// exists: the stand-ins don't need one.
fn test_display(name: &str) -> String {
    format!("inputd-test-{}-{name}", std::process::id())
}

/// The part of trawld's D-Bus interface the daemon uses.
const TRAWL_INTERFACE: &str = r#"<node>
  <interface name="org.regolith.trawl1">
    <method name="GetResource">
      <arg name="key" type="s" direction="in"/>
      <arg name="value" type="s" direction="out"/>
    </method>
  </interface>
</node>"#;

/// Serve `org.regolith.Trawl` on the bus at `address` until the bus goes,
/// answering `GetResource` for `regolith.experimental.ibus` as `trawl` says
/// and any other resource as unset, as trawld does, and logging each
/// resource asked for to `calls`. Returns once the name is owned.
fn fake_trawld(address: &str, trawl: Trawl, calls: Arc<Mutex<Vec<String>>>) {
    use gio::{Cancellable, DBusCallFlags, DBusConnection, DBusConnectionFlags, DBusNodeInfo};
    use glib::ToVariant;

    let address = address.to_string();
    let (ready, started) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let context = glib::MainContext::new();
        context
            .with_thread_default(|| {
                let connection = DBusConnection::for_address_sync(
                    &address,
                    DBusConnectionFlags::AUTHENTICATION_CLIENT
                        | DBusConnectionFlags::MESSAGE_BUS_CONNECTION,
                    None,
                    Cancellable::NONE,
                )
                .expect("the stand-in trawld cannot connect to the bus");
                let node = DBusNodeInfo::for_xml(TRAWL_INTERFACE).unwrap();
                let interface = node.lookup_interface("org.regolith.trawl1").unwrap();
                connection
                    .register_object(
                        "/org/regolith/Trawl",
                        &interface,
                        move |_, _, _, _, method, parameters, invocation| {
                            let Some((key,)) = parameters.get::<(String,)>() else {
                                invocation.return_dbus_error(
                                    "org.freedesktop.DBus.Error.InvalidArgs",
                                    method,
                                );
                                return;
                            };
                            calls.lock().unwrap().push(key.clone());
                            let value = match &trawl {
                                _ if key != "regolith.experimental.ibus" => "",
                                Trawl::Value(value) => value,
                                Trawl::Fails => {
                                    invocation.return_dbus_error(
                                        "org.freedesktop.DBus.Error.Failed",
                                        "the resources cannot be read",
                                    );
                                    return;
                                }
                                _ => "",
                            };
                            invocation.return_value(Some(&(value,).to_variant()));
                        },
                        |_, _, _, _, _| unreachable!("trawld has no properties"),
                        |_, _, _, _, _, _| false,
                    )
                    .unwrap();
                // DBUS_NAME_FLAG_DO_NOT_QUEUE: the bus is the test's own.
                connection
                    .call_sync(
                        Some("org.freedesktop.DBus"),
                        "/org/freedesktop/DBus",
                        "org.freedesktop.DBus",
                        "RequestName",
                        Some(&("org.regolith.Trawl", 4u32).to_variant()),
                        None,
                        DBusCallFlags::NONE,
                        -1,
                        Cancellable::NONE,
                    )
                    .expect("the stand-in trawld cannot own its name");
                let main_loop = glib::MainLoop::new(Some(&context), false);
                let quit = main_loop.clone();
                connection.connect_closed(move |_, _, _| quit.quit());
                ready.send(()).unwrap();
                main_loop.run();
            })
            .unwrap();
    });
    started.recv().expect("the stand-in trawld did not start");
}

impl Daemon {
    /// Wait up to five seconds for `key` to read `value`; returns what it
    /// read last.
    pub fn wait_for_gsetting(&self, schema: &str, key: &str, value: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let current = self.gsettings_get(schema, key);
            if current == value || Instant::now() >= deadline {
                return current;
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    /// Record every change to `schema` from now on, whoever makes it.
    pub fn monitor(&self, schema: &str) -> GSettingsMonitor {
        let mut child = Command::new("gsettings")
            .args(["monitor", schema])
            .env("GSETTINGS_BACKEND", "keyfile")
            .env("XDG_CONFIG_HOME", self.dir.join("config"))
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let changes = Arc::new(Mutex::new(Vec::new()));
        let (stdout, recorded) = (child.stdout.take().unwrap(), changes.clone());
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                recorded.lock().unwrap().push(line);
            }
        });
        // gsettings monitor reports nothing until it watches the keyfile.
        thread::sleep(Duration::from_millis(300));
        GSettingsMonitor { child, changes }
    }
}

/// The changes `gsettings monitor` reported, one line per change.
pub struct GSettingsMonitor {
    child: Child,
    changes: Arc<Mutex<Vec<String>>>,
}

impl GSettingsMonitor {
    pub fn changes(&self) -> Vec<String> {
        self.changes.lock().unwrap().clone()
    }
}

impl Drop for GSettingsMonitor {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Whether the installed GSettings schema `schema` has `key`. Keys are added
/// over GNOME releases, so older distributions lack some of them.
pub fn schema_has_key(schema: &str, key: &str) -> bool {
    let output = Command::new("gsettings")
        .args(["list-keys", schema])
        .output()
        .unwrap();
    assert!(output.status.success(), "gsettings list-keys {schema}");
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .any(|k| k == key)
}

/// Change a key and return the first command sway gets afterwards that
/// starts with `prefix`.
pub fn change(
    sway: &FakeSway,
    daemon: &Daemon,
    (schema, key, value): (&str, &str, &str),
    prefix: &str,
) -> Option<String> {
    let seen = sway.commands().len();
    daemon.gsettings_set(schema, key, value);
    sway.wait_for(seen, prefix)
}
