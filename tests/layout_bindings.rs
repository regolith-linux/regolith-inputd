//! Shortcuts in layouts without Latin letters: while such a layout is in
//! use, the daemon binds the keysyms it cannot produce to the keys that
//! produce them in the US layout, in the binding mode sway is in, and
//! withdraws those bindings while a Latin layout is in use.

mod common;

use common::{Daemon, FakeSway};
use std::thread;
use std::time::Duration;

/// A keyboard with GNOME's default single layout, as sway reports it.
const INPUTS: &str = r#"[
  {"identifier": "1:1:AT_Translated_Set_2_keyboard", "name": "AT Translated Set 2 keyboard",
   "vendor": 1, "product": 1, "type": "keyboard",
   "xkb_active_layout_name": "English (US)", "xkb_layout_names": ["English (US)"],
   "xkb_active_layout_index": 0, "libinput": {"send_events": "enabled"}}
]"#;

/// Regolith-style bindings: `$mod` from the resource database (trawld does
/// not run here, so it takes its default), a `--release` binding whose
/// command has a variable sway must not expand again, a key the user bound
/// by code, a `--to-code` binding sway already binds by code, and a mode.
const CONFIG: &str = "\
set_from_resource $mod wm.mod Mod4
set $alt Mod1
bindsym $mod+z workspace z
bindsym $mod+Return exec terminal
bindsym --release $mod+$alt+bracketleft exec echo $$HOME
bindsym $mod+c nop c
bindcode $mod+54 nop code 54
bindsym --to-code $mod+x nop x
mode \"resize\" {
    bindsym l resize grow width 10 px
    bindsym Escape mode default
}
";

/// The fallback for the default mode: US z (AB01, code 52) and US
/// bracketleft (AD11, code 34).
const DEFAULT_FALLBACK: &str = "bindcode --no-warn Mod4+52 'workspace z'; \
    bindcode --no-warn --release Mod1+Mod4+34 'exec echo $$HOME'";

fn start(name: &str, sources: &str) -> (FakeSway, Daemon) {
    let keyfile = format!("[org/gnome/desktop/input-sources]\nsources={sources}\n");
    let (sway, daemon) = Daemon::start_with_config(name, &keyfile, INPUTS, CONFIG);
    sway.wait_for_startup();
    sway.wait_for_subscribers(3);
    (sway, daemon)
}

/// The commands sway got from the `skip`th on that add or remove bindings.
fn bind_commands(sway: &FakeSway, skip: usize) -> Vec<String> {
    sway.commands()
        .into_iter()
        .skip(skip)
        .filter(|c| c.starts_with("bindcode") || c.starts_with("unbindcode"))
        .collect()
}

/// Wait for the next command after the `skip`th that adds or removes
/// bindings, and check it is `expected`.
fn expect(sway: &FakeSway, skip: usize, expected: &str) {
    let prefix = expected.split(' ').next().unwrap();
    let got = sway.wait_for(skip, prefix);
    assert_eq!(
        got.as_deref(),
        Some(expected),
        "sway got {:?}",
        sway.commands()
    );
}

#[test]
fn non_latin_layouts_get_us_positions_in_each_mode() {
    let (sway, _daemon) = start("non-latin", "[('xkb', 'ru'), ('xkb', 'us')]");
    sway.wait_for_layout(&["ru", "us"], 0);
    expect(&sway, 0, DEFAULT_FALLBACK);

    // A Latin layout in use: the fallback is withdrawn.
    let seen = sway.commands().len();
    sway.user_switch(1);
    expect(
        &sway,
        seen,
        "unbindcode Mod4+52; unbindcode --release Mod1+Mod4+34",
    );

    // Back on Russian, and again after a reload, which drops it.
    let seen = sway.commands().len();
    sway.user_switch(0);
    expect(&sway, seen, DEFAULT_FALLBACK);
    let seen = sway.commands().len();
    sway.reload();
    expect(&sway, seen, DEFAULT_FALLBACK);

    // Modes get their own bindings when they are entered.
    let seen = sway.commands().len();
    sway.enter_mode("resize");
    expect(
        &sway,
        seen,
        "bindcode --no-warn 46 'resize grow width 10 px'",
    );
    let seen = sway.commands().len();
    sway.enter_mode("default");
    thread::sleep(Duration::from_millis(500));
    assert_eq!(bind_commands(&sway, seen), Vec::<String>::new());
}

#[test]
fn latin_layouts_keep_sways_own_bindings() {
    let (sway, _daemon) = start("latin", "[('xkb', 'us'), ('xkb', 'de')]");
    sway.wait_for_layout(&["us", "de"], 0);
    sway.user_switch(1);
    sway.reload();
    sway.enter_mode("resize");
    thread::sleep(Duration::from_millis(500));
    assert_eq!(bind_commands(&sway, 0), Vec::<String>::new());
}
