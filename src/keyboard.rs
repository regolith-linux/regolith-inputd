use crate::InputHandler;
use gio::{prelude::SettingsExtManual, Settings};
use std::{
    error::Error,
    sync::mpsc::{self, Receiver, Sender},
};
use swayipc::{Connection as SwayConnection, Input};

pub struct KeyboardHandler {
    settings: Settings,
    sway_connection: SwayConnection,
    tx: Sender<Input>,
    rx: Receiver<Input>,
}
impl KeyboardHandler {
    pub fn new() -> KeyboardHandler {
        let settings = Settings::new("org.gnome.desktop.peripherals.keyboard");
        let sway_connection = SwayConnection::new().unwrap();
        let (tx, rx) = mpsc::channel();
        KeyboardHandler {
            settings,
            sway_connection,
            tx,
            rx,
        }
    }
    fn set_repeat_interval(&mut self) -> Result<(), Box<dyn Error>> {
        let interval: u32 = self.settings().get("repeat-interval");
        let repeat_freq = 1000f64 / interval as f64;
        let cmd = format!("input type:keyboard repeat_rate {repeat_freq}");
        self.sway_connection().run_command(cmd)?;
        Ok(())
    }
    fn set_repeat_delay(&mut self) -> Result<(), Box<dyn Error>> {
        let delay: u32 = self.settings().get("delay");
        let cmd = format!("input type:keyboard repeat_delay {delay}");
        self.sway_connection().run_command(cmd)?;
        Ok(())
    }
}

impl InputHandler for KeyboardHandler {
    fn apply_changes(&mut self, key: &str) -> Result<(), Box<dyn Error>> {
        match key {
            "repeat-interval" => self.set_repeat_interval()?,
            "delay" => self.set_repeat_delay()?,
            _ => (),
        };
        Ok(())
    }
    fn settings(&self) -> &Settings {
        &self.settings
    }
    fn sway_connection(&mut self) -> &mut swayipc::Connection {
        &mut self.sway_connection
    }
    fn get_swayinput_tx(&self) -> Sender<Input> {
        self.tx.clone()
    }
    fn get_swayinput_rx(&self) -> &Receiver<Input> {
        &self.rx
    }
    fn apply_all(&mut self) -> Result<(), Box<dyn Error>> {
        Ok(())
    }
    fn monitor_sway_inputs(&self) {}
}
unsafe impl Send for KeyboardHandler {}
