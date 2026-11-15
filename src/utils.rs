use log::{error, info, warn};
use std::env;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::Command;
use std::{error::Error, fmt::Display, thread, time::Duration};
use swayipc::{
    CommandType, Connection as SwayConnection, Error as SwayError, Event, Fallible, Input, Success,
    MAGIC,
};

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

/// Read one whole sway IPC message: its type and its payload. The payload is
/// read in full before it is decoded, so one that doesn't decode never
/// desynchronises the stream.
pub fn receive_message(stream: &mut UnixStream) -> Fallible<(u32, Vec<u8>)> {
    let mut header = [0_u8; 14];
    stream.read_exact(&mut header)?;
    if header[..6] != MAGIC {
        let mut magic = [0_u8; 6];
        magic.copy_from_slice(&header[..6]);
        return Err(SwayError::InvalidMagic(magic));
    }
    let len = u32::from_ne_bytes([header[6], header[7], header[8], header[9]]);
    let kind = u32::from_ne_bytes([header[10], header[11], header[12], header[13]]);
    let mut payload = vec![0_u8; len as usize];
    stream.read_exact(&mut payload)?;
    Ok((kind, payload))
}

/// sway's input devices, untyped. swayipc's `Input` requires `vendor` and
/// `product`, which sway reports for libinput devices only: virtual
/// keyboards (on-screen keyboards, remote-desktop servers) and the wlroots
/// X11/Wayland backends' devices have neither.
pub fn get_inputs_json() -> Fallible<Vec<serde_json::Value>> {
    let mut stream: UnixStream = SwayConnection::new()?.into();
    stream.write_all(&CommandType::GetInputs.encode())?;
    CommandType::GetInputs.decode(receive_message(&mut stream)?)
}

/// The input devices swayipc's `Input` can describe; the others are left out.
pub fn get_inputs() -> Fallible<Vec<Input>> {
    Ok(get_inputs_json()?
        .into_iter()
        .filter_map(|i| serde_json::from_value(i).ok())
        .collect())
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
        let reply: Success = CommandType::Subscribe.decode(receive_message(&mut stream)?)?;
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
        match receive_message(stream) {
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

#[cfg(test)]
mod tests {
    use super::receive_message;
    use std::io::Write;
    use std::os::unix::net::UnixStream;
    use swayipc::MAGIC;

    fn message(kind: u32, payload: &[u8]) -> Vec<u8> {
        let mut msg = MAGIC.to_vec();
        msg.extend_from_slice(&(payload.len() as u32).to_ne_bytes());
        msg.extend_from_slice(&kind.to_ne_bytes());
        msg.extend_from_slice(payload);
        msg
    }

    #[test]
    fn messages_are_read_whole_whatever_their_payload() {
        let (mut sway, mut daemon) = UnixStream::pair().unwrap();
        // An input event swayipc's typed `Input` can't decode, then another.
        let virtual_keyboard = br#"{"change": "added", "input": {"identifier": "0:0:wlr_virtual_keyboard_v1", "type": "keyboard"}}"#;
        sway.write_all(&message(0x8000_0015, virtual_keyboard))
            .unwrap();
        sway.write_all(&message(
            0x8000_0007,
            br#"{"first": false, "payload": "x"}"#,
        ))
        .unwrap();
        assert_eq!(
            receive_message(&mut daemon).unwrap(),
            (0x8000_0015, virtual_keyboard.to_vec())
        );
        assert_eq!(receive_message(&mut daemon).unwrap().0, 0x8000_0007);
        sway.write_all(b"not-ipc-at-all").unwrap();
        assert!(receive_message(&mut daemon).is_err());
    }
}
