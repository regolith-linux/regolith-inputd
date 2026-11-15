//! The IBus engines the daemon knows, each with the XKB layout and variant
//! it applies in sway.
//!
//! As in GNOME, the running ibus-daemon decides which engines exist once it
//! has answered: [`crate::input_method`] hands its engine list over with
//! [`follow_ibus`] each time it connects to an ibus-daemon. The last list
//! stays in force while IBus is away.
//!
//! Before that, and in sessions without an input method, the engines are
//! read from IBus's component files the way ibus-daemon builds its own list:
//! a component either spells its engines out in its `<engines>` element or
//! names a command there (`<engines exec="…">`) that prints them. Unlike
//! ibus-daemon, which waits for a command however long it takes, a command
//! gets [`COMMAND_TIME_LIMIT`], and the commands of different components run
//! side by side. As ibus-daemon does with its registry cache, what a
//! component gave is kept until its file or one of its observed paths
//! changes, so each command runs once per change.

use crate::{shell, xml};
use log::{info, warn};
use std::collections::HashMap;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

/// Engine descriptions by engine name: the layout and variant each applies.
pub type Engines = HashMap<String, (String, String)>;

/// The layout of an engine whose description names none (or `default`).
pub const DEFAULT_LAYOUT: &str = "us";

/// Where IBus looks for components when `IBUS_COMPONENT_PATH` is unset.
const COMPONENT_DIR: &str = "/usr/share/ibus/component";

/// How long a component's command may run before it is stopped.
const COMMAND_TIME_LIMIT: Duration = Duration::from_secs(5);

/// How often a running command is checked once its output has ended.
const COMMAND_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// How deep below an observed directory its entries are followed.
const MAX_OBSERVED_DEPTH: usize = 16;

/// The engine list the session's IBus last reported, once it has answered.
static IBUS_ENGINES: Mutex<Option<Arc<Engines>>> = Mutex::new(None);

/// What the component files gave.
static COMPONENTS: Mutex<Option<Components>> = Mutex::new(None);

/// Those to tell when IBus reports a new engine list.
static WATCHERS: Mutex<Vec<glib::Sender<()>>> = Mutex::new(Vec::new());

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The engines installed for this session: those IBus reports once it has
/// answered, else those the component files describe.
pub fn installed() -> Arc<Engines> {
    if let Some(engines) = lock(&IBUS_ENGINES).clone() {
        return engines;
    }
    lock(&COMPONENTS)
        .get_or_insert_with(Components::default)
        .refresh()
}

/// Follow the engine list IBus reported: it decides from now on. Watchers
/// are told when it differs from the list in force.
pub fn follow_ibus(engines: Engines) {
    let mut current = lock(&IBUS_ENGINES);
    if current.as_deref() == Some(&engines) {
        return;
    }
    info!("IBus lists {} engines", engines.len());
    *current = Some(Arc::new(engines));
    drop(current);
    lock(&WATCHERS).retain(|watcher| watcher.send(()).is_ok());
}

/// A receiver that gets a message each time IBus reports an engine list
/// that differs from the one in force. Attach it to the main context.
pub fn watch() -> glib::Receiver<()> {
    let (sender, receiver) = glib::MainContext::channel(glib::PRIORITY_DEFAULT);
    lock(&WATCHERS).push(sender);
    receiver
}

/// The layout and variant sway gets for an engine description, as GNOME
/// Flashback maps them, made safe for sway: options in brackets after the
/// layout are dropped (as GNOME ignores engine options), an empty, `default`
/// or `*` layout is the default layout, and a `default` variant is none.
pub fn engine_layout(layout: &str, variant: &str) -> (String, String) {
    let layout = layout.trim();
    let layout = match layout.find('[') {
        Some(start) if layout.ends_with(']') => layout[..start].trim_end(),
        _ => layout,
    };
    let layout = match layout {
        "" | "default" | "*" => DEFAULT_LAYOUT,
        layout => layout,
    };
    let variant = match variant.trim() {
        "default" => "",
        variant => variant,
    };
    (layout.to_string(), variant.to_string())
}

/// The state of a path that decides whether what was read from it still
/// holds: whether it exists, its modification time and size, and for a
/// directory the state of each entry below it.
#[derive(Clone, Debug, PartialEq, Eq)]
enum PathState {
    Missing,
    File {
        modified: Option<SystemTime>,
        size: u64,
    },
    Directory {
        modified: Option<SystemTime>,
        entries: Vec<(OsString, PathState)>,
    },
}

impl PathState {
    fn of(path: &Path) -> PathState {
        PathState::below(path, MAX_OBSERVED_DEPTH)
    }

    fn below(path: &Path, depth: usize) -> PathState {
        let Ok(metadata) = fs::metadata(path) else {
            return PathState::Missing;
        };
        let modified = metadata.modified().ok();
        if !metadata.is_dir() {
            return PathState::File {
                modified,
                size: metadata.len(),
            };
        }
        let mut entries: Vec<(OsString, PathState)> = match fs::read_dir(path) {
            Ok(entries) if depth > 0 => entries
                .flatten()
                .map(|entry| {
                    let state = PathState::below(&entry.path(), depth - 1);
                    (entry.file_name(), state)
                })
                .collect(),
            _ => Vec::new(),
        };
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        PathState::Directory { modified, entries }
    }
}

/// What was read from one component file.
struct Component {
    /// The file's state when it was read.
    state: PathState,
    /// The component's observed paths, with their states when it was read.
    observed: Vec<(PathBuf, PathState)>,
    /// Its engines, in description order.
    engines: Vec<(String, (String, String))>,
}

impl Component {
    fn still_holds(&self, state: &PathState) -> bool {
        self.state == *state
            && self
                .observed
                .iter()
                .all(|(path, state)| PathState::of(path) == *state)
    }
}

/// One `<engines>` element of a component: its command, if it names one,
/// and the engines spelled out inside it.
struct EngineList {
    command: Option<String>,
    engines: Vec<(String, (String, String))>,
}

/// A component file read anew, before its commands ran.
struct Pending {
    path: PathBuf,
    state: PathState,
    observed: Vec<(PathBuf, PathState)>,
    lists: Vec<EngineList>,
}

#[derive(Default)]
struct Components {
    /// By component file.
    read: HashMap<PathBuf, Component>,
    /// The engines of every component, merged in the order IBus reads them.
    engines: Arc<Engines>,
}

impl Components {
    /// The engines the component files describe now. Files that changed
    /// since they were read are read again, and their commands run again.
    fn refresh(&mut self) -> Arc<Engines> {
        let files = component_files();
        let mut changed = false;
        let mut kept = HashMap::new();
        let mut pending = Vec::new();
        for path in &files {
            let state = PathState::of(path);
            if let Some(component) = self.read.remove(path) {
                if component.still_holds(&state) {
                    kept.insert(path.clone(), component);
                    continue;
                }
            }
            changed = true;
            pending.push(read_component(path, state));
        }
        // Components whose file went away.
        changed |= !self.read.is_empty();
        for component in run_commands(pending) {
            kept.insert(component.0, component.1);
        }
        self.read = kept;
        if changed {
            let mut engines = Engines::new();
            for component in files.iter().filter_map(|path| self.read.get(path)) {
                engines.extend(component.engines.iter().cloned());
            }
            self.engines = Arc::new(engines);
        }
        self.engines.clone()
    }
}

/// The component files in IBus's component directories
/// (`$IBUS_COMPONENT_PATH`, else `/usr/share/ibus/component`), in the order
/// their engines are merged: later ones replace earlier ones of the same name.
fn component_files() -> Vec<PathBuf> {
    let dirs: Vec<PathBuf> = match env::var_os("IBUS_COMPONENT_PATH") {
        Some(path) => env::split_paths(&path).collect(),
        None => vec![PathBuf::from(COMPONENT_DIR)],
    };
    let mut files = Vec::new();
    for dir in dirs {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        let mut paths: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|e| e == "xml"))
            .collect();
        paths.sort();
        files.extend(paths);
    }
    files
}

/// Read a component file. A file that can't be read, or isn't a component
/// description, is remembered as one without engines.
fn read_component(path: &Path, state: PathState) -> Pending {
    let mut pending = Pending {
        path: path.to_path_buf(),
        state,
        observed: Vec::new(),
        lists: Vec::new(),
    };
    let component = match fs::read(path).map(String::from_utf8) {
        Ok(Ok(text)) => xml::parse(&text).ok(),
        _ => None,
    };
    let Some(component) = component.filter(|c| c.name == "component") else {
        warn!("{} is not an IBus component description", path.display());
        return pending;
    };
    for paths in component.elements_named("observed-paths") {
        for observed in paths.elements_named("path") {
            let observed = observed_path(&observed.text());
            let state = PathState::of(&observed);
            pending.observed.push((observed, state));
        }
    }
    for list in component.elements_named("engines") {
        pending.lists.push(EngineList {
            command: list.attribute("exec").map(str::to_string),
            engines: engine_descriptions(list),
        });
    }
    pending
}

/// An observed path as IBus reads it: `~` stands for the home directory.
fn observed_path(path: &str) -> PathBuf {
    match (path.strip_prefix('~'), env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest.trim_start_matches('/')),
        _ => PathBuf::from(path),
    }
}

/// The `<engine>` descriptions inside an `<engines>` element.
fn engine_descriptions(engines: &xml::Element) -> Vec<(String, (String, String))> {
    engines
        .elements_named("engine")
        .filter_map(|engine| {
            let name = engine.child_text("name").filter(|n| !n.is_empty())?;
            let layout = engine.child_text("layout").unwrap_or_default();
            let variant = engine.child_text("layout_variant").unwrap_or_default();
            Some((name, engine_layout(&layout, &variant)))
        })
        .collect()
}

/// Run the commands of the components read anew, all at once, and give
/// each component with its engines: those its commands printed, or for a
/// command that failed, the engines spelled out next to it.
fn run_commands(pending: Vec<Pending>) -> Vec<(PathBuf, Component)> {
    let running: Vec<Vec<_>> = pending
        .iter()
        .map(|component| {
            component
                .lists
                .iter()
                .map(|list| {
                    let command = list.command.clone()?;
                    Some(thread::spawn(move || command_engines(&command)))
                })
                .collect()
        })
        .collect();
    pending
        .into_iter()
        .zip(running)
        .map(|(component, running)| {
            let mut engines = Vec::new();
            for (list, run) in component.lists.into_iter().zip(running) {
                let printed = run.and_then(|run| run.join().ok().flatten());
                engines.extend(printed.unwrap_or(list.engines));
            }
            let read = Component {
                state: component.state,
                observed: component.observed,
                engines,
            };
            (component.path, read)
        })
        .collect()
}

/// The engines a component's command prints, if it prints an `<engines>`
/// document within the time limit. Its exit status doesn't matter.
fn command_engines(command: &str) -> Option<Vec<(String, (String, String))>> {
    let Some(output) = run_command(command, COMMAND_TIME_LIMIT) else {
        warn!("IBus component command `{command}` gave no engine list");
        return None;
    };
    match String::from_utf8(output).map(|text| xml::parse(&text)) {
        Ok(Ok(root)) if root.name == "engines" => Some(engine_descriptions(&root)),
        _ => {
            warn!("IBus component command `{command}` printed no engine list");
            None
        }
    }
}

/// Run a command line as IBus does (split as by GLib, with no shell, the
/// daemon's environment, no input and its errors ignored) and give its
/// output once it has ended and the command has exited. A command still
/// running after `limit`, with all it started, is stopped instead.
fn run_command(command: &str, limit: Duration) -> Option<Vec<u8>> {
    let started = Instant::now();
    let argv = shell::parse_argv(command)?;
    let mut child = match Command::new(&argv[0])
        .args(&argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        // Its own process group, so that it can be stopped with every
        // process it started.
        .process_group(0)
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            warn!("Cannot run `{command}`: {e}");
            return None;
        }
    };
    // The output is read as it comes: a command that prints more than a
    // pipe holds waits for its reader before it can exit.
    let mut stdout = child.stdout.take()?;
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut output = Vec::new();
        let read = stdout.read_to_end(&mut output).map(|_| output);
        let _ = sender.send(read);
    });
    let deadline = started + limit;
    if let Ok(Ok(output)) =
        receiver.recv_timeout(deadline.saturating_duration_since(Instant::now()))
    {
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return Some(output),
                Ok(None) if Instant::now() < deadline => thread::sleep(COMMAND_POLL_INTERVAL),
                Ok(None) => break,
                Err(e) => {
                    warn!("`{command}`: {e}");
                    break;
                }
            }
        }
    }
    warn!(
        "`{command}` did not finish within {} s, stopping it",
        limit.as_secs()
    );
    stop(&mut child);
    None
}

/// Stop a command's process group, and reap the command.
fn stop(child: &mut Child) {
    extern "C" {
        fn kill(pid: i32, signal: i32) -> i32;
    }
    const SIGKILL: i32 = 9;
    let group = child.id() as i32;
    // SAFETY: kill(2) only sends a signal; the group is the one the command
    // was started in, and the command is not reaped yet, so its id still
    // names that group.
    if unsafe { kill(-group, SIGKILL) } != 0 {
        let _ = child.kill();
    }
    let _ = child.wait();
}

#[cfg(test)]
mod tests {
    use super::{engine_descriptions, engine_layout, run_command};
    use crate::xml;
    use std::time::{Duration, Instant};

    #[test]
    fn engine_layouts_are_made_safe_for_sway() {
        for ((layout, variant), expected) in [
            (("jp", ""), ("jp", "")),
            (("kr", "kr104"), ("kr", "kr104")),
            (("", ""), ("us", "")),
            (("default", "dvorak"), ("us", "dvorak")),
            (("*", ""), ("us", "")),
            (("default[lv3:ralt_switch]", ""), ("us", "")),
            (("de[lv3:ralt_switch]", "nodeadkeys"), ("de", "nodeadkeys")),
            (("us", "default"), ("us", "")),
        ] {
            assert_eq!(
                engine_layout(layout, variant),
                (expected.0.to_string(), expected.1.to_string()),
                "{layout} {variant}"
            );
        }
    }

    #[test]
    fn engine_descriptions_are_read_from_engines_elements() {
        let component = xml::parse(
            "<component><name>c</name><!-- <engine><name>x</name></engine> -->\
             <engines exec='cmd'><engine><name>xkb:jp::jpn</name><layout>jp</layout></engine>\
             <engine><name>e</name><layout_variant>dvorak</layout_variant>\
             <layout_option>ctrl:nocaps</layout_option></engine>\
             <engine><layout>de</layout></engine></engines></component>",
        )
        .unwrap();
        let engines = component.elements_named("engines").next().unwrap();
        assert_eq!(engines.attribute("exec"), Some("cmd"));
        assert_eq!(
            engine_descriptions(engines),
            vec![
                ("xkb:jp::jpn".into(), ("jp".into(), "".into())),
                ("e".into(), ("us".into(), "dvorak".into())),
            ]
        );
    }

    #[test]
    fn commands_give_their_whole_output_whatever_their_exit_status() {
        let output = run_command(
            "/bin/sh -c 'yes engine | head -c 300000; exit 3'",
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(output.len(), 300000);
        assert_eq!(
            run_command("/nonexistent/command", Duration::from_secs(5)),
            None
        );
        assert_eq!(run_command("'unterminated", Duration::from_secs(5)), None);
    }

    #[test]
    fn commands_are_stopped_with_what_they_started() {
        let started = Instant::now();
        // The shell's child keeps the output open after the shell exits.
        let output = run_command(
            "/bin/sh -c 'echo partial; sleep 30 & exit 0'",
            Duration::from_millis(300),
        );
        assert_eq!(output, None);
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
