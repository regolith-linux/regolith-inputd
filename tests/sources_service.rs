//! The input-source service of GNOME's X11 session
//! (`org.gnome.Flashback.InputSources`), published by the daemon on its
//! session bus: the listed sources with GNOME's names, the current source,
//! `Activate`, the `Changed` signal, and the `current` key kept on the
//! current source's position.

mod common;

use common::{Daemon, FakeSway, SessionBus};
use gio::prelude::*;
use gio::{Cancellable, DBusCallFlags, DBusConnection, DBusConnectionFlags, DBusSignalFlags};
use glib::{MainContext, Variant, VariantDict};
use std::cell::Cell;
use std::rc::Rc;
use std::thread;
use std::time::{Duration, Instant};

const SOURCES: &str = "org.gnome.desktop.input-sources";
const NAME: &str = "org.gnome.Flashback.InputSources";
const PATH: &str = "/org/gnome/Flashback/InputSources";

/// A keyboard with GNOME's default single layout, as sway reports it.
const INPUTS: &str = r#"[
  {"identifier": "1:1:AT_Translated_Set_2_keyboard", "name": "AT Translated Set 2 keyboard",
   "vendor": 1, "product": 1, "type": "keyboard",
   "xkb_active_layout_name": "English (US)", "xkb_layout_names": ["English (US)"],
   "xkb_active_layout_index": 0, "libinput": {"send_events": "enabled"}}
]"#;

/// The system's XKB rules description, cut down to what the tests list.
const RULES: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<xkbConfigRegistry version="1.1">
  <layoutList>
    <layout>
      <configItem>
        <name>us</name>
        <shortDescription>en</shortDescription>
        <description>English (US)</description>
      </configItem>
    </layout>
    <layout>
      <configItem>
        <name>de</name>
        <shortDescription>de</shortDescription>
        <description>German</description>
      </configItem>
      <variantList>
        <variant>
          <configItem>
            <name>nodeadkeys</name>
            <description>German (no dead keys)</description>
          </configItem>
        </variant>
      </variantList>
    </layout>
    <layout>
      <configItem>
        <name>fr</name>
        <shortDescription>fr</shortDescription>
        <description>French</description>
      </configItem>
    </layout>
  </layoutList>
</xkbConfigRegistry>
"#;

fn keyfile(sources: &str, extra: &str) -> String {
    format!("[org/gnome/desktop/input-sources]\nsources={sources}\n{extra}")
}

/// A client of the service, counting the `Changed` signals it receives.
struct Client {
    conn: DBusConnection,
    context: MainContext,
    changed: Rc<Cell<usize>>,
}

impl Client {
    fn connect(bus: &SessionBus) -> Client {
        let context = MainContext::new();
        let changed = Rc::new(Cell::new(0));
        let counter = changed.clone();
        let conn = context
            .with_thread_default(|| {
                let conn = DBusConnection::for_address_sync(
                    &bus.address,
                    DBusConnectionFlags::AUTHENTICATION_CLIENT
                        | DBusConnectionFlags::MESSAGE_BUS_CONNECTION,
                    None,
                    Cancellable::NONE,
                )
                .unwrap();
                conn.signal_subscribe(
                    None,
                    Some(NAME),
                    Some("Changed"),
                    Some(PATH),
                    None,
                    DBusSignalFlags::NONE,
                    move |_, _, _, _, _, _| counter.set(counter.get() + 1),
                );
                conn
            })
            .unwrap();
        Client {
            conn,
            context,
            changed,
        }
    }

    fn call(&self, method: &str, args: Option<&Variant>) -> Result<Variant, glib::Error> {
        self.conn.call_sync(
            Some(NAME),
            PATH,
            NAME,
            method,
            args,
            None,
            DBusCallFlags::NONE,
            5000,
            Cancellable::NONE,
        )
    }

    /// Wait until the daemon owns the service's name.
    fn wait_for_service(&self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while let Err(e) = self.call("GetInputSources", None) {
            assert!(Instant::now() < deadline, "service not published: {e}");
            thread::sleep(Duration::from_millis(50));
        }
    }

    /// The listed sources, and the current source's description.
    fn sources(&self) -> (Vec<(u32, String, String, bool)>, VariantDict) {
        let reply = self.call("GetInputSources", None).unwrap();
        assert_eq!(reply.type_().as_str(), "(a(ussb)a{sv})");
        let list = reply.child_value(0).get().unwrap();
        (list, VariantDict::new(Some(&reply.child_value(1))))
    }

    /// The index of the source marked current.
    fn marked(&self) -> Option<u32> {
        self.sources().0.into_iter().find(|e| e.3).map(|e| e.0)
    }

    fn activate(&self, index: u32) -> Result<Variant, glib::Error> {
        self.call("Activate", Some(&(index,).to_variant()))
    }

    /// Dispatch the signals received for `duration`.
    fn pump(&self, duration: Duration) {
        let deadline = Instant::now() + duration;
        while Instant::now() < deadline {
            while self.context.iteration(false) {}
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Wait until more than `seen` `Changed` signals arrived.
    fn wait_changed(&self, seen: usize) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.changed.get() <= seen {
            assert!(Instant::now() < deadline, "no Changed signal");
            self.pump(Duration::from_millis(50));
        }
    }
}

/// Start the daemon on a private bus, and wait until its switcher follows
/// sway's events and the service is published.
fn start(name: &str, keyfile: &str) -> (SessionBus, FakeSway, Daemon, Client) {
    let bus = SessionBus::start();
    let (sway, daemon) = Daemon::start_on_bus(name, keyfile, INPUTS, &bus, RULES);
    sway.wait_for_startup();
    sway.wait_for_subscribers(2);
    sway.wait_for_get_tree();
    let client = Client::connect(&bus);
    client.wait_for_service();
    (bus, sway, daemon, client)
}

/// Wait until the key `current` is `expected`.
fn wait_for_current(daemon: &Daemon, expected: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let current = daemon.gsettings_get(SOURCES, "current");
        if current == expected {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "current is {current}, expected {expected}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

fn text(dict: &VariantDict, key: &str) -> String {
    dict.lookup::<String>(key).unwrap().unwrap()
}

#[test]
fn sources_are_listed_with_gnome_names_and_their_positions() {
    let (_bus, sway, daemon, client) = start(
        "service-list",
        &keyfile(
            "[('xkb', 'us'), ('other', 'x'), ('xkb', 'de'), ('xkb', 'de+nodeadkeys')]",
            "mru-sources=[('xkb', 'de+nodeadkeys')]\n",
        ),
    );
    sway.wait_for_layout(&["us", "de", "de"], 2);
    wait_for_current(&daemon, "uint32 3");
    let (list, current) = client.sources();
    // The source of another type keeps its position: the others are not
    // renumbered. Short forms shared by several sources are numbered.
    assert_eq!(
        list,
        vec![
            (0, "en".into(), "English (US)".into(), false),
            (2, "de\u{2081}".into(), "German".into(), false),
            (3, "de\u{2082}".into(), "German (no dead keys)".into(), true),
        ]
    );
    assert_eq!(text(&current, "tooltip"), "German (no dead keys)");
    assert_eq!(text(&current, "icon-text"), "de\u{2082}");
    assert_eq!(text(&current, "layout"), "de");
    assert_eq!(text(&current, "layout-variant"), "nodeadkeys");
    let properties = current.lookup_value("properties", None).unwrap();
    assert_eq!(properties.print(true).as_str(), "@a(sa{sv}) []");
    assert_eq!(current.end().n_children(), 5);
}

#[test]
fn activate_switches_as_a_user_does() {
    let (_bus, sway, daemon, client) = start(
        "service-activate",
        &keyfile("[('xkb', 'us'), ('xkb', 'de'), ('xkb', 'fr')]", ""),
    );
    sway.wait_for_layout(&["us", "de", "fr"], 0);
    client.pump(Duration::from_millis(300));
    let seen = client.changed.get();
    client.activate(2).unwrap();
    // Stored by the time the call returns.
    assert_eq!(
        daemon.gsettings_get(SOURCES, "mru-sources"),
        "[('xkb', 'fr'), ('xkb', 'us'), ('xkb', 'de')]"
    );
    assert_eq!(daemon.gsettings_get(SOURCES, "current"), "uint32 2");
    assert_eq!(client.marked(), Some(2));
    sway.wait_for_layout(&["us", "de", "fr"], 2);
    client.wait_changed(seen);

    // Activating the current source changes nothing and announces nothing.
    client.pump(Duration::from_millis(300));
    let seen = client.changed.get();
    let commands = sway.commands().len();
    client.activate(2).unwrap();
    client.pump(Duration::from_millis(500));
    assert_eq!(client.changed.get(), seen);
    assert_eq!(sway.commands().len(), commands);
}

#[test]
fn user_switches_and_list_edits_are_announced() {
    let (_bus, sway, daemon, client) = start(
        "service-follow",
        &keyfile("[('xkb', 'us'), ('xkb', 'de'), ('xkb', 'fr')]", ""),
    );
    sway.wait_for_layout(&["us", "de", "fr"], 0);
    client.pump(Duration::from_millis(300));
    let seen = client.changed.get();
    sway.user_switch(1);
    client.wait_changed(seen);
    assert_eq!(client.marked(), Some(1));
    wait_for_current(&daemon, "uint32 1");

    // de stays current at its new position.
    let seen = client.changed.get();
    daemon.gsettings_set(
        SOURCES,
        "sources",
        "[('xkb', 'fr'), ('xkb', 'us'), ('xkb', 'de')]",
    );
    sway.wait_for_layout(&["fr", "us", "de"], 2);
    client.wait_changed(seen);
    assert_eq!(client.marked(), Some(2));
    wait_for_current(&daemon, "uint32 2");

    // Others' writes to `current` are not followed or announced.
    client.pump(Duration::from_millis(300));
    let seen = client.changed.get();
    daemon.gsettings_set(SOURCES, "current", "0");
    client.pump(Duration::from_millis(500));
    assert_eq!(client.changed.get(), seen);
    assert_eq!(client.marked(), Some(2));
    assert_eq!(sway.layouts().1, 2);
}

#[test]
fn unknown_indexes_and_properties_are_refused() {
    let (_bus, sway, _daemon, client) = start(
        "service-refuse",
        &keyfile("[('xkb', 'us'), ('other', 'x'), ('xkb', 'de')]", ""),
    );
    sway.wait_for_layout(&["us", "de"], 0);
    // GDBus reports a D-Bus error as "GDBus.Error:<name>: <message>".
    let refusal = |result: Result<Variant, glib::Error>| {
        let e = result.unwrap_err();
        let remote = e.message().strip_prefix("GDBus.Error:").unwrap();
        let (name, message) = remote.split_once(": ").unwrap();
        (name.to_string(), message.to_string())
    };
    let invalid = "org.freedesktop.DBus.Error.InvalidArgs".to_string();
    for index in [1, 3] {
        assert_eq!(
            refusal(client.activate(index)),
            (
                invalid.clone(),
                format!("Input source with index {index} does not exist")
            )
        );
    }
    assert_eq!(
        refusal(client.call("ActivateProperty", Some(&("key",).to_variant()))),
        (invalid, "Input source does not have properties".to_string())
    );
    assert_eq!(client.marked(), Some(0));
}
