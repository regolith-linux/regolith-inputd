use crate::traits::{InputHandler, PointerMethods, PrimitiveToSwayType, SwayTypeToPrimitive};
use gio::{prelude::SettingsExtManual, traits::SettingsExt, Settings};
use log::debug;

use std::{
    error::Error,
    sync::mpsc::{self, Receiver, Sender},
};
use swayipc::{Connection as SwayConnection, Input};

pub struct TouchpadHandler {
    settings: Settings,
    sway_connection: SwayConnection,
    tx: Sender<Input>,
    rx: Receiver<Input>,
}
impl TouchpadHandler {
    pub fn new() -> TouchpadHandler {
        let settings = Settings::new("org.gnome.desktop.peripherals.touchpad");
        let connection = SwayConnection::new().unwrap();
        let (tx, rx) = mpsc::channel();
        let handler = TouchpadHandler {
            settings,
            sway_connection: connection,
            tx,
            rx,
        };
        handler
    }
    fn apply_tap(&mut self) -> Result<(), Box<dyn Error>> {
        let tap_enabled: &str = self
            .settings
            .get::<bool>("tap-to-click")
            .to_sway_type()
            .to_primitive();
        let cmd = format!("input type:touchpad tap {tap_enabled}");
        self.sway_connection.run_command(cmd)?;
        Ok(())
    }
    fn apply_scroll_method(&mut self) -> Result<(), Box<dyn Error>> {
        let two_finger: bool = self.settings().get("two-finger-scrolling-enabled");
        let edge_scroll: bool = self.settings().get("edge-scrolling-enabled");
        let scroll_method = if two_finger {
            "two_finger"
        } else if edge_scroll {
            "edge"
        } else {
            "none"
        };
        let cmd = format!("input type:touchpad scroll_method {scroll_method}");
        self.sway_connection.run_command(cmd)?;
        Ok(())
    }
    fn use_dwt(&mut self) -> Result<(), Box<dyn Error>> {
        let dwt_enabled: &str = self
            .settings
            .get::<bool>("disable-while-typing")
            .to_sway_type()
            .to_primitive();
        let cmd = format!("input type:touchpad dwt {dwt_enabled}");
        self.sway_connection.run_command(cmd)?;
        Ok(())
    }
    fn send_events(&mut self) -> Result<(), Box<dyn Error>> {
        let new_val: String = self.settings().get("send-events");
        let cmd = format!("input type:touchpad events {new_val}");
        debug!("{cmd}");
        self.sway_connection.run_command(cmd)?;
        Ok(())
    }
    fn use_drag(&mut self) -> Result<(), Box<dyn Error>> {
        let use_drag_enabled: &str = self
            .settings
            .get::<bool>("tap-and-drag")
            .to_sway_type()
            .to_primitive();
        let cmd = format!("input type:touchpad drag {use_drag_enabled}");
        debug!("{cmd}");
        self.sway_connection.run_command(cmd)?;
        Ok(())
    }
    fn use_drag_lock(&mut self) -> Result<(), Box<dyn Error>> {
        let drag_lock_enabled: &str = self
            .settings
            .get::<bool>("tap-and-drag-lock")
            .to_sway_type()
            .to_primitive();
        let cmd = format!("input type:touchpad drag_lock {drag_lock_enabled}");
        debug!("{cmd}");
        self.sway_connection.run_command(cmd)?;
        Ok(())
    }

    fn emulate_middle_click(&mut self) -> Result<(), Box<dyn Error>> {
        let emulate_middle_enabled: &str = self
            .settings
            .get::<bool>("middle-click-emulation")
            .to_sway_type()
            .to_primitive();
        let cmd = format!("input type:touchpad middle_emulation {emulate_middle_enabled}");
        self.sway_connection.run_command(cmd)?;
        Ok(())
    }
}

impl PointerMethods for TouchpadHandler {
    fn pointer_type(&self) -> &str {
        "touchpad"
    }
}

impl InputHandler for TouchpadHandler {
    fn apply_changes(&mut self, key: &str) -> Result<(), Box<dyn Error>> {
        debug!("{key}");
        match key {
            "speed" => self.apply_speed()?,
            "natural-scroll" => self.apply_natural_scroll()?,
            "tap-to-click" => self.apply_tap()?,
            "two-finger-scrolling-enabled" | "edge-scrolling-enabled" => {
                self.apply_scroll_method()?
            }
            "left-handed" => self.apply_left_handed()?,
            "send-events" => self.send_events()?,
            "disable-while-typing" => self.use_dwt()?,
            "tap-and-drag" => self.use_drag()?,
            "tap-and-drag-lock" => self.use_drag_lock()?,
            "middle-click-emulation" => self.emulate_middle_click()?,
            _ => (),
        };
        Ok(())
    }
    fn get_swayinput_tx(&self) -> Sender<Input> {
        self.tx.clone()
    }
    fn settings(&self) -> &Settings {
        &self.settings
    }
    fn sway_connection(&mut self) -> &mut swayipc::Connection {
        &mut self.sway_connection
    }
    fn apply_all(&mut self) -> Result<(), Box<dyn Error>> {
        self.apply_speed()?;
        // self.apply_left_handed()?;
        self.apply_natural_scroll()?;
        self.apply_tap()?;
        self.apply_scroll_method()?;
        self.send_events()?;
        self.use_dwt()?;
        self.use_drag()?;
        self.use_drag_lock()?;
        self.emulate_middle_click()?;
        Ok(())
    }
    fn sync_gsettings(&mut self, input: Input) -> Result<(), Box<dyn Error>> {
        debug!("Sync touchpad gsettings");
        debug!("Received at ");
        line!();
        self.sync_pointer_gsettings(&input)?;
        if input.libinput.is_none() {
            return Ok(());
        }
        let libinput = input.libinput.unwrap();
        if let Some(tap) = libinput.tap {
            self.settings()
                .set_boolean("tap-to-click", tap.to_primitive())?;
        }
        if let Some(drag) = libinput.tap_drag {
            self.settings()
                .set_boolean("tap-and-drag", drag.to_primitive())?;
        }
        if let Some(drag_lock) = libinput.tap_drag_lock {
            self.settings()
                .set_boolean("tap-and-drag-lock", drag_lock.to_primitive())?;
        }

        debug!("Exiting touchpad sync gsettings");
        Ok(())
    }
    fn get_swayinput_rx(&self) -> &Receiver<Input> {
        &self.rx
    }
    fn monitor_sway_inputs(&self) {}
}
unsafe impl Send for TouchpadHandler {}
unsafe impl Sync for TouchpadHandler {}
