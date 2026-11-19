use crate::sway;
use std::io::Read;
use std::os::unix::net::UnixStream;
use swayipc::{Error as SwayError, Fallible, Input, MAGIC};

const WORKSPACE_EVENT: u32 = 0x8000_0000;
const TICK_EVENT: u32 = 0x8000_0007;
const INPUT_EVENT: u32 = 0x8000_0015;

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
/// X11/Wayland backends' devices have neither. They are the current sway's.
pub fn get_inputs_json() -> Fallible<Vec<serde_json::Value>> {
    sway::current().ok_or_else(sway::gone)?.get_inputs_json()
}

/// sway's outputs, untyped, disabled ones included. swayipc's `Output`
/// requires fields sway leaves out for some outputs. They are the current
/// sway's.
pub fn get_outputs_json() -> Fallible<Vec<serde_json::Value>> {
    sway::current().ok_or_else(sway::gone)?.get_outputs_json()
}

/// The input devices swayipc's `Input` can describe; the others are left out.
pub fn get_inputs() -> Fallible<Vec<Input>> {
    Ok(get_inputs_json()?
        .into_iter()
        .filter_map(|i| serde_json::from_value(i).ok())
        .collect())
}

/// An event from sway, as far as the daemon cares.
pub enum SwayEvent {
    /// An input device was configured. What the event reports isn't kept:
    /// by the time it is read it may be out of date.
    Input,
    /// An input device was added or removed.
    Hotplug,
    /// A tick sent with `swaymsg -t send_tick`, with its payload.
    Tick(String),
    /// sway loaded its configuration again.
    WorkspaceReload,
    Other,
}

/// What an event the daemon subscribed to with its input, tick and workspace
/// events is to it. The event's payload was read whole, so one that doesn't
/// decode (devices without vendor or product ids) still gets through.
pub(crate) fn decode_event(kind: u32, payload: &[u8]) -> SwayEvent {
    let payload: serde_json::Value = serde_json::from_slice(payload).unwrap_or_default();
    match kind {
        INPUT_EVENT if payload["change"] == "added" || payload["change"] == "removed" => {
            SwayEvent::Hotplug
        }
        INPUT_EVENT => SwayEvent::Input,
        // The tick sent to every new subscriber is no reload.
        TICK_EVENT if payload["first"] == false => match payload["payload"].as_str() {
            Some(tick) => SwayEvent::Tick(tick.to_string()),
            None => SwayEvent::Other,
        },
        WORKSPACE_EVENT if payload["change"] == "reload" => SwayEvent::WorkspaceReload,
        _ => SwayEvent::Other,
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
