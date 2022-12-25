mod input_sources;
mod keyboard;
mod mouse;
mod touchpad;
mod traits;

use input_sources::InputSourcesHandler;
use keyboard::KeyboardHandler;
use log::{debug, info, warn};
use mouse::MouseHandler;
use serde::Deserialize;
use std::error::Error;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use swayipc::{
    Connection as SwayConnection, Event, EventStream, EventType, Fallible, Input, TickEvent,
};
use touchpad::TouchpadHandler;
use traits::InputHandler;

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
        let mut retry_count = 5;
        loop {
            let connection = SwayConnection::new();
            if connection.is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
            retry_count -= 1;
            if retry_count == 0 {
                panic!("Failed to start regolith-inputd: cannot connect to sway IPC");
            }
        }
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

        let mut handlers_sref = self.handlers.clone();
        thread::spawn(move || {
            let mut retry_count = 0;
            let event_stream = loop {
                match get_new_inputevent_stream() {
                    Ok(stream) => break stream,
                    Err(e) => {
                        // Report Error and retry connection
                        warn!("Failed to subscribe to sway input event: {e}. Retrying...");
                        if retry_count < 5 {
                            thread::sleep(Duration::from_secs(retry_count));
                            retry_count += 1;
                        } else {
                            std::process::exit(1);
                        }
                    }
                }
            };
            let mut is_allow_sync = true;
            for event in event_stream {
                match event {
                    Ok(Event::Input(event)) => {
                        if is_allow_sync {
                            sync_input_gsettings(&mut handlers_sref, &event.input).unwrap()
                        }
                    }
                    Ok(Event::Tick(TickEvent {
                        payload,
                        first: false,
                        ..
                    })) => {
                        let reload_tick = if let Ok(reload_tick) =
                            serde_json::from_str::<SwayReloadTick>(&payload)
                        {
                            reload_tick
                        } else {
                            debug!("Invalid payload recieved: {payload}");
                            continue;
                        };
                        debug!("Tick recieved: {reload_tick:#?}");
                        if reload_tick.status == SwayReloadStatus::ReloadPending {
                            is_allow_sync = false
                        } else if reload_tick.status == SwayReloadStatus::ReloadDone {
                            is_allow_sync = true
                        }
                    }
                    Err(e) => warn!("{e}"),
                    _ => continue,
                }
            }
        });
        Ok(())
    }
}

// Functions
fn sync_input_gsettings<'a>(
    handlers_sref: &'a mut HandlerList,
    input: &Input,
) -> Result<(), Box<dyn Error + 'a>> {
    let input_type = input.input_type.clone();
    let handler_index = match input_type.as_ref() {
        "pointer" => 0,
        "keyboard" => 1,
        "touchpad" => 2,
        _ => return Err("Incompatible input type".into()),
    };
    info!("Recieved Sway InputEvent for {}", input.input_type);
    let mut handlers_lock = handlers_sref.lock()?;
    handlers_lock[handler_index].sync_gsettings(input)?;
    Ok(())
}

fn get_new_inputevent_stream() -> Fallible<EventStream> {
    let connection = SwayConnection::new()?;
    let subs = [EventType::Input, EventType::Tick];
    connection.subscribe(subs)
}
