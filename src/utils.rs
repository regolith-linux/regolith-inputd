use log::{error, info, warn};
use std::env;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::Command;
use std::{error::Error, fmt::Display, thread, time::Duration};
use swayipc::{CommandType, Error as SwayError, Event, Fallible, Input, Success, MAGIC};

use crate::HandlerList;

pub fn sync_input_gsettings<'a>(
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
    handlers_lock[handler_index].sync_gsettings_sync(input)?;
    Ok(())
}

/// An event from sway.
pub enum SwayEvent {
    /// An event swayipc decodes.
    Event(Event),
    /// An output was added, removed or reconfigured. swayipc 3 has no type
    /// for these events.
    Output,
}

/// The input, tick and output events sway sends.
///
/// swayipc 3 cannot subscribe to output events, so this subscribes over its
/// own connection and hands every other event to swayipc's decoder. All
/// three kinds are subscribed to in one request, so once sway has answered
/// none of them can be missed.
pub struct SwayEventStream(Option<UnixStream>);

const EVENT_FLAG: u32 = 1 << 31;
const OUTPUT_EVENT: u32 = EVENT_FLAG | 1;

impl SwayEventStream {
    pub fn subscribe() -> Fallible<SwayEventStream> {
        let events = r#"["input","tick","output"]"#;
        let mut stream = UnixStream::connect(sway_socket_path()?)?;
        stream.write_all(&CommandType::Subscribe.encode_with(events))?;
        let reply: Success = CommandType::Subscribe.decode(read_message(&mut stream)?)?;
        if !reply.success {
            return Err(SwayError::SubscriptionFailed(events.to_string()));
        }
        Ok(SwayEventStream(Some(stream)))
    }
}

impl Iterator for SwayEventStream {
    type Item = Fallible<SwayEvent>;

    /// Ends when the connection to sway is lost.
    fn next(&mut self) -> Option<Self::Item> {
        let stream = self.0.as_mut()?;
        match read_message(stream) {
            Ok((OUTPUT_EVENT, _)) => Some(Ok(SwayEvent::Output)),
            Ok(message) => Some(Event::decode(message).map(SwayEvent::Event)),
            Err(e) => {
                error!("Lost the connection to sway: {e}");
                self.0 = None;
                None
            }
        }
    }
}

/// Finds sway's IPC socket the way swayipc does.
fn sway_socket_path() -> io::Result<PathBuf> {
    if let Some(path) = env::var_os("I3SOCK").or_else(|| env::var_os("SWAYSOCK")) {
        return Ok(path.into());
    }
    let output = Command::new("sway").arg("--get-socketpath").output()?;
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if path.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "sway socket not found",
        ));
    }
    Ok(path.into())
}

/// Reads one IPC message: its type and payload.
fn read_message(stream: &mut UnixStream) -> Fallible<(u32, Vec<u8>)> {
    let mut header = [0_u8; 14];
    stream.read_exact(&mut header)?;
    let magic: [u8; 6] = header[..6].try_into().unwrap();
    if magic != MAGIC {
        return Err(SwayError::InvalidMagic(magic));
    }
    let length = u32::from_ne_bytes(header[6..10].try_into().unwrap());
    let message_type = u32::from_ne_bytes(header[10..14].try_into().unwrap());
    let mut payload = vec![0_u8; length as usize];
    stream.read_exact(&mut payload)?;
    Ok((message_type, payload))
}

pub fn retry_action<F, T, E>(action: F, max_retry: usize, duration_before_retry: Duration) -> T
where
    F: Fn() -> Result<T, E>,
    E: Display,
{
    let mut retries_left = max_retry;
    loop {
        match action() {
            Ok(res) => break res,
            Err(e) => {
                if retries_left == 0 {
                    error!("{e}");
                    panic!();
                }
                warn!("{e}");
                retries_left -= 1;
                thread::sleep(duration_before_retry);
            }
        }
    }
}
