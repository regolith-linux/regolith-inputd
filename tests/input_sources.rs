//! GNOME's input sources must reach sway as exactly the listed layouts and
//! variants, in list order, whatever sway had before.

mod common;

use common::{Daemon, FakeSway};

const SOURCES: &str = "org.gnome.desktop.input-sources";

/// The first `n` `xkb_variant` and `xkb_layout` commands sway gets after
/// the first `skip`. The `xkb_switch_layout` commands of source switching
/// can come in between; they are left out.
fn layout_commands(sway: &FakeSway, skip: usize, n: usize) -> Vec<String> {
    sway.wait_for(skip, "input type:keyboard xkb_variant");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let commands: Vec<String> = sway
            .commands()
            .into_iter()
            .skip(skip)
            .filter(|c| {
                c.starts_with("input type:keyboard xkb_variant")
                    || c.starts_with("input type:keyboard xkb_layout")
            })
            .take(n)
            .collect();
        if commands.len() == n || std::time::Instant::now() > deadline {
            return commands;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// sway checks each `input` command against the field it doesn't set, so
/// the variants are cleared first and set only once the layouts are in.
fn expected(layouts: &str, variants: &str) -> Vec<String> {
    vec![
        "input type:keyboard xkb_variant ''".to_string(),
        format!("input type:keyboard xkb_layout '{layouts}'"),
        format!("input type:keyboard xkb_variant '{variants}'"),
    ]
}

#[test]
fn variants_are_applied_after_their_layouts_at_startup() {
    let (sway, _daemon) = Daemon::start(
        "sources-startup",
        "[org/gnome/desktop/input-sources]\n\
         sources=[('xkb', 'us'), ('xkb', 'de+nodeadkeys')]\n",
    );
    sway.wait_for_startup();
    assert_eq!(
        layout_commands(&sway, 0, 3),
        expected("us,de", ",nodeadkeys")
    );
}

#[test]
fn list_edits_are_applied_after_clearing_the_variants() {
    let (sway, daemon) = Daemon::start(
        "sources-edit",
        "[org/gnome/desktop/input-sources]\n\
         sources=[('xkb', 'us'), ('xkb', 'de+nodeadkeys')]\n",
    );
    sway.wait_for_startup();
    for (value, layouts, variants) in [
        // Settings' "Move up": the old variants don't fit the new layouts.
        (
            "[('xkb', 'de+nodeadkeys'), ('xkb', 'us')]",
            "de,us",
            "nodeadkeys,",
        ),
        // An added variant: it doesn't fit the old layouts.
        ("[('xkb', 'gb+extd')]", "gb", "extd"),
    ] {
        let seen = sway.commands().len();
        daemon.gsettings_set(SOURCES, "sources", value);
        assert_eq!(
            layout_commands(&sway, seen, 3),
            expected(layouts, variants),
            "sources {value}"
        );
    }
}

#[test]
fn ibus_sources_apply_their_engine_layout() {
    let (sway, daemon) = Daemon::start(
        "sources-ibus",
        "[org/gnome/desktop/input-sources]\nsources=[('xkb', 'us')]\n",
    );
    sway.wait_for_startup();
    daemon.install_ibus_component(
        "mozc",
        "<component><name>com.google.IBus.Mozc</name><engines>\
         <engine><name>mozc-jp</name><layout>jp</layout></engine>\
         <engine><name>mozc-dv</name><layout>default</layout>\
         <layout_variant>dvorak</layout_variant></engine>\
         </engines></component>",
    );
    for (value, layouts, variants) in [
        // An engine applies the layout of its description; engines with no
        // description and sources of unknown types are left out, and the
        // other sources still apply.
        (
            "[('xkb', 'de'), ('ibus', 'mozc-jp'), ('ibus', 'not-installed'), ('foo', 'bar')]",
            "de,jp",
            ",",
        ),
        // "default" (or no layout) in the description means GNOME's default.
        (
            "[('ibus', 'mozc-dv'), ('xkb', 'fr+oss')]",
            "us,fr",
            "dvorak,oss",
        ),
        // Nothing usable: GNOME's default layout.
        ("[('ibus', 'not-installed')]", "us", ""),
    ] {
        let seen = sway.commands().len();
        daemon.gsettings_set(SOURCES, "sources", value);
        assert_eq!(
            layout_commands(&sway, seen, 3),
            expected(layouts, variants),
            "sources {value}"
        );
    }
}
