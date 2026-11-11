//! Shared harness for the integration tests: run the real daemon against a
//! fake sway IPC socket and a keyfile GSettings backend, and record which
//! commands it sends to sway.

#![allow(dead_code)]

use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const MAGIC: &[u8; 6] = b"i3-ipc";
const RUN_COMMAND: u32 = 0;
const SUBSCRIBE: u32 = 2;
const GET_TREE: u32 = 4;
const SEND_TICK: u32 = 10;
const GET_INPUTS: u32 = 100;
const WORKSPACE_EVENT: u32 = 0x8000_0000;
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
}

struct State {
    inputs: Value,
    /// Subscribed clients, with the events they subscribed to.
    subscribers: Vec<(UnixStream, Vec<String>)>,
    /// How many times a client asked for the tree.
    tree_requests: usize,
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
        let state = Arc::new(Mutex::new(State {
            inputs: serde_json::from_str(inputs).unwrap(),
            subscribers: Vec::new(),
            tree_requests: 0,
        }));
        let (recorded, flag, shared) = (commands.clone(), subscribed.clone(), state.clone());
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (recorded, flag, shared) = (recorded.clone(), flag.clone(), shared.clone());
                thread::spawn(move || serve(stream, recorded, flag, shared));
            }
        });
        FakeSway {
            commands,
            subscribed,
            state,
        }
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
        stream.read_exact(&mut payload).unwrap();
        let payload = String::from_utf8(payload).unwrap();
        // Replies are written with the state locked, so that a client never
        // gets the events a command caused before the command's reply
        // (which is what a fence relies on).
        let mut state = state.lock().unwrap();
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
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.dir);
    }
}

impl Daemon {
    /// Whether the daemon process is still running.
    pub fn is_running(&mut self) -> bool {
        self.child.try_wait().unwrap().is_none()
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
        let dir = std::env::temp_dir().join(format!("inputd-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let settings_dir = dir.join("config/glib-2.0/settings");
        fs::create_dir_all(&settings_dir).unwrap();
        fs::write(settings_dir.join("keyfile"), keyfile).unwrap();

        let socket = dir.join("sway.sock");
        let sway = FakeSway::start(&socket, inputs);
        let child = Command::new(env!("CARGO_BIN_EXE_regolith-inputd"))
            // Without a Wayland display the daemon starts no input method, so
            // tests run from a desktop session leave its IBus alone.
            .env_remove("WAYLAND_DISPLAY")
            .env("SWAYSOCK", &socket)
            .env("GSETTINGS_BACKEND", "keyfile")
            .env("XDG_CONFIG_HOME", dir.join("config"))
            .env("IBUS_COMPONENT_PATH", dir.join("ibus"))
            .env("RUST_LOG", "info")
            .spawn()
            .unwrap();
        (sway, Daemon { child, dir })
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
