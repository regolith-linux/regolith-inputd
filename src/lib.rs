mod input_sources;
mod keyboard;
mod mouse;
mod touchpad;
mod traits;
mod utils;

use input_sources::InputSourcesHandler;
use keyboard::KeyboardHandler;
use log::{debug, warn};
use mouse::MouseHandler;
use serde::Deserialize;
use std::error::Error;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use swayipc::{Connection as SwayConnection, Event, TickEvent};
use touchpad::TouchpadHandler;
use traits::InputHandler;
use log::info;

// Type Aliases
type SharedRef<T> = Arc<Mutex<T>>;
type HandlerList = SharedRef<[Box<dyn InputHandler + Send>; 4]>;

// Structs
pub struct SettingsManager {
    handlers: HandlerList,
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
        ]));
        SettingsManager { handlers }
    }

    pub fn start_monitoring(&mut self) -> Result<(), Box<dyn Error + '_>> {
        let mut handlers_lock = self.handlers.lock()?;
        for handle in handlers_lock.iter_mut() {
            handle.apply_all()?;
            handle.monitor_gsettings_change();
        }

        let handlers_sref = self.handlers.clone();
        thread::spawn(move || Self::monitor_swayinput_events(handlers_sref));
        Ok(())
    }

    fn monitor_swayinput_events(mut handlers_sref: HandlerList) {
        let event_stream = utils::retry_action(
            utils::get_new_inputevent_stream,
            5,
            Duration::from_millis(500),
        );
        let mut is_allow_sync = true;
        for event in event_stream {
            match event {
                Ok(Event::Input(event)) if is_allow_sync => {
                    utils::sync_input_gsettings(&mut handlers_sref, &event.input).unwrap()
                }
                Ok(Event::Tick(TickEvent {
                    payload,
                    first: false,
                    ..
                })) => {
                    use SwayReloadStatus::{ReloadDone, ReloadPending};
                    match serde_json::from_str::<SwayReloadTick>(&payload) {
                        Ok(SwayReloadTick {
                            status: ReloadPending,
                        }) => {
                            is_allow_sync = false;
                            info!("Recieved tick, allow_sync = {is_allow_sync}");
                        }
                        Ok(SwayReloadTick { status: ReloadDone }) => {
                            thread::sleep(Duration::from_millis(100));
                            is_allow_sync = true;
                            info!("Sway reload done - Reapplying configurations from gsettings");
                            let mut handlers_lock = handlers_sref.lock().expect("Acquired lock for handers_sref");
                            for handle in handlers_lock.iter_mut() {
                                handle.apply_all().expect("Failed to re-apply configs from gsettings");
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

