//! Compositor backends.
//!
//! Adding a compositor means answering two questions -- "where is the
//! wallpaper and what covers it" and "tell me when that might have changed"
//! -- and nothing else. All the judgement lives in [`crate::decide`].

use crate::decide::OutputState;
use std::io;
use std::os::fd::RawFd;

pub mod hyprland;

pub trait Compositor {
    fn name(&self) -> &'static str;

    /// Current geometry for every output.
    fn snapshot(&mut self) -> io::Result<Vec<OutputState>>;

    /// A file descriptor that becomes readable when the compositor has news.
    ///
    /// The supervisor owns the waiting so that it can watch this and its own
    /// shutdown pipe in a single `poll`, which is what makes the daemon both
    /// idle at zero cost and instantly responsive to SIGTERM. A backend with
    /// no event stream returns `None` and is simply polled on the heartbeat.
    fn event_fd(&self) -> Option<RawFd> {
        None
    }

    /// Consume whatever is pending on [`Compositor::event_fd`].
    ///
    /// Event *content* is deliberately ignored: any activity means "re-check",
    /// and reconciling is cheap and idempotent, so there is nothing to gain
    /// from parsing which window moved.
    fn drain_events(&mut self) -> io::Result<()> {
        Ok(())
    }

    /// False when we are polling rather than following events.
    fn event_driven(&self) -> bool {
        self.event_fd().is_some()
    }

    /// PIDs owning wallpaper surfaces, used to auto-discover an mpv IPC
    /// socket from the process's own command line.
    fn wallpaper_pids(&mut self) -> io::Result<Vec<u32>> {
        Ok(Vec::new())
    }
}

/// Pick a backend from the environment.
pub fn detect(wallpaper_ns: &str, alpha_min: f64) -> io::Result<Box<dyn Compositor>> {
    if hyprland::available() {
        return Ok(Box::new(hyprland::Hyprland::connect(wallpaper_ns, alpha_min)?));
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "no supported compositor detected \
         (Hyprland is found via HYPRLAND_INSTANCE_SIGNATURE)",
    ))
}
