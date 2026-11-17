use log::{error, info, warn};
use serde::Deserialize;
use serde_json::Value;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::{error::Error, fmt::Display, thread, time::Duration};
use swayipc::{
    CommandType, Connection as SwayConnection, Error as SwayError, EventType, Fallible, Input,
    TickEvent, MAGIC,
};

use crate::HandlerList;

const OUTPUT_EVENT: u32 = 0x8000_0001;
const TICK_EVENT: u32 = 0x8000_0007;
const INPUT_EVENT: u32 = 0x8000_0015;

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

/// sway's outputs, untyped, disabled ones included. swayipc's `Output`
/// requires fields sway leaves out for some outputs.
pub fn get_outputs_json() -> Fallible<Vec<serde_json::Value>> {
    let mut stream: UnixStream = SwayConnection::new()?.into();
    stream.write_all(&CommandType::GetOutputs.encode())?;
    CommandType::GetOutputs.decode(receive_message(&mut stream)?)
}

/// The input devices swayipc's `Input` can describe; the others are left out.
pub fn get_inputs() -> Fallible<Vec<Input>> {
    Ok(get_inputs_json()?
        .into_iter()
        .filter_map(|i| serde_json::from_value(i).ok())
        .collect())
}

/// A string field of a GET_INPUTS or GET_OUTPUTS object, or "".
pub fn str_field<'a>(object: &'a Value, field: &str) -> &'a str {
    object[field].as_str().unwrap_or_default()
}

/// Whether a GET_INPUTS device is a trackball: a pointer whose name says so,
/// whatever its capitalisation, as GNOME tells trackballs from mice.
pub fn is_trackball(device: &Value) -> bool {
    str_field(device, "type") == "pointer"
        && str_field(device, "name")
            .to_lowercase()
            .contains("trackball")
}

/// Whether sway reports `setting` among a device's libinput settings. sway
/// lists only those the device supports.
pub fn offers(device: &Value, setting: &str) -> bool {
    device["libinput"].get(setting).is_some()
}

/// Run `commands` as one command list, so no other client's command can come
/// between them. A command sway rejects is logged; the others still run.
pub fn run_command_list(connection: &mut SwayConnection, commands: &[String]) -> Fallible<()> {
    if commands.is_empty() {
        return Ok(());
    }
    let list = commands.join("; ");
    info!("Executing command: {list}");
    for outcome in connection.run_command(&list)? {
        if let Err(e) = outcome {
            warn!("sway rejected part of {list:?}: {e}");
        }
    }
    Ok(())
}

/// [`run_command_list`] on a connection of its own, for callers that may run
/// alongside a handler's own commands.
pub fn run_commands(commands: &[String]) -> Fallible<()> {
    if commands.is_empty() {
        return Ok(());
    }
    run_command_list(&mut SwayConnection::new()?, commands)
}

/// The sway events the daemon follows.
pub enum SwayEvent {
    /// An input device changed; `input` is its GET_INPUTS object.
    Input {
        change: String,
        input: Value,
    },
    Tick(TickEvent),
    /// Outputs were connected, disconnected or changed.
    Output,
}

#[derive(Deserialize)]
struct RawInputEvent {
    change: String,
    input: Value,
}

/// Subscribe to input, tick and output events. The stream is read by hand:
/// swayipc has no output event, and its typed input events require
/// vendor/product ids.
pub fn subscribe_events() -> Fallible<UnixStream> {
    let mut stream: UnixStream = SwayConnection::new()?.into();
    let events = serde_json::to_string(&[
        serde_json::to_value(EventType::Input)?,
        serde_json::to_value(EventType::Tick)?,
        Value::from("output"),
    ])?;
    stream.write_all(&CommandType::Subscribe.encode_with(&events))?;
    let reply: Value = CommandType::Subscribe.decode(receive_message(&mut stream)?)?;
    if reply["success"] != true {
        return Err(SwayError::SubscriptionFailed(events));
    }
    Ok(stream)
}

/// Decode a message read from a [`subscribe_events`] stream.
pub fn decode_event((kind, payload): (u32, Vec<u8>)) -> Fallible<SwayEvent> {
    Ok(match kind {
        INPUT_EVENT => {
            let RawInputEvent { change, input } = serde_json::from_slice(&payload)?;
            SwayEvent::Input { change, input }
        }
        TICK_EVENT => SwayEvent::Tick(serde_json::from_slice(&payload)?),
        OUTPUT_EVENT => SwayEvent::Output,
        _ => return Err(SwayError::UnimplementedEvent(kind & 0x7fff_ffff, payload)),
    })
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
