//! Things that can be paused.

use std::io;

pub mod freeze;
pub mod mpv;

pub trait Target {
    fn describe(&self) -> String;

    /// Bring the target to `paused`, doing nothing if it is already there.
    /// Returns whether anything actually changed.
    ///
    /// Deliberately one call rather than a `is_paused` / `set_paused` pair.
    /// A target can be in a *half* state -- [`freeze::Freeze`] pauses the
    /// player and stops the process, and those two can disagree -- which a
    /// single "are you paused?" bool cannot express. Comparing such a bool
    /// against the desired state made "player paused but process still
    /// running" look correct, so the freeze was never completed.
    ///
    /// The current state is always re-read here, never cached. If anything
    /// else pauses or unpauses the wallpaper, the next reconcile corrects it
    /// instead of desyncing permanently.
    fn sync(&mut self, paused: bool) -> io::Result<bool>;
}
