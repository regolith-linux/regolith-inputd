//! The input-source service of GNOME's X11 session, for the sway session.
//!
//! GNOME Flashback publishes the input sources on the session bus as
//! `org.gnome.Flashback.InputSources`, and status bars and scripts read the
//! sources, follow the current one and switch through it. The daemon
//! publishes the same interface while it runs:
//! - `GetInputSources` lists the sources applied to sway, each with its
//!   position in the `sources` setting, GNOME's short form and name for it,
//!   and whether it is the current source, and describes the current source;
//! - `Activate` switches to a listed source as a user's switch does;
//! - `ActivateProperty` is refused: no source has properties;
//! - `Changed` is emitted whenever the list or the current source changes.
//!
//! The switcher owns the state: method calls are queued to it like sway's
//! events, and it answers them (see [`crate::source_switching`]).

use crate::input_sources::EffectiveSource;
use crate::xkb_rules::XkbRules;
use gio::{BusNameOwnerFlags, DBusConnection, DBusMethodInvocation, DBusNodeInfo};
use glib::{ToVariant, Variant, VariantDict, VariantTy};
use log::{info, warn};
use std::collections::HashMap;

pub const NAME: &str = "org.gnome.Flashback.InputSources";
pub const PATH: &str = "/org/gnome/Flashback/InputSources";
pub const INTERFACE: &str = "org.gnome.Flashback.InputSources";

const INTERFACE_XML: &str = r#"<node>
  <interface name="org.gnome.Flashback.InputSources">
    <method name="GetInputSources">
      <arg type="a(ussb)" name="input_sources" direction="out"/>
      <arg type="a{sv}" name="current_source" direction="out"/>
    </method>
    <method name="Activate">
      <arg type="u" name="index" direction="in"/>
    </method>
    <method name="ActivateProperty">
      <arg type="s" name="key" direction="in"/>
    </method>
    <signal name="Changed"/>
  </interface>
</node>"#;

const INVALID_ARGS: &str = "org.freedesktop.DBus.Error.InvalidArgs";

/// A listed source as the service shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The source's position in the `sources` setting.
    pub position: u32,
    /// GNOME's short form, numbered when other entries share it.
    pub short: String,
    pub name: String,
    pub layout: String,
    pub variant: String,
}

/// The entries for the effective sources, in their order. As in GNOME, a
/// short form that several entries share is numbered with subscript digits
/// in list order; one that a single entry has stays bare.
pub fn entries(sources: &[EffectiveSource], rules: &XkbRules) -> Vec<Entry> {
    let mut entries: Vec<Entry> = sources
        .iter()
        .map(|s| {
            let (short, name) = match rules.describe(&s.layout, &s.variant) {
                Some(d) => (d.short, d.name),
                None => (s.layout.clone(), s.source.1.clone()),
            };
            Entry {
                position: s.position,
                short,
                name,
                layout: s.layout.clone(),
                variant: s.variant.clone(),
            }
        })
        .collect();
    let mut shared: HashMap<String, usize> = HashMap::new();
    for e in &entries {
        *shared.entry(e.short.clone()).or_default() += 1;
    }
    let mut seen: HashMap<String, usize> = HashMap::new();
    for e in entries.iter_mut() {
        if shared[&e.short] > 1 {
            let n = seen.entry(e.short.clone()).or_default();
            *n += 1;
            e.short.push_str(&subscript(*n));
        }
    }
    entries
}

/// `n` in subscript digits.
fn subscript(n: usize) -> String {
    n.to_string()
        .chars()
        .filter_map(|c| c.to_digit(10))
        .filter_map(|d| char::from_u32(0x2080 + d))
        .collect()
}

/// A call of the interface's methods.
pub enum Call {
    GetInputSources,
    Activate(u32),
    ActivateProperty,
}

impl Call {
    fn from_method(method: &str, parameters: &Variant) -> Option<Call> {
        match method {
            "GetInputSources" => Some(Call::GetInputSources),
            "Activate" => parameters.get::<(u32,)>().map(|(i,)| Call::Activate(i)),
            "ActivateProperty" => Some(Call::ActivateProperty),
            _ => None,
        }
    }
}

/// The reply to `GetInputSources`: the entries with the current one marked,
/// and the description of the current source (empty without one).
pub fn input_sources_reply(entries: &[Entry], current: Option<usize>) -> Variant {
    let list: Vec<(u32, String, String, bool)> = entries
        .iter()
        .enumerate()
        .map(|(i, e)| {
            (
                e.position,
                e.short.clone(),
                e.name.clone(),
                current == Some(i),
            )
        })
        .collect();
    let dict = VariantDict::new(None);
    if let Some(e) = current.and_then(|i| entries.get(i)) {
        let no_properties = VariantTy::new("(sa{sv})").expect("valid type");
        dict.insert_value("tooltip", &e.name.to_variant());
        dict.insert_value("icon-text", &e.short.to_variant());
        dict.insert_value("layout", &e.layout.to_variant());
        dict.insert_value("layout-variant", &e.variant.to_variant());
        dict.insert_value(
            "properties",
            &Variant::array_from_iter_with_type(no_properties, Vec::<Variant>::new()),
        );
    }
    Variant::tuple_from_iter([list.to_variant(), dict.end()])
}

/// Refuse `Activate` for a position that isn't listed.
pub fn refuse_index(invocation: DBusMethodInvocation, index: u32) {
    invocation.return_dbus_error(
        INVALID_ARGS,
        &format!("Input source with index {index} does not exist"),
    );
}

/// Refuse `ActivateProperty`.
pub fn refuse_property(invocation: DBusMethodInvocation) {
    invocation.return_dbus_error(INVALID_ARGS, "Input source does not have properties");
}

/// Publish the interface on `bus` and take the service's name. Method calls
/// are handed to `on_call` on the thread that runs the default main loop.
pub fn export<F>(bus: &DBusConnection, on_call: F) -> Result<(), glib::Error>
where
    F: Fn(Call, DBusMethodInvocation) + Send + Sync + 'static,
{
    let node = DBusNodeInfo::for_xml(INTERFACE_XML)?;
    let interface = node.lookup_interface(INTERFACE).expect("interface in XML");
    bus.register_object(
        PATH,
        &interface,
        move |_, _, _, _, method, parameters, invocation| match Call::from_method(
            method,
            &parameters,
        ) {
            Some(call) => on_call(call, invocation),
            None => invocation.return_dbus_error(
                "org.freedesktop.DBus.Error.UnknownMethod",
                &format!("Unknown method {method}"),
            ),
        },
        |_, _, _, _, _| ().to_variant(),
        |_, _, _, _, _, _| false,
    )?;
    gio::bus_own_name_on_connection(
        bus,
        NAME,
        BusNameOwnerFlags::NONE,
        |_, name| info!("Owning {name}"),
        |_, name| warn!("Lost or could not own {name}"),
    );
    Ok(())
}

/// Tell the interface's listeners that the list or the current source
/// changed.
pub fn emit_changed(bus: &DBusConnection) {
    if let Err(e) = bus.emit_signal(None, PATH, INTERFACE, "Changed", None) {
        warn!("{e}");
    }
}

#[cfg(test)]
mod tests {
    use super::{entries, input_sources_reply, subscript, Entry};
    use crate::input_sources::effective_sources;
    use crate::xkb_rules::XkbRules;
    use glib::VariantDict;

    fn labels(sources: &[(&str, &str)]) -> Vec<(u32, String)> {
        let sources = sources
            .iter()
            .map(|(k, i)| (k.to_string(), i.to_string()))
            .collect();
        entries(&effective_sources(sources), &XkbRules::default())
            .into_iter()
            .map(|e| (e.position, e.short))
            .collect()
    }

    #[test]
    fn shared_short_forms_are_numbered_in_list_order() {
        assert_eq!(subscript(1), "\u{2081}");
        assert_eq!(subscript(12), "\u{2081}\u{2082}");
        // Without rules a layout's short form is its name.
        assert_eq!(
            labels(&[
                ("xkb", "de"),
                ("other", "x"),
                ("xkb", "fr"),
                ("xkb", "de+nodeadkeys")
            ]),
            vec![
                (0, "de\u{2081}".to_string()),
                (2, "fr".to_string()),
                (3, "de\u{2082}".to_string())
            ]
        );
    }

    #[test]
    fn reply_describes_the_current_source() {
        let entry = Entry {
            position: 2,
            short: "de".into(),
            name: "German (no dead keys)".into(),
            layout: "de".into(),
            variant: "nodeadkeys".into(),
        };
        let reply = input_sources_reply(std::slice::from_ref(&entry), Some(0));
        assert_eq!(reply.type_().as_str(), "(a(ussb)a{sv})");
        assert_eq!(
            reply.child_value(0).print(true).as_str(),
            "[(uint32 2, 'de', 'German (no dead keys)', true)]"
        );
        let current = VariantDict::new(Some(&reply.child_value(1)));
        let text = |key: &str| current.lookup::<String>(key).unwrap().unwrap();
        assert_eq!(text("tooltip"), "German (no dead keys)");
        assert_eq!(text("icon-text"), "de");
        assert_eq!(text("layout"), "de");
        assert_eq!(text("layout-variant"), "nodeadkeys");
        let properties = current.lookup_value("properties", None).unwrap();
        assert_eq!(properties.print(true).as_str(), "@a(sa{sv}) []");
        assert_eq!(reply.child_value(1).n_children(), 5);
        let none = input_sources_reply(&[entry], None);
        assert_eq!(
            none.print(true).as_str(),
            "([(uint32 2, 'de', 'German (no dead keys)', false)], @a{sv} {})"
        );
    }
}
