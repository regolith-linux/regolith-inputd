mod cursor;
mod input_method;
mod input_sources;
mod keyboard;
mod mouse;
mod shortcuts;
pub mod source_switching;
mod touchpad;
mod touchscreen;
mod trackball;
mod traits;
mod utils;

use cursor::CursorHandler;
use input_sources::InputSourcesHandler;
use keyboard::KeyboardHandler;
use log::info;
use log::{debug, error, warn};
use mouse::MouseHandler;
use serde::Deserialize;
use shortcuts::ShortcutsHandler;
use std::error::Error;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::thread;
use std::time::Duration;
use swayipc::{Connection as SwayConnection, Input, TickEvent};
use touchpad::TouchpadHandler;
use touchscreen::TouchscreenHandler;
use trackball::TrackballHandler;
use traits::InputHandler;
use utils::SwayEvent;

static mut ALLOW_SWAYINPUT_APPLY: AtomicBool = AtomicBool::new(true);
static mut ALLOW_GSETTINGS_APPLY: AtomicBool = AtomicBool::new(true);

// Type Aliases
type SharedRef<T> = Arc<Mutex<T>>;
type HandlerList = SharedRef<Vec<Box<dyn InputHandler + Send>>>;

// Structs
pub struct SettingsManager {
    handlers: HandlerList,
    touchscreens: Option<Arc<TouchscreenHandler>>,
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
        let mut handlers: Vec<Box<dyn InputHandler + Send>> = vec![
            Box::new(MouseHandler::new()),
            Box::new(KeyboardHandler::new()),
            Box::new(TouchpadHandler::new()),
            Box::new(InputSourcesHandler::new()),
            Box::new(CursorHandler::new()),
        ];
        match TrackballHandler::new() {
            Some(trackball) => handlers.push(Box::new(trackball)),
            None => warn!("Trackball schema not found, trackball settings are not mirrored"),
        }
        let touchscreens = TouchscreenHandler::new();
        if touchscreens.is_none() {
            warn!("Touchscreen schema not found, touchscreens are not mapped");
        }
        match ShortcutsHandler::new() {
            Some(shortcuts) => handlers.push(Box::new(shortcuts)),
            None => warn!("Media keys schema not found, keyboard shortcuts are not mirrored"),
        }
        SettingsManager {
            handlers: Arc::new(Mutex::new(handlers)),
            touchscreens,
        }
    }

    pub fn start_monitoring(&mut self) -> Result<(), Box<dyn Error + '_>> {
        let mut handlers_lock = self.handlers.lock()?;
        for handle in handlers_lock.iter_mut() {
            handle.apply_all_sync()?;
            handle.monitor_gsettings_change();
        }
        drop(handlers_lock);
        if let Some(touchscreens) = &self.touchscreens {
            if let Err(e) = touchscreens.apply_all() {
                error!("Failed to map touchscreens: {e}");
            }
        }

        let handlers_sref = self.handlers.clone();
        let touchscreens = self.touchscreens.clone();
        thread::spawn(move || Self::monitor_swayinput_events(handlers_sref, touchscreens));
        Ok(())
    }

    /// Configure a device sway reports as added, in every handler that
    /// configures devices one by one.
    fn device_added(
        handlers_sref: &HandlerList,
        touchscreens: Option<&TouchscreenHandler>,
        device: &serde_json::Value,
    ) {
        let mut handlers_lock = handlers_sref
            .lock()
            .expect("Acquired lock for handers_sref");
        for handle in handlers_lock.iter_mut() {
            if let Err(e) = handle.device_added(device) {
                error!("Failed to configure an added device: {e}");
            }
        }
        drop(handlers_lock);
        if let Some(Err(e)) = touchscreens.map(|t| t.device_added(device)) {
            error!("Failed to map an added touchscreen: {e}");
        }
    }

    fn monitor_swayinput_events(
        mut handlers_sref: HandlerList,
        touchscreens: Option<Arc<TouchscreenHandler>>,
    ) {
        let mut event_stream =
            utils::retry_action(utils::subscribe_events, 5, Duration::from_millis(500));
        loop {
            // A message that doesn't decode was still read whole: the stream
            // stays in step.
            let event = match utils::receive_message(&mut event_stream) {
                Ok(message) => utils::decode_event(message),
                Err(e) => {
                    warn!("{e}");
                    break;
                }
            };
            match event {
                Ok(SwayEvent::Input { change, input }) => {
                    // Hardware that appears gets its per-device settings
                    // whatever the reload and sync state.
                    if change == "added" {
                        Self::device_added(&handlers_sref, touchscreens.as_deref(), &input);
                    }
                    if !unsafe { ALLOW_SWAYINPUT_APPLY.load(Ordering::Relaxed) } {
                        continue;
                    }
                    // swayipc's typed `Input` requires vendor/product ids.
                    let Ok(input) = serde_json::from_value::<Input>(input) else {
                        continue;
                    };
                    if let Err(e) = utils::sync_input_gsettings(&mut handlers_sref, &input) {
                        debug!("{e}");
                    }
                }
                Ok(SwayEvent::Output) => {
                    if let Some(Err(e)) = touchscreens.as_ref().map(|t| t.outputs_changed()) {
                        error!("Failed to remap touchscreens after an output change: {e}");
                    }
                }
                Ok(SwayEvent::Tick(TickEvent {
                    payload,
                    first: false,
                    ..
                })) => {
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
                                // One failing handler must not end reload handling.
                                if let Err(e) = handle.apply_all_sync() {
                                    error!("Failed to re-apply configs from gsettings: {e}");
                                }
                            }
                            drop(handlers_lock);
                            if let Some(Err(e)) = touchscreens.as_ref().map(|t| t.reload()) {
                                error!("Failed to re-map touchscreens: {e}");
                            }
                        }
                        Err(e) => debug!("Invalid Payload Recieved: {e}"),
                    }
                }
                Err(e) => debug!("{e}"),
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
