use crate::traits::{InputHandler, PointerMethods};
use gio::Settings;
use log::debug;
use std::{
    error::Error,
    sync::mpsc::{self, Receiver, Sender},
};
use swayipc::{Connection as SwayConnection, Input};
pub struct MouseHandler {
    settings: Settings,
    sway_connection: SwayConnection,
    tx: Sender<Input>,
    rx: Receiver<Input>,
}
impl MouseHandler {
    pub fn new() -> MouseHandler {
        let settings = Settings::new("org.gnome.desktop.peripherals.mouse");
        let sway_connection = SwayConnection::new().unwrap();
        let (tx, rx) = mpsc::channel();
        MouseHandler {
            settings,
            sway_connection,
            tx,
            rx,
        }
    }
}

impl PointerMethods for MouseHandler {}

impl InputHandler for MouseHandler {
    fn apply_changes(&mut self, key: &str) -> Result<(), Box<dyn Error>> {
        debug!("{key}");
        match key {
            "speed" => self.apply_speed()?,
            "natural-scroll" => self.apply_natural_scroll()?,
            "left-handed" => self.apply_left_handed()?,
            _ => (),
        };
        Ok(())
    }
    fn settings(&self) -> &Settings {
        &self.settings
    }
    fn get_swayinput_tx(&self) -> Sender<Input> {
        self.tx.clone()
    }
    fn get_swayinput_rx(&self) -> &Receiver<Input> {
        &self.rx
    }
    fn sway_connection(&mut self) -> &mut swayipc::Connection {
        &mut self.sway_connection
    }
    fn apply_all(&mut self) -> Result<(), Box<dyn Error>> {
        self.apply_speed()?;
        self.apply_left_handed()?;
        self.apply_natural_scroll()?;
        Ok(())
    }
    fn sync_gsettings(&mut self, _input: Input) -> Result<(), Box<dyn Error>> {
        let rx_ref = self.get_swayinput_rx();
        for input in rx_ref {
            self.sync_pointer_gsettings(&input)?;
        }
        Ok(())
    }
}

unsafe impl Send for MouseHandler {}
