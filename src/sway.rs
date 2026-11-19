//! Finding sway, and the sway instance the daemon follows.
//!
//! sway can start after the daemon, exit and be replaced (at the same socket
//! or at a new one), drop one of the daemon's connections or stop answering.
//! Every instance the daemon reaches is a [`Sway`], with which each of the
//! daemon's connections to it is registered. Losing any of them loses the
//! instance: all of its connections are closed, and the owner of the
//! handlers (see `lib.rs`) is told, looks for sway again and treats the next
//! instance it reaches as a first launch.

use crate::{utils, Message};
use gio::{BusType, Cancellable, DBusCallFlags};
use glib::{ToVariant, VariantTy};
use log::debug;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::env;
use std::error::Error;
use std::fmt::Display;
use std::io::{self, ErrorKind, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Duration;
use swayipc::{CommandOutcome, CommandType, Error as SwayError, Fallible, Node, Success};

/// How long a request may go unanswered before sway counts as gone. A reply
/// within two seconds is sway being slow, not gone.
pub const REQUEST_TIMEOUT: Duration = Duration::from_millis(4500);

/// How long a socket may take to answer the first request before the sway
/// behind it counts as unreachable.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// How long the user manager may take to report its environment.
const MANAGER_TIMEOUT_MS: i32 = 1000;

const MANAGER_NAME: &str = "org.freedesktop.systemd1";
const MANAGER_PATH: &str = "/org/freedesktop/systemd1";
const MANAGER_INTERFACE: &str = "org.freedesktop.systemd1.Manager";

/// The sway the daemon follows, while one is reachable.
static CURRENT: Mutex<Option<Arc<Sway>>> = Mutex::new(None);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The sockets sway may be listening on, in the order to try them: the
/// `SWAYSOCK` in the user manager's environment, then the daemon's own.
///
/// Both are read again on every call: sway can start, or move to another
/// socket, at any time, and the user manager announces no change to its
/// environment. `I3SOCK` is i3's socket, never sway's, wherever it points.
pub fn candidates() -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    let sockets = [
        manager_swaysock().map(PathBuf::from),
        env::var_os("SWAYSOCK").map(PathBuf::from),
    ];
    for path in sockets.into_iter().flatten() {
        if !path.as_os_str().is_empty() && !candidates.contains(&path) {
            candidates.push(path);
        }
    }
    candidates
}

/// The `SWAYSOCK` the systemd user manager on the session bus has in its
/// environment, where the session announces the socket of the sway it
/// started.
fn manager_swaysock() -> Option<String> {
    let bus = gio::bus_get_sync(BusType::Session, Cancellable::NONE).ok()?;
    let reply = bus.call_sync(
        Some(MANAGER_NAME),
        MANAGER_PATH,
        "org.freedesktop.DBus.Properties",
        "Get",
        Some(&(MANAGER_INTERFACE, "Environment").to_variant()),
        VariantTy::new("(v)").ok(),
        DBusCallFlags::NO_AUTO_START,
        MANAGER_TIMEOUT_MS,
        Cancellable::NONE,
    );
    let environment: Vec<String> = match reply {
        Ok(reply) => reply.child_value(0).as_variant()?.get()?,
        Err(e) => {
            debug!("No environment from the user manager: {e}");
            return None;
        }
    };
    environment
        .into_iter()
        .find_map(|entry| entry.strip_prefix("SWAYSOCK=").map(str::to_string))
}

/// The error for a request to a sway that is gone, or to none.
pub fn gone() -> SwayError {
    SwayError::Io(io::Error::new(ErrorKind::NotConnected, "no reachable sway"))
}

/// Whether `e` means the connection it happened on can't be used any more:
/// it failed, timed out, or lost track of where messages begin.
fn is_loss(e: &SwayError) -> bool {
    matches!(
        e,
        SwayError::Io(_) | SwayError::InvalidMagic(_) | SwayError::InvalidCommandType(..)
    )
}

/// Whether `e` is a request failing because sway is gone. The loss itself is
/// logged once, when the owner of the handlers learns of it, so the failures
/// it causes need not be.
pub fn is_gone(e: &(dyn Error + 'static)) -> bool {
    e.downcast_ref::<SwayError>().is_some_and(is_loss)
}

/// Why a failed request loses sway, for the log.
fn describe(e: &SwayError) -> String {
    match e {
        SwayError::Io(io) if matches!(io.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
            format!("no reply within {REQUEST_TIMEOUT:?}")
        }
        SwayError::Io(io) if io.kind() == ErrorKind::UnexpectedEof => {
            String::from("the connection was closed")
        }
        e => e.to_string(),
    }
}

/// The sway the daemon follows, if one is reachable.
pub fn current() -> Option<Arc<Sway>> {
    lock(&CURRENT).clone()
}

/// Make `sway` the one requests go to.
pub(crate) fn set_current(sway: Arc<Sway>) {
    *lock(&CURRENT) = Some(sway);
}

/// Stop sending requests to sway instance `generation`, if it is the
/// current one.
pub(crate) fn forget(generation: u64) {
    let mut current = lock(&CURRENT);
    if current.as_ref().is_some_and(|s| s.generation == generation) {
        *current = None;
    }
}

/// Run `command` on the current sway.
pub fn run_command<T: AsRef<str>>(command: T) -> Fallible<Vec<Fallible<()>>> {
    current().ok_or_else(gone)?.run_command(command)
}

/// The connection requests are made on. Its replies are read as they come,
/// on a thread of its own, so that sway closing it is noticed even while no
/// request is waiting.
struct Commands {
    stream: UnixStream,
    replies: Receiver<(u32, Vec<u8>)>,
}

/// A sway instance the daemon reached, and its connections to it.
pub struct Sway {
    /// Which of the instances reached since the start this is.
    generation: u64,
    path: PathBuf,
    /// The connection requests are made on, until sway is lost.
    commands: Mutex<Option<Commands>>,
    /// Every connection to this sway, to close when it is lost; `None` once
    /// it is.
    connections: Mutex<Option<Vec<UnixStream>>>,
    /// The owner of the handlers, told when this sway is lost.
    owner: Sender<Message>,
}

impl Sway {
    /// Connect to the sway listening on `path`. It counts as reachable only
    /// once it answers a request.
    pub(crate) fn connect(
        path: &Path,
        generation: u64,
        owner: Sender<Message>,
    ) -> Fallible<Arc<Sway>> {
        let mut stream = UnixStream::connect(path)?;
        stream.set_read_timeout(Some(PROBE_TIMEOUT))?;
        stream.set_write_timeout(Some(PROBE_TIMEOUT))?;
        stream.write_all(&CommandType::GetInputs.encode())?;
        let _: Vec<Value> = CommandType::GetInputs.decode(utils::receive_message(&mut stream)?)?;
        // From now on replies are waited for on the channel.
        stream.set_read_timeout(None)?;
        stream.set_write_timeout(Some(REQUEST_TIMEOUT))?;
        let mut reader = stream.try_clone()?;
        let registered = stream.try_clone()?;
        let (sender, replies) = mpsc::channel();
        let sway = Arc::new(Sway {
            generation,
            path: path.to_path_buf(),
            commands: Mutex::new(Some(Commands { stream, replies })),
            connections: Mutex::new(Some(vec![registered])),
            owner,
        });
        let lost = sway.clone();
        thread::spawn(move || loop {
            match utils::receive_message(&mut reader) {
                Ok(reply) => {
                    if sender.send(reply).is_err() {
                        return;
                    }
                }
                Err(e) => {
                    // A request waiting for its reply fails at once.
                    drop(sender);
                    return lost.lose(&describe(&e));
                }
            }
        });
        Ok(sway)
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The socket this sway was reached at.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Give this sway up: close every connection to it and tell the owner of
    /// the handlers why. Only the first call does anything.
    pub fn lose(&self, reason: &dyn Display) {
        let Some(connections) = lock(&self.connections).take() else {
            return;
        };
        // Shutting the connections down also ends any request or event wait
        // on them.
        for connection in &connections {
            let _ = connection.shutdown(Shutdown::Both);
        }
        drop(connections);
        lock(&self.commands).take();
        forget(self.generation);
        let reason = format!("{reason} (at {})", self.path.display());
        let _ = self.owner.send(Message::SwayLost(self.generation, reason));
    }

    fn request<D: DeserializeOwned>(&self, kind: CommandType, payload: &[u8]) -> Fallible<D> {
        let mut commands = lock(&self.commands);
        let Some(Commands { stream, replies }) = commands.as_mut() else {
            return Err(gone());
        };
        if let Err(e) = stream.write_all(&kind.encode_with(payload)) {
            drop(commands);
            let e = SwayError::from(e);
            self.lose(&describe(&e));
            return Err(e);
        }
        let reply = match replies.recv_timeout(REQUEST_TIMEOUT) {
            Ok(reply) => kind.decode(reply),
            Err(RecvTimeoutError::Timeout) => Err(io::Error::from(ErrorKind::TimedOut).into()),
            // The reader saw the connection end, and loses sway itself.
            Err(RecvTimeoutError::Disconnected) => return Err(gone()),
        };
        drop(commands);
        match reply {
            Err(e) if is_loss(&e) => {
                self.lose(&describe(&e));
                Err(e)
            }
            reply => reply,
        }
    }

    /// Run `command` as sway commands.
    pub fn run_command<T: AsRef<str>>(&self, command: T) -> Fallible<Vec<Fallible<()>>> {
        let outcome: Vec<CommandOutcome> =
            self.request(CommandType::RunCommand, command.as_ref().as_bytes())?;
        Ok(outcome.into_iter().map(CommandOutcome::decode).collect())
    }

    /// Send a tick event with `payload`.
    pub fn send_tick(&self, payload: &str) -> Fallible<bool> {
        let reply: Success = self.request(CommandType::SendTick, payload.as_bytes())?;
        Ok(reply.success)
    }

    pub fn get_tree(&self) -> Fallible<Node> {
        self.request(CommandType::GetTree, b"")
    }

    /// sway's input devices, untyped (see [`utils::get_inputs_json`]).
    pub fn get_inputs_json(&self) -> Fallible<Vec<Value>> {
        self.request(CommandType::GetInputs, b"")
    }

    /// sway's outputs, untyped (see [`utils::get_outputs_json`]).
    pub fn get_outputs_json(&self) -> Fallible<Vec<Value>> {
        self.request(CommandType::GetOutputs, b"")
    }

    /// Subscribe to `events` (a JSON list of event names) on a connection of
    /// its own. A subscription that fails loses this sway; so must the end
    /// of the returned stream.
    pub fn subscribe(&self, events: &str) -> Fallible<UnixStream> {
        let subscribed = (|| {
            let mut stream = UnixStream::connect(&self.path)?;
            stream.set_read_timeout(Some(REQUEST_TIMEOUT))?;
            stream.set_write_timeout(Some(REQUEST_TIMEOUT))?;
            stream.write_all(&CommandType::Subscribe.encode_with(events))?;
            let reply: Success =
                CommandType::Subscribe.decode(utils::receive_message(&mut stream)?)?;
            if !reply.success {
                return Err(SwayError::SubscriptionFailed(events.to_string()));
            }
            // Events come whenever they come.
            stream.set_read_timeout(None)?;
            let registered = stream.try_clone()?;
            Ok((stream, registered))
        })();
        let (stream, registered) = match subscribed {
            Ok(streams) => streams,
            Err(e) => {
                self.lose(&describe(&e));
                return Err(e);
            }
        };
        match lock(&self.connections).as_mut() {
            Some(connections) => connections.push(registered),
            None => {
                let _ = stream.shutdown(Shutdown::Both);
                return Err(gone());
            }
        }
        Ok(stream)
    }

    /// Subscribe to `events` and pass every message on the subscription to
    /// `forward`, on a thread of its own, until `forward` returns false or
    /// the subscription ends, which loses this sway.
    pub fn follow<F>(self: &Arc<Self>, events: &str, mut forward: F) -> Fallible<()>
    where
        F: FnMut(u32, Vec<u8>) -> bool + Send + 'static,
    {
        let mut stream = self.subscribe(events)?;
        let sway = self.clone();
        thread::spawn(move || loop {
            match utils::receive_message(&mut stream) {
                Ok((kind, payload)) => {
                    if !forward(kind, payload) {
                        return;
                    }
                }
                Err(e) => return sway.lose(&describe(&e)),
            }
        });
        Ok(())
    }
}
