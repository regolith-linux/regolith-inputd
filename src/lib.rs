mod cursor;
mod input_method;
mod input_sources;
mod keyboard;
mod mouse;
mod shortcuts;
pub mod source_switching;
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
use std::error::Error;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};
use sway_state::{Change, SwayState};
use swayipc::Connection as SwayConnection;
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

type Handlers = Vec<Box<dyn InputHandler + Send>>;

/// What the owner of the handlers is told, in the order it happened.
pub(crate) enum Message {
    /// A key of the schema of handler `.0` changed in GSettings.
    GSettingsChanged(usize, String),
    /// sway sent an input event about a device's configuration.
    SwayInput,
    /// sway sent an input event about a device being added or removed.
    SwayHotplug,
    /// Regolith's reload binding is about to reload sway.
    ReloadPending,
    /// sway has loaded its configuration again: a `reload_done` tick, or
    /// sway's own workspace "reload" event.
    ReloadDone,
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
struct Owner {
    handlers: Handlers,
    /// sway's input state as the daemon last accepted it.
    known: SwayState,
    /// When sway last sent an input event the owner hasn't looked at.
    unsettled: Option<Instant>,
    reload: Option<Reload>,
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
                Some(Message::SwayInput) => self.unsettled = Some(Instant::now()),
                Some(Message::SwayHotplug) => self.hotplugged(),
                Some(Message::ReloadPending) => self.reloading(false),
                Some(Message::ReloadDone) => self.reloading(true),
                None => self.settled(),
            }
        }
    }

    /// When to look at sway's state next, if anything is waiting.
    fn deadline(&self) -> Option<Instant> {
        match (&self.reload, self.unsettled) {
            (Some(reload), _) if !reload.done => Some(reload.started + RELOAD_TIMEOUT),
            (_, Some(last_event)) => Some(last_event + SETTLE_TIME),
            (_, None) => None,
        }
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
                    error!("Failed to re-apply configs from gsettings: {e}");
                }
            }
            self.accept_sway_state();
        } else {
            self.mirror_sway_changes();
        }
    }

    fn apply_change(&mut self, index: usize, key: &str) {
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
                        warn!("Failed to read sway's input state: {e}");
                        None
                    }
                }
            }
            _ => None,
        };
        if let Err(e) = self.handlers[index].apply_changes(key) {
            error!("{e}");
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
            Err(e) => warn!("Failed to read sway's input state: {e}"),
        }
    }

    /// Mirror to GSettings what changed in sway since the accepted state.
    fn mirror_sway_changes(&mut self) {
        let state = match SwayState::read() {
            Ok(state) => state,
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

// Method Implementations
impl SettingsManager {
    pub fn new() -> SettingsManager {
        utils::retry_action(SwayConnection::new, 5, Duration::from_millis(500));
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

    pub fn start_monitoring(&mut self) -> Result<(), Box<dyn Error + '_>> {
        let (sender, messages) = mpsc::channel();
        for (index, handler) in self.handlers.iter_mut().enumerate() {
            handler.apply_all()?;
            handler.monitor_gsettings_change(index, sender.clone());
        }
        let owner = Owner {
            handlers: std::mem::take(&mut self.handlers),
            known: SwayState::read()?,
            unsettled: None,
            reload: None,
        };
        thread::spawn(move || owner.run(messages));
        thread::spawn(move || Self::monitor_sway_events(sender));
        Ok(())
    }

    /// Pass sway's input events and reloads on to the owner.
    fn monitor_sway_events(sender: Sender<Message>) {
        let events = utils::retry_action(utils::subscribe_to_events, 5, Duration::from_millis(500));
        for event in events {
            let message = match event {
                Ok(SwayEvent::Input) => Message::SwayInput,
                Ok(SwayEvent::Hotplug) => Message::SwayHotplug,
                Ok(SwayEvent::WorkspaceReload) => Message::ReloadDone,
                Ok(SwayEvent::Tick(payload)) => {
                    use SwayReloadStatus::{ReloadDone, ReloadPending};
                    match serde_json::from_str::<SwayReloadTick>(&payload) {
                        Ok(SwayReloadTick {
                            status: ReloadPending,
                        }) => Message::ReloadPending,
                        Ok(SwayReloadTick { status: ReloadDone }) => Message::ReloadDone,
                        Err(e) => {
                            debug!("Invalid Payload Recieved: {e}");
                            continue;
                        }
                    }
                }
                Ok(SwayEvent::Other) => continue,
                Err(e) => {
                    warn!("{e}");
                    return;
                }
            };
            if sender.send(message).is_err() {
                return;
            }
        }
    }
}

impl Default for SettingsManager {
    fn default() -> Self {
        Self::new()
    }
}
