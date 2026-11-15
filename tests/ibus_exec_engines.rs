//! IBus components that name a command printing their engines
//! (`<engines exec="…">`, as ibus-anthy, ibus-mozc, ibus-libpinyin,
//! ibus-m17n, ibus-table and ibus-unikey ship) must count like components
//! that spell their engines out: their sources reach sway with the layout of
//! the printed description.

mod common;

use common::{Daemon, FakeSway};
use std::time::{Duration, Instant};

const SOURCES: &str = "org.gnome.desktop.input-sources";

/// The `xkb_layout` and `xkb_variant` commands sway gets once the sources
/// changed after the first `skip` commands.
fn applied(sway: &FakeSway, skip: usize) -> (String, String) {
    let layout = sway
        .wait_for(skip, "input type:keyboard xkb_layout")
        .unwrap_or_else(|| panic!("no layout applied; sway got {:?}", sway.commands()));
    let at = sway.commands().iter().position(|c| *c == layout).unwrap();
    let variant = sway
        .wait_for(at + 1, "input type:keyboard xkb_variant")
        .unwrap_or_else(|| panic!("no variant applied; sway got {:?}", sway.commands()));
    (layout, variant)
}

fn expected(layouts: &str, variants: &str) -> (String, String) {
    (
        format!("input type:keyboard xkb_layout '{layouts}'"),
        format!("input type:keyboard xkb_variant '{variants}'"),
    )
}

#[test]
fn engines_printed_by_a_component_command_count() {
    let (sway, daemon) = Daemon::start(
        "exec-engines",
        "[org/gnome/desktop/input-sources]\nsources=[('xkb', 'us')]\n",
    );
    sway.wait_for_startup();
    // The command is run directly with its arguments, as IBus runs it: a
    // path with a space, quoted, and an argument it checks.
    let command = daemon.install_script(
        "libexec/ibus engines/print-engines",
        "#!/bin/sh\n\
         [ \"$1\" = --xml ] || exit 1\n\
         cat <<'EOF'\n\
         <?xml version=\"1.0\"?>\n\
         <engines>\n\
         <!-- <engine><name>hidden</name><layout>fr</layout></engine> -->\n\
         <engine><name>anthy</name><layout>jp</layout></engine>\n\
         <engine><name>hangul</name><layout>kr</layout>\
         <layout_variant>kr104</layout_variant></engine>\n\
         <engine><name>m17n:hi:inscript</name>\
         <layout>default[lv3:ralt_switch]</layout></engine>\n\
         </engines>\n\
         EOF\n",
    );
    daemon.install_ibus_component(
        "printed",
        &format!(
            "<component><name>org.freedesktop.IBus.Printed</name>\
             <engines exec=\"&quot;{}&quot; --xml\"/></component>",
            command.display()
        ),
    );
    for (value, layouts, variants) in [
        (
            "[('xkb', 'us'), ('ibus', 'anthy'), ('ibus', 'hangul')]",
            "us,jp,kr",
            ",,kr104",
        ),
        // Options after the layout are dropped, and `default` is GNOME's
        // default layout; an engine left in a comment doesn't exist.
        (
            "[('ibus', 'm17n:hi:inscript'), ('ibus', 'hidden'), ('xkb', 'de')]",
            "us,de",
            ",",
        ),
    ] {
        let seen = sway.commands().len();
        daemon.gsettings_set(SOURCES, "sources", value);
        assert_eq!(
            applied(&sway, seen),
            expected(layouts, variants),
            "sources {value}"
        );
    }
}

#[test]
fn a_command_that_hangs_is_stopped_and_the_spelled_out_engines_count() {
    let (sway, daemon) = Daemon::start(
        "exec-engines-hang",
        "[org/gnome/desktop/input-sources]\nsources=[('xkb', 'us')]\n",
    );
    sway.wait_for_startup();
    let command = daemon.install_script("libexec/hang", "#!/bin/sh\nexec sleep 60\n");
    daemon.install_ibus_component(
        "hangs",
        &format!(
            "<component><name>org.freedesktop.IBus.Hangs</name>\
             <engines exec=\"{}\"><engine><name>fallback</name>\
             <layout>jp</layout></engine></engines></component>",
            command.display()
        ),
    );
    let seen = sway.commands().len();
    let started = Instant::now();
    daemon.gsettings_set(SOURCES, "sources", "[('xkb', 'us'), ('ibus', 'fallback')]");
    assert_eq!(applied(&sway, seen), expected("us,jp", ","));
    assert!(
        started.elapsed() < Duration::from_secs(9),
        "the sources took {:?}",
        started.elapsed()
    );
}
