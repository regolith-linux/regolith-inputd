//! sway need not be there when the daemon starts, and need not stay: it can
//! start later, exit and be replaced, drop the daemon's event subscription
//! or stop answering. Each sway the daemon reaches gets every setting, as at
//! a first launch, and is then followed as one present at launch is.

mod common;

use common::{change, schema_has_key, Daemon, FakeSway};
use std::fs;
use std::thread;
use std::time::{Duration, Instant};

const TOUCHPAD: &str = "org.gnome.desktop.peripherals.touchpad";
const MEDIA_KEYS: &str = "org.gnome.settings-daemon.plugins.media-keys";

const KEYFILE: &str = "[org/gnome/desktop/input-sources]\n\
                       sources=[('xkb', 'us')]\n\
                       [org/gnome/desktop/peripherals/touchpad]\n\
                       tap-to-click=false\n";

/// The event subscriptions the daemon keeps to a sway it follows: the
/// settings' and input-source switching's.
const SUBSCRIPTIONS: usize = 2;

/// Wait until `done` holds, for up to ten seconds.
fn wait_until(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        thread::sleep(Duration::from_millis(50));
    }
}

/// Wait until the daemon follows `sway`: settings applied and events
/// subscribed to. Returns how many connections it then keeps open there.
fn wait_until_followed(sway: &FakeSway) -> usize {
    sway.wait_for_startup();
    wait_until("the daemon subscribes to sway's events", || {
        sway.subscribers() == SUBSCRIPTIONS
    });
    let mut open = sway.open_connections();
    loop {
        thread::sleep(Duration::from_millis(500));
        let now = sway.open_connections();
        if now == open {
            return open;
        }
        open = now;
    }
}

/// Wait until the daemon follows `sway` again, with as many connections
/// open as after a first launch.
fn wait_until_followed_again(sway: &FakeSway, connections: usize) {
    wait_until("the daemon follows sway again", || {
        sway.subscribers() == SUBSCRIPTIONS && sway.open_connections() == connections
    });
}

/// The number of entries of `/proc/<pid>/<dir>`.
fn count(pid: u32, dir: &str) -> usize {
    fs::read_dir(format!("/proc/{pid}/{dir}")).unwrap().count()
}

#[test]
fn settings_are_applied_once_sway_starts_after_the_daemon() {
    let mut daemon = Daemon::start_without_sway("lifecycle-late-sway", KEYFILE);
    thread::sleep(Duration::from_secs(3));
    assert!(daemon.is_running(), "the daemon gave up waiting for sway");

    let sway = FakeSway::start_at(&daemon.socket());
    wait_until_followed(&sway);
    let commands = sway.commands();
    for expected in [
        "input type:touchpad tap disabled",
        "input type:keyboard xkb_layout 'us'",
    ] {
        assert!(
            commands.iter().any(|c| c == expected),
            "sway got {commands:?}"
        );
    }
    assert_eq!(
        change(
            &sway,
            &daemon,
            (TOUCHPAD, "tap-to-click", "true"),
            "input type:touchpad tap"
        )
        .as_deref(),
        Some("input type:touchpad tap enabled")
    );
}

#[test]
fn a_sway_that_replaces_the_one_followed_gets_every_setting() {
    let (first, mut daemon) = Daemon::start("lifecycle-replaced", KEYFILE);
    let connections = wait_until_followed(&first);
    first.stop();
    // Settings changed while no sway is reachable are applied once one is.
    daemon.gsettings_set(TOUCHPAD, "natural-scroll", "true");
    thread::sleep(Duration::from_secs(1));
    assert!(daemon.is_running(), "the daemon exited with sway");

    let second = FakeSway::start_at(&daemon.socket());
    assert_eq!(wait_until_followed(&second), connections);
    let commands = second.commands();
    for expected in [
        "input type:touchpad tap disabled",
        "input type:touchpad natural_scroll enabled",
        "input type:keyboard xkb_layout 'us'",
    ] {
        assert!(
            commands.iter().any(|c| c == expected),
            "the new sway got {commands:?}"
        );
    }
    // What the new sway started with was overwritten, not mirrored.
    assert_eq!(daemon.gsettings_get(TOUCHPAD, "natural-scroll"), "true");

    // The new sway is followed in both directions, and through reloads.
    thread::sleep(Duration::from_secs(2));
    second.user_command("input type:touchpad tap enabled");
    assert_eq!(
        daemon.wait_for_gsetting(TOUCHPAD, "tap-to-click", "true"),
        "true"
    );
    assert_eq!(
        change(
            &second,
            &daemon,
            (TOUCHPAD, "natural-scroll", "false"),
            "input type:touchpad natural_scroll"
        )
        .as_deref(),
        Some("input type:touchpad natural_scroll disabled")
    );
    let seen = second.commands().len();
    second.send_tick(r#"{"status": "reload_pending"}"#);
    second.send_workspace_reload();
    second.send_tick(r#"{"status": "reload_done"}"#);
    assert_eq!(
        second.wait_for(seen, "input type:touchpad tap").as_deref(),
        Some("input type:touchpad tap enabled")
    );
}

#[test]
fn a_shortcut_changed_while_sway_was_away_is_bound_on_the_new_sway() {
    if !schema_has_key(MEDIA_KEYS, "volume-up") {
        return;
    }
    let (first, daemon) = Daemon::start(
        "lifecycle-shortcuts",
        "[org/gnome/settings-daemon/plugins/media-keys]\n\
         volume-up=['<Super>F12']\n",
    );
    wait_until_followed(&first);
    let bound = "bindsym --locked Mod4+F12 exec volumectl -u + $wm.media.volume.step";
    assert!(first.commands().iter().any(|c| c == bound));
    first.stop();
    daemon.gsettings_set(MEDIA_KEYS, "volume-up", "['<Super>minus']");
    thread::sleep(Duration::from_secs(1));

    let second = FakeSway::start_at(&daemon.socket());
    wait_until_followed(&second);
    let commands = second.commands();
    assert!(
        commands
            .iter()
            .any(|c| c == "bindsym --locked Mod4+minus exec volumectl -u + $wm.media.volume.step"),
        "the new sway got {commands:?}"
    );
    // The old key was never bound there, so there is nothing to unbind
    // either.
    assert!(
        !commands.iter().any(|c| c.contains("Mod4+F12")),
        "the new sway got {commands:?}"
    );
}

#[test]
fn a_dropped_event_subscription_is_followed_again() {
    let (sway, daemon) = Daemon::start("lifecycle-dropped-events", KEYFILE);
    let connections = wait_until_followed(&sway);
    let seen = sway.commands().len();
    sway.drop_subscribers();
    // Every connection to that sway is given up, and the sway is treated as
    // a new one.
    assert_eq!(
        sway.wait_for(seen, "input type:touchpad tap").as_deref(),
        Some("input type:touchpad tap disabled")
    );
    wait_until_followed_again(&sway, connections);
    thread::sleep(Duration::from_secs(2));
    sway.user_command("input type:touchpad tap enabled");
    assert_eq!(
        daemon.wait_for_gsetting(TOUCHPAD, "tap-to-click", "true"),
        "true"
    );
}

#[test]
fn a_sway_that_stops_answering_is_given_up_and_followed_again() {
    let (sway, daemon) = Daemon::start("lifecycle-silent", KEYFILE);
    let connections = wait_until_followed(&sway);
    sway.set_silent(true);
    daemon.gsettings_set(TOUCHPAD, "tap-to-click", "true");
    thread::sleep(Duration::from_secs(1));
    sway.set_silent(false);
    // The unanswered request loses sway within five seconds; the sway that
    // answers again then gets every setting, the latest included.
    let seen = sway.commands().len();
    assert_eq!(
        sway.wait_for(seen, "input type:touchpad tap").as_deref(),
        Some("input type:touchpad tap enabled")
    );
    wait_until_followed_again(&sway, connections);
}

#[test]
fn nothing_accumulates_across_returns() {
    let (sway, daemon) = Daemon::start("lifecycle-leaks", KEYFILE);
    let connections = wait_until_followed(&sway);
    let mut counts = Vec::new();
    for _ in 0..4 {
        let seen = sway.commands().len();
        sway.drop_subscribers();
        sway.wait_for(seen, "input type:touchpad tap")
            .expect("sway was not followed again");
        wait_until_followed_again(&sway, connections);
        thread::sleep(Duration::from_millis(500));
        counts.push((count(daemon.pid(), "fd"), count(daemon.pid(), "task")));
    }
    let (first, last) = (counts[0], counts[counts.len() - 1]);
    assert!(
        last.0 <= first.0 && last.1 <= first.1,
        "open files and threads after each return: {counts:?}"
    );
}
