//! GNOME's per-tablet settings reach the tablet's tool and pad devices.

mod common;

use common::{change, Daemon};

const PEN: &str = "1386:855:Wacom_Intuos_Pro_M_Pen";
const PAD: &str = "1386:855:Wacom_Intuos_Pro_M_Pad";
const TABLET: &str =
    "org.gnome.desktop.peripherals.tablet:/org/gnome/desktop/peripherals/tablets/056a:0357/";

/// A Wacom Intuos Pro M (056a:0357) as sway reports it, next to a touchpad.
const INPUTS: &str = r#"[
  {"identifier": "1386:855:Wacom_Intuos_Pro_M_Pen", "name": "Wacom Intuos Pro M Pen",
   "vendor": 1386, "product": 855, "type": "tablet_tool",
   "libinput": {"send_events": "enabled", "left_handed": "disabled"}},
  {"identifier": "1386:855:Wacom_Intuos_Pro_M_Pad", "name": "Wacom Intuos Pro M Pad",
   "vendor": 1386, "product": 855, "type": "tablet_pad",
   "libinput": {"send_events": "enabled", "left_handed": "disabled"}},
  {"identifier": "1267:12445:ELAN0001:00_04F3:309D_Touchpad", "name": "ELAN0001:00 04F3:309D Touchpad",
   "vendor": 1267, "product": 12445, "type": "touchpad",
   "libinput": {"send_events": "enabled"}}
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
