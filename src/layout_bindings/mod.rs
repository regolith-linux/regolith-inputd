//! Shortcuts that stay on the key that types their letter.
//!
//! A `bindsym` binding names a keysym, and each window manager decides on its
//! own which keys produce it: i3 resolves every keysym in the first layout
//! group once, when it loads its configuration, and sway matches the keysyms
//! of the layout in use, so a layout without Latin letters leaves letter
//! shortcuts with no key at all. This module keeps the shortcuts on the keys
//! that produce their keysyms in the layout in use, and on the plain US key
//! positions for the keysyms a non-Latin layout cannot produce:
//!
//! - in the X11 session it maintains a file of group-specific `bindcode`
//!   bindings that Regolith's i3 configuration includes ahead of every other
//!   binding ([`i3`]);
//! - in the Wayland session it adds the US-position fallback to sway while a
//!   non-Latin layout is in use, and withdraws it otherwise ([`sway`]).
//!
//! Both read the bindings from the window manager's running configuration
//! ([`config`]) and work out the keys with the same rules ([`keys`]).

mod config;
pub mod i3;
mod keys;
pub mod sway;

use std::env;

/// Whether the daemon runs in Regolith's X11 session, under i3.
pub fn x11_session() -> bool {
    env::var("XDG_SESSION_TYPE").is_ok_and(|t| t == "x11")
        && env::var_os("SWAYSOCK").is_none()
        && env::var_os("WAYLAND_DISPLAY").is_none()
}
