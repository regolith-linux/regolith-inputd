use crate::InputHandler;
use async_trait::async_trait;
use gio::{prelude::SettingsExtManual, Settings};
use swayipc_async::Connection as SwayConnection;
use tokio::task::spawn_blocking;

pub struct TouchpadHandler {
    settings: Settings,
    sway_connection: SwayConnection,
}

impl TouchpadHandler {
    pub async fn new() -> TouchpadHandler {
        let settings = Settings::new("org.gnome.desktop.peripherals.touchpad");
        let connection = SwayConnection::new().await.unwrap();
        TouchpadHandler {
            settings,
            sway_connection: connection,
        }
    }
}

impl InputHandler for TouchpadHandler {
    fn apply_changes(&mut self, key: &str) {
        match key {
            "speed" => {
                let new_val: f64 = self.settings.get(key);
                let cmd = format!("input type:touchpad pointer_accel {new_val}");
                self.sway_connection.run_command(cmd);
            }
            _ => (),
        };
    }
    fn settings(&self) -> &Settings {
        &self.settings
    }
    fn monitor_sway_inputs(&self) {}
}
