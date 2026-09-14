//! Things that can be paused.

use std::io;

pub mod freeze;
pub mod mpv;

pub trait Target {
    fn describe(&self) -> String;

    /// Ask the target what it is actually doing.
    ///
    /// Always asked, never cached. If anything else pauses or unpauses the
    /// wallpaper -- a keybind, another tool, the user poking the IPC socket --
    /// the next reconcile corrects it instead of desyncing permanently.
    fn is_paused(&mut self) -> io::Result<bool>;

    fn set_paused(&mut self, paused: bool) -> io::Result<()>;
}
