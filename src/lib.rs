mod cursor;
mod input_sources;
mod keyboard;
mod libwacom;
mod mouse;
mod tablet;
mod touchpad;
mod traits;
mod utils;

use cursor::CursorHandler;
use input_sources::InputSourcesHandler;
use keyboard::KeyboardHandler;
use log::info;
use log::{debug, error, warn};
use mouse::MouseHandler;
use serde::Deserialize;
use std::error::Error;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread;
use std::time::Duration;
use swayipc::{Connection as SwayConnection, Event, TickEvent};
use tablet::TabletHandler;
use touchpad::TouchpadHandler;
use traits::InputHandler;
use utils::SwayEvent;

static mut ALLOW_SWAYINPUT_APPLY: AtomicBool = AtomicBool::new(true);
static mut ALLOW_GSETTINGS_APPLY: AtomicBool = AtomicBool::new(true);

// Type Aliases
type SharedRef<T> = Arc<Mutex<T>>;
type HandlerList = SharedRef<[Box<dyn InputHandler + Send>; 5]>;

// Structs
pub struct SettingsManager {
    handlers: HandlerList,
    /// Tablets have per-device settings and follow devices and outputs, so
    /// they are handled apart from the per-schema handlers above.
    tablets: Option<TabletHandler>,
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
// Method Implementations
impl SettingsManager {
    pub fn new() -> SettingsManager {
        utils::retry_action(SwayConnection::new, 5, Duration::from_millis(500));
        let handlers: HandlerList = Arc::new(Mutex::new([
            Box::new(MouseHandler::new()),
            Box::new(KeyboardHandler::new()),
            Box::new(TouchpadHandler::new()),
            Box::new(InputSourcesHandler::new()),
            Box::new(CursorHandler::new()),
        ]));
        let tablets = TabletHandler::new();
        if tablets.is_none() {
            warn!("Tablet settings schema not found, tablet settings are not mirrored");
        }
        SettingsManager { handlers, tablets }
    }

    pub fn start_monitoring(&mut self) -> Result<(), Box<dyn Error + '_>> {
        let mut handlers_lock = self.handlers.lock()?;
        for handle in handlers_lock.iter_mut() {
            handle.apply_all_sync()?;
            handle.monitor_gsettings_change();
        }
        if let Some(tablets) = &self.tablets {
            if let Err(e) = tablets.apply_all(false) {
                error!("Failed to apply tablet settings: {e}");
            }
        }

        let handlers_sref = self.handlers.clone();
        let tablets = self.tablets.clone();
        thread::spawn(move || Self::monitor_swayinput_events(handlers_sref, tablets));
        Ok(())
    }

    fn monitor_swayinput_events(mut handlers_sref: HandlerList, tablets: Option<TabletHandler>) {
        let event_stream = utils::retry_action(
            utils::SwayEventStream::subscribe,
            5,
            Duration::from_millis(500),
        );
        for event in event_stream {
            match event {
                // Tablets react to devices being plugged in and out, never
                // to the configuration changes their own commands cause, so
                // they skip the guard below.
                Ok(SwayEvent::Event(Event::Input(event))) if tablet::is_tablet(&event.input) => {
                    if let Some(tablets) = &tablets {
                        if let Err(e) = tablets.input_changed(&event) {
                            warn!("Failed to apply tablet settings: {e}");
                        }
                    }
                }
                Ok(SwayEvent::Event(Event::Input(event)))
                    if unsafe { ALLOW_SWAYINPUT_APPLY.load(Ordering::Relaxed) } =>
                {
                    utils::sync_input_gsettings(&mut handlers_sref, &event.input).unwrap();
                }
                Ok(SwayEvent::Output) => {
                    if let Some(tablets) = &tablets {
                        if let Err(e) = tablets.outputs_changed() {
                            warn!("Failed to update tablet mappings: {e}");
                        }
                    }
                }
                Ok(SwayEvent::Event(Event::Tick(TickEvent {
                    payload,
                    first: false,
                    ..
                }))) => {
                    use SwayReloadStatus::{ReloadDone, ReloadPending};
                    match serde_json::from_str::<SwayReloadTick>(&payload) {
                        Ok(SwayReloadTick {
                            status: ReloadPending,
                        }) => unsafe {
                            ALLOW_SWAYINPUT_APPLY.store(false, Ordering::Relaxed);
                            info!(
                                "Recieved tick, allow_sync = {}",
                                ALLOW_SWAYINPUT_APPLY.load(Ordering::Relaxed)
                            );
                        },
                        Ok(SwayReloadTick { status: ReloadDone }) => {
                            thread::sleep(Duration::from_millis(100));
                            unsafe {
                                ALLOW_SWAYINPUT_APPLY.store(true, Ordering::Relaxed);
                                info!(
                                    "Recieved tick, allow_sync = {}",
                                    ALLOW_SWAYINPUT_APPLY.load(Ordering::Relaxed)
                                );
                            }
                            info!("Sway reload done - Reapplying configurations from gsettings");
                            let mut handlers_lock = handlers_sref
                                .lock()
                                .expect("Acquired lock for handers_sref");
                            for handle in handlers_lock.iter_mut() {
                                handle
                                    .apply_all_sync()
                                    .expect("Failed to re-apply configs from gsettings");
                            }
                            drop(handlers_lock);
                            if let Some(tablets) = &tablets {
                                if let Err(e) = tablets.apply_all(true) {
                                    warn!("Failed to re-apply tablet settings: {e}");
                                }
                            }
                        }
                        Err(e) => debug!("Invalid Payload Recieved: {e}"),
                    }
                }
                Err(e) => warn!("{e}"),
                _ => continue,
            }
        }
    }
}

impl Default for SettingsManager {
    fn default() -> Self {
        Self::new()
    }
}
