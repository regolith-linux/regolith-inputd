mod cursor;
mod input_method;
mod input_sources;
mod keyboard;
mod mouse;
mod shortcuts;
pub mod source_switching;
mod sway;
mod sway_state;
mod touchpad;
mod traits;
mod utils;

use cursor::CursorHandler;
use input_sources::InputSourcesHandler;
use keyboard::KeyboardHandler;
use log::{debug, error, info, warn};
use mouse::MouseHandler;
use serde::Deserialize;
use shortcuts::ShortcutsHandler;
use source_switching::SourceSwitching;
use std::error::Error;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use sway::Sway;
use sway_state::{Change, SwayState};
use touchpad::TouchpadHandler;
use traits::InputHandler;
use utils::SwayEvent;

/// How long sway's input devices must go without an event before the daemon
/// reads their state. sway reports a reload's device resets before the
/// reload itself, so a change is taken as the user's only if no reload
/// follows it within this time.
const SETTLE_TIME: Duration = Duration::from_millis(500);

/// sway makes all of a reload's device changes within this time of the
/// reload's signal, so input events older than this, with no reload signal
/// since, are no reload's.
const RELOAD_LEAD: Duration = Duration::from_millis(250);

/// How long to wait for the end of a reload announced by a `reload_pending`
/// tick. A reload whose configuration fails to load never ends.
const RELOAD_TIMEOUT: Duration = Duration::from_secs(10);

/// While no sway is reachable, the shortest time between two attempts to
/// connect to one. sway can stay away for long, so looking for it must stay
/// cheap: fewer than two attempts a second, however many sockets there are
/// to try.
const RETRY_INTERVAL: Duration = Duration::from_millis(600);

/// The events the owner follows.
const SWAY_EVENTS: &str = r#"["input", "tick", "workspace"]"#;

type Handlers = Vec<Box<dyn InputHandler + Send>>;

/// What the owner of the handlers is told, in the order it happened. What
/// comes from sway names the sway instance it comes from (see
/// [`Sway::generation`]): an earlier instance's messages are left alone.
pub(crate) enum Message {
    /// A key of the schema of handler `.0` changed in GSettings.
    GSettingsChanged(usize, String),
    /// sway sent an input event about a device's configuration.
    SwayInput(u64),
    /// sway sent an input event about a device being added or removed.
    SwayHotplug(u64),
    /// Regolith's reload binding is about to reload sway.
    ReloadPending(u64),
    /// sway has loaded its configuration again: a `reload_done` tick, or
    /// sway's own workspace "reload" event.
    ReloadDone(u64),
    /// The sway is gone, for the reason given: one of the daemon's
    /// connections to it closed or failed, or it left a request unanswered.
    SwayLost(u64, String),
}

// Structs
pub struct SettingsManager {
    handlers: Handlers,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
enum SwayReloadStatus {
    #[serde(rename = "reload_pending")]
    ReloadPending,
    #[serde(rename = "reload_done")]
    ReloadDone,
}

#[derive(Deserialize, Debug)]
struct SwayReloadTick {
    status: SwayReloadStatus,
}

/// A sway reload in progress.
struct Reload {
    started: Instant,
    done: bool,
}

/// The single owner of the handlers. GSettings and sway only send it
/// messages, which it handles one at a time, so applying GSettings to sway
/// and mirroring sway to GSettings never overlap.
///
/// sway's input events don't say who made a change, and they can arrive
/// after later changes were made, so what an event reports is never
/// mirrored. Once sway's devices have settled, the owner reads their state
/// and mirrors what changed since the state it last accepted, which it
/// reads after each of its own changes to sway. Its own commands, and the
/// events sway sends about them, then show no change.
///
/// The owner also looks for sway whenever none is reachable, and treats each
/// sway it reaches as a first launch: whatever it knew about an earlier one
/// is dropped, GSettings is applied in full, and only then is sway's state
/// taken as the accepted one and its events followed. Settings changed in
/// the meantime are applied then, from their current values.
struct Owner {
    handlers: Handlers,
    /// sway's input state as the daemon last accepted it.
    known: SwayState,
    /// When sway last sent an input event the owner hasn't looked at.
    unsettled: Option<Instant>,
    reload: Option<Reload>,
    /// The sway the daemon follows, while one is reachable.
    sway: Option<Arc<Sway>>,
    /// How many sway instances the daemon has reached.
    generation: u64,
    /// While no sway is reachable, when to look for one next.
    next_attempt: Option<Instant>,
    /// When the daemon last tried to connect to a sway socket.
    last_attempt: Option<Instant>,
    /// Whether the log already says no sway is reachable: once per loss, not
    /// once per attempt.
    absence_logged: bool,
    /// The owner's own queue, for what sway instances report.
    sender: Sender<Message>,
    switching: SourceSwitching,
}

/// Whether `e` only says that sway is gone, which is logged once, when the
/// owner learns of it. It is then logged at debug level here.
fn quiet(e: &(dyn Error + 'static)) -> bool {
    let gone = sway::is_gone(e);
    if gone {
        debug!("{e}");
    }
    gone
}

impl Owner {
    fn run(mut self, messages: Receiver<Message>) {
        loop {
            let message = match self.deadline() {
                Some(deadline) => {
                    let timeout = deadline.saturating_duration_since(Instant::now());
                    match messages.recv_timeout(timeout) {
                        Ok(message) => Some(message),
                        Err(RecvTimeoutError::Timeout) => None,
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                }
                None => match messages.recv() {
                    Ok(message) => Some(message),
                    Err(_) => return,
                },
            };
            match message {
                Some(Message::GSettingsChanged(index, key)) => self.apply_change(index, &key),
                Some(Message::SwayInput(g)) if self.follows(g) => {
                    self.unsettled = Some(Instant::now())
                }
                Some(Message::SwayHotplug(g)) if self.follows(g) => self.hotplugged(),
                Some(Message::ReloadPending(g)) if self.follows(g) => self.reloading(false),
                Some(Message::ReloadDone(g)) if self.follows(g) => self.reloading(true),
                Some(Message::SwayLost(g, reason)) if self.follows(g) => self.lost(&reason),
                // From an earlier sway.
                Some(_) => (),
                None if self.sway.is_none() => self.look_for_sway(),
                None => self.settled(),
            }
        }
    }

    /// Whether the owner follows sway instance `generation`.
    fn follows(&self, generation: u64) -> bool {
        self.sway
            .as_ref()
            .is_some_and(|sway| sway.generation() == generation)
    }

    /// When to look at sway's state next, if anything is waiting, or, while
    /// no sway is reachable, when to look for one.
    fn deadline(&self) -> Option<Instant> {
        if self.sway.is_none() {
            return self.next_attempt;
        }
        match (&self.reload, self.unsettled) {
            (Some(reload), _) if !reload.done => Some(reload.started + RELOAD_TIMEOUT),
            (_, Some(last_event)) => Some(last_event + SETTLE_TIME),
            (_, None) => None,
        }
    }

    /// Try each socket sway may be listening on, and follow the first sway
    /// that answers.
    fn look_for_sway(&mut self) {
        for path in sway::candidates() {
            if let Some(last) = self.last_attempt {
                thread::sleep((last + RETRY_INTERVAL).saturating_duration_since(Instant::now()));
            }
            self.last_attempt = Some(Instant::now());
            match Sway::connect(&path, self.generation + 1, self.sender.clone()) {
                Ok(sway) => {
                    self.generation += 1;
                    return self.start(sway);
                }
                Err(e) => debug!("No sway answers at {}: {e}", path.display()),
            }
        }
        if !self.absence_logged {
            warn!("No sway is reachable, waiting for one");
            self.absence_logged = true;
        }
        self.next_attempt = Some(Instant::now() + RETRY_INTERVAL);
    }

    /// Follow a sway that was just reached as at a first launch.
    fn start(&mut self, sway: Arc<Sway>) {
        info!("Connected to sway at {}", sway.path().display());
        self.absence_logged = false;
        self.next_attempt = None;
        self.unsettled = None;
        self.reload = None;
        // Nothing the daemon knew about an earlier sway holds for this one.
        for handler in self.handlers.iter_mut() {
            handler.reset();
        }
        sway::set_current(sway.clone());
        self.sway = Some(sway.clone());
        for handler in self.handlers.iter_mut() {
            // One failing handler must not keep the others from applying.
            if let Err(e) = handler.apply_all() {
                if !quiet(&*e) {
                    error!("Failed to apply configs from gsettings: {e}");
                }
            }
        }
        // What sway started with is overwritten, never mirrored: its state
        // after GSettings was applied is the accepted one.
        self.accept_sway_state();
        let sender = self.sender.clone();
        let generation = sway.generation();
        let followed = sway.follow(SWAY_EVENTS, move |kind, payload| {
            match sway_message(generation, kind, &payload) {
                Some(message) => sender.send(message).is_ok(),
                None => true,
            }
        });
        // A sway that is lost already is followed no further.
        if followed.is_ok() {
            self.switching.follow(sway);
        }
    }

    /// The sway followed is gone: look for one again.
    fn lost(&mut self, reason: &str) {
        warn!("Lost the connection to sway: {reason}");
        self.absence_logged = true;
        if let Some(sway) = self.sway.take() {
            sway::forget(sway.generation());
        }
        self.known = SwayState::default();
        self.unsettled = None;
        self.reload = None;
        self.next_attempt = Some(Instant::now());
    }

    fn reloading(&mut self, done: bool) {
        info!("Sway reload {}", if done { "done" } else { "pending" });
        // The wait for a reload runs from its own reload_pending tick.
        let started = match &self.reload {
            Some(reload) if done => reload.started,
            _ => Instant::now(),
        };
        self.reload = Some(Reload { started, done });
        // Some of the reload's device resets may still be on their way.
        self.unsettled = Some(Instant::now());
    }

    /// A device was plugged in or out. What it arrived with, or took away,
    /// is no change a person made: take the devices of the types whose
    /// devices changed as they are now, so that changes made to them from
    /// now on are mirrored.
    fn hotplugged(&mut self) {
        self.unsettled = Some(Instant::now());
        // After a reload, sway's whole state is taken anyway.
        if self.reload.is_some() {
            return;
        }
        match SwayState::read() {
            Ok(state) => self.known.adopt_devices(&state),
            Err(e) if quiet(&e) => (),
            Err(e) => warn!("Failed to read sway's input state: {e}"),
        }
    }

    fn settled(&mut self) {
        self.unsettled = None;
        if self.reload.take().is_some() {
            // Whatever sway reported since the reload began is the reload's
            // doing: apply GSettings over it.
            info!("Sway reload done - Reapplying configurations from gsettings");
            for handler in self.handlers.iter_mut() {
                // One failing handler must not end reload handling.
                if let Err(e) = handler.apply_all() {
                    if !quiet(&*e) {
                        error!("Failed to re-apply configs from gsettings: {e}");
                    }
                }
            }
            self.accept_sway_state();
        } else {
            self.mirror_sway_changes();
        }
    }

    fn apply_change(&mut self, index: usize, key: &str) {
        if self.sway.is_none() {
            // Everything is applied, from its current value, once sway is
            // back.
            debug!("No sway to apply {key} to yet");
            return;
        }
        // A change made in sway that hasn't settled yet, but is too old to
        // be a reload's, is a person's. Taking sway's state after applying
        // GSettings would hide it, so note it first.
        let pending = match (&self.reload, self.unsettled) {
            (None, Some(last_event)) if last_event.elapsed() >= RELOAD_LEAD => {
                match SwayState::read() {
                    Ok(state) => {
                        let changes = self.known.changes(&state);
                        Some((state, changes))
                    }
                    Err(e) => {
                        if !quiet(&e) {
                            warn!("Failed to read sway's input state: {e}");
                        }
                        None
                    }
                }
            }
            _ => None,
        };
        if let Err(e) = self.handlers[index].apply_changes(key) {
            if !quiet(&*e) {
                error!("{e}");
            }
        }
        // During a reload, everything is applied again once it ends.
        if self.reload.is_none() {
            self.accept_sway_state();
            // Mirror what the GSettings change left in place: a later
            // GSettings change to the same setting wins over sway's.
            if let Some((before, changes)) = pending {
                let kept: Vec<_> = changes
                    .into_iter()
                    .filter(|change| self.known.still_holds(&before, change))
                    .collect();
                self.mirror(kept);
            }
        }
    }

    /// Take sway's current input state as the one changes are relative to.
    fn accept_sway_state(&mut self) {
        match SwayState::read() {
            Ok(state) => self.known = state,
            Err(e) if quiet(&e) => (),
            Err(e) => warn!("Failed to read sway's input state: {e}"),
        }
    }

    /// Mirror to GSettings what changed in sway since the accepted state.
    fn mirror_sway_changes(&mut self) {
        let state = match SwayState::read() {
            Ok(state) => state,
            Err(e) if quiet(&e) => return,
            Err(e) => return warn!("Failed to read sway's input state: {e}"),
        };
        let changes = self.known.changes(&state);
        self.mirror(changes);
        self.known = state;
    }

    /// Write settings sway's devices changed to GSettings.
    fn mirror(&self, changes: Vec<Change>) {
        for Change {
            input_type,
            setting,
            value,
        } in changes
        {
            let handler = self
                .handlers
                .iter()
                .find(|h| h.sway_input_type() == Some(input_type.as_str()));
            if let Some(handler) = handler {
                debug!("Sway set {input_type} {setting} to {value}");
                if let Err(e) = handler.sync_gsettings(&setting, &value) {
                    error!("Failed to mirror {input_type} {setting} to gsettings: {e}");
                }
            }
        }
    }
}

/// The message for the owner about an event of sway instance `generation`,
/// if it is one the owner follows.
fn sway_message(generation: u64, kind: u32, payload: &[u8]) -> Option<Message> {
    Some(match utils::decode_event(kind, payload) {
        SwayEvent::Input => Message::SwayInput(generation),
        SwayEvent::Hotplug => Message::SwayHotplug(generation),
        SwayEvent::WorkspaceReload => Message::ReloadDone(generation),
        SwayEvent::Tick(payload) => {
            use SwayReloadStatus::{ReloadDone, ReloadPending};
            match serde_json::from_str::<SwayReloadTick>(&payload) {
                Ok(SwayReloadTick {
                    status: ReloadPending,
                }) => Message::ReloadPending(generation),
                Ok(SwayReloadTick { status: ReloadDone }) => Message::ReloadDone(generation),
                Err(e) => {
                    debug!("Invalid Payload Recieved: {e}");
                    return None;
                }
            }
        }
        SwayEvent::Other => return None,
    })
}

// Method Implementations
impl SettingsManager {
    pub fn new() -> SettingsManager {
        let mut handlers: Handlers = vec![
            Box::new(MouseHandler::new()),
            Box::new(KeyboardHandler::new()),
            Box::new(TouchpadHandler::new()),
            Box::new(InputSourcesHandler::new()),
            Box::new(CursorHandler::new()),
        ];
        match ShortcutsHandler::new() {
            Some(shortcuts) => handlers.push(Box::new(shortcuts)),
            None => warn!("Media keys schema not found, keyboard shortcuts are not mirrored"),
        }
        SettingsManager { handlers }
    }

    /// Follow GSettings, and sway whenever one is reachable: it may start
    /// later, go away and come back. Each sway reached gets every setting,
    /// and then `switching` follows it too.
    pub fn start_monitoring(&mut self, switching: SourceSwitching) {
        let (sender, messages) = mpsc::channel();
        for (index, handler) in self.handlers.iter().enumerate() {
            handler.monitor_gsettings_change(index, sender.clone());
        }
        let owner = Owner {
            handlers: std::mem::take(&mut self.handlers),
            known: SwayState::default(),
            unsettled: None,
            reload: None,
            sway: None,
            generation: 0,
            next_attempt: Some(Instant::now()),
            last_attempt: None,
            absence_logged: false,
            sender,
            switching,
        };
        thread::spawn(move || owner.run(messages));
    }
}

impl Default for SettingsManager {
    fn default() -> Self {
        Self::new()
    }
}
