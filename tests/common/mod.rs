//! Shared harness for the integration tests: run the real daemon against a
//! fake sway IPC socket and a keyfile GSettings backend, and record which
//! commands it sends to sway.

#![allow(dead_code)]

use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const MAGIC: &[u8; 6] = b"i3-ipc";
const RUN_COMMAND: u32 = 0;
const SUBSCRIBE: u32 = 2;
const GET_INPUTS: u32 = 100;
const TICK_EVENT: u32 = 0x8000_0007;

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
pub struct FakeSway {
    commands: Arc<Mutex<Vec<String>>>,
    subscribed: Arc<Mutex<bool>>,
    events: Arc<Mutex<Option<UnixStream>>>,
}

impl FakeSway {
    fn start(socket: &Path, inputs: &'static str) -> FakeSway {
        let listener = UnixListener::bind(socket).unwrap();
        let commands = Arc::new(Mutex::new(Vec::new()));
        let subscribed = Arc::new(Mutex::new(false));
        let events = Arc::new(Mutex::new(None));
        let (recorded, flag, subscriber) = (commands.clone(), subscribed.clone(), events.clone());
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (recorded, flag, subscriber) =
                    (recorded.clone(), flag.clone(), subscriber.clone());
                thread::spawn(move || serve(stream, inputs, recorded, flag, subscriber));
            }
        });
        FakeSway {
            commands,
            subscribed,
            events,
        }
    }

    /// Send a tick event to the daemon, as `swaymsg -t send_tick` does. The
    /// Regolith sway config sends `{"status": "reload_done"}` on every
    /// config load.
    pub fn send_tick(&self, payload: &str) {
        let event = serde_json::json!({"first": false, "payload": payload}).to_string();
        let mut stream = self.events.lock().unwrap();
        let stream = stream.as_mut().expect("daemon has not subscribed");
        stream.write_all(&message(TICK_EVENT, &event)).unwrap();
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
    inputs: &str,
    recorded: Arc<Mutex<Vec<String>>>,
    subscribed: Arc<Mutex<bool>>,
    subscriber: Arc<Mutex<Option<UnixStream>>>,
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
        let reply = match kind {
            RUN_COMMAND => {
                recorded
                    .lock()
                    .unwrap()
                    .push(String::from_utf8(payload).unwrap());
                r#"[{"success": true}]"#
            }
            SUBSCRIBE => {
                *subscriber.lock().unwrap() = stream.try_clone().ok();
                *subscribed.lock().unwrap() = true;
                r#"{"success": true}"#
            }
            GET_INPUTS => inputs,
            _ => "[]",
        };
        if stream.write_all(&message(kind, reply)).is_err() {
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
            .env("SWAYSOCK", &socket)
            .env("GSETTINGS_BACKEND", "keyfile")
            .env("XDG_CONFIG_HOME", dir.join("config"))
            .env("RUST_LOG", "info")
            .spawn()
            .unwrap();
        (sway, Daemon { child, dir })
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
