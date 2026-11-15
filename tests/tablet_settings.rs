//! GNOME's per-tablet settings reach the tablet's tool and pad devices.

mod common;

use common::{change, Daemon};

const PEN: &str = "1386:855:Wacom_Intuos_Pro_M_Pen";
const PAD: &str = "1386:855:Wacom_Intuos_Pro_M_Pad";
const TABLET: &str =
    "org.gnome.desktop.peripherals.tablet:/org/gnome/desktop/peripherals/tablets/056a:0357/";

/// A Wacom Intuos Pro M (056a:0357) as sway reports it, next to a touchpad
/// and a virtual keyboard, which has no vendor or product id.
const INPUTS: &str = r#"[
  {"identifier": "1386:855:Wacom_Intuos_Pro_M_Pen", "name": "Wacom Intuos Pro M Pen",
   "vendor": 1386, "product": 855, "type": "tablet_tool",
   "libinput": {"send_events": "enabled", "left_handed": "disabled"}},
  {"identifier": "1386:855:Wacom_Intuos_Pro_M_Pad", "name": "Wacom Intuos Pro M Pad",
   "vendor": 1386, "product": 855, "type": "tablet_pad",
   "libinput": {"send_events": "enabled", "left_handed": "disabled"}},
  {"identifier": "1267:12445:ELAN0001:00_04F3:309D_Touchpad", "name": "ELAN0001:00 04F3:309D Touchpad",
   "vendor": 1267, "product": 12445, "type": "touchpad",
   "libinput": {"send_events": "enabled"}},
  {"identifier": "0:0:wlr_virtual_keyboard_v1", "name": "wlr_virtual_keyboard_v1",
   "type": "keyboard", "repeat_delay": 600, "repeat_rate": 25,
   "xkb_active_layout_name": "English (US)", "xkb_layout_names": ["English (US)"],
   "xkb_active_layout_index": 0}
]"#;

#[test]
fn tablet_settings_are_applied_at_startup_and_on_change() {
    let (sway, daemon) = Daemon::start_with_inputs(
        "tablet-settings",
        "[org/gnome/desktop/input-sources]\n\
         sources=[('xkb', 'us')]\n\
         [org/gnome/desktop/peripherals/tablets/056a:0357]\n\
         mapping='relative'\n\
         left-handed=true\n",
        INPUTS,
    );
    sway.wait_for_startup();
    let commands = sway.commands();
    for expected in [
        format!("input \"{PEN}\" tool_mode * relative"),
        format!("input \"{PEN}\" map_to_output \"*\""),
        format!("input \"{PEN}\" map_from_region 0x0 1x1"),
        format!("input \"{PEN}\" left_handed enabled"),
        format!("input \"{PAD}\" left_handed enabled"),
    ] {
        assert!(
            commands.contains(&expected),
            "no {expected:?} in {commands:?}"
        );
    }
    assert!(
        !commands
            .iter()
            .any(|c| c.contains(PAD) && c.contains("tool_mode")),
        "the pad has no tools: {commands:?}"
    );

    assert_eq!(
        change(
            &sway,
            &daemon,
            (TABLET, "mapping", "'absolute'"),
            &format!("input \"{PEN}\" tool_mode")
        )
        .as_deref(),
        Some(format!("input \"{PEN}\" tool_mode * absolute").as_str())
    );
    assert_eq!(
        change(
            &sway,
            &daemon,
            (TABLET, "left-handed", "false"),
            &format!("input \"{PAD}\" left_handed")
        )
        .as_deref(),
        Some(format!("input \"{PAD}\" left_handed disabled").as_str())
    );
}

/// A tablet whose device names sway turns into identifiers with quotes,
/// backslashes, `$` and command separators, and one ending in a backslash,
/// which sway can't be sent as part of a command.
const ODD_INPUTS: &str = r#"[
  {"identifier": "1386:856:Pen_\"odd\"_$mod_it's", "name": "Pen \"odd\" $mod it's",
   "vendor": 1386, "product": 856, "type": "tablet_tool",
   "libinput": {"send_events": "enabled", "left_handed": "disabled"}},
  {"identifier": "1386:856:Pad_a\\b;c,d'", "name": "Pad a\\b;c,d'",
   "vendor": 1386, "product": 856, "type": "tablet_pad",
   "libinput": {"send_events": "enabled", "left_handed": "disabled"}},
  {"identifier": "1386:856:Bad_Pen\\", "name": "Bad Pen\\",
   "vendor": 1386, "product": 856, "type": "tablet_tool",
   "libinput": {"send_events": "enabled", "left_handed": "disabled"}}
]"#;

#[test]
fn odd_identifiers_are_quoted_the_way_sway_reads_them() {
    let (sway, daemon) = Daemon::start_with_inputs(
        "tablet-odd-identifiers",
        "[org/gnome/desktop/input-sources]\n\
         sources=[('xkb', 'us')]\n\
         [org/gnome/desktop/peripherals/tablets/056a:0358]\n\
         left-handed=true\n",
        ODD_INPUTS,
    );
    sway.wait_for_startup();
    // Sway removes the quotes, keeps backslashes and turns `$$` into `$`,
    // so it reads `1386:856:Pen_"odd"_$mod_it's` and `1386:856:Pad_a\b;c,d'`.
    let pen = r#"'1386:856:Pen_"odd"_$$mod_it'"'s""#;
    let pad = r#""1386:856:Pad_a\b;c,d'""#;
    let commands = sway.commands();
    for expected in [
        format!("input {pen} tool_mode * absolute"),
        format!("input {pen} left_handed enabled"),
        format!("input {pad} left_handed enabled"),
    ] {
        assert!(
            commands.contains(&expected),
            "no {expected:?} in {commands:?}"
        );
    }
    assert!(
        !commands.iter().any(|c| c.contains("Bad_Pen")),
        "an identifier ending in a backslash reached sway: {commands:?}"
    );

    assert_eq!(
        change(
            &sway,
            &daemon,
            (
                "org.gnome.desktop.peripherals.tablet:/org/gnome/desktop/peripherals/tablets/056a:0358/",
                "left-handed",
                "false"
            ),
            &format!("input {pad} left_handed")
        )
        .as_deref(),
        Some(format!("input {pad} left_handed disabled").as_str())
    );
}
