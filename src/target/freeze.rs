//! Stop the wallpaper process outright, on top of pausing its player.
//!
//! Pausing mpv stops *decoding*, but the video output keeps redrawing the
//! same frame every time the compositor hands it a frame callback -- and a
//! compositor that never realises the surface is hidden never stops handing
//! them out, which is the whole reason this tool exists. Measured on a
//! 2560x1600 panel with a 15fps H.264 wallpaper, fully covered:
//!
//!   no pauser                40% of a core
//!   mpv pause only           10%
//!   mpv pause + SIGSTOP       0%
//!
//! So freezing is what turns a large saving into a total one.
//!
//! A stopped client simply stops committing buffers; the compositor keeps
//! showing the last one it was given, which is exactly the frozen frame we
//! want. It is opt-in all the same, because a stopped process cannot answer
//! anything -- including a compositor ping.

use super::Target;
use std::io;

pub struct Freeze<T: Target> {
    inner: T,
    pids: Vec<u32>,
}

impl<T: Target> Freeze<T> {
    pub fn new(inner: T, pids: Vec<u32>) -> Self {
        Freeze { inner, pids }
    }

    /// Process state from `/proc/<pid>/stat`, `T` meaning stopped.
    ///
    /// Parsed from the last `)` because a process name can itself contain
    /// spaces and parentheses, which breaks naive field splitting.
    fn is_stopped(pid: u32) -> Option<bool> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let rest = &stat[stat.rfind(')')? + 1..];
        let state = rest.split_whitespace().next()?;
        Some(state == "T")
    }

    fn any_stopped(&self) -> bool {
        self.pids.iter().any(|p| Self::is_stopped(*p) == Some(true))
    }

    fn signal_all(&self, sig: libc::c_int) {
        for pid in &self.pids {
            // SAFETY: kill(2) on a pid we discovered from the compositor. A
            // dead pid just returns ESRCH, which we ignore deliberately --
            // the wallpaper restarting is not our problem to report.
            unsafe {
                libc::kill(*pid as libc::pid_t, sig);
            }
        }
    }
}

impl<T: Target> Target for Freeze<T> {
    fn describe(&self) -> String {
        format!("{} (+freeze {:?})", self.inner.describe(), self.pids)
    }

    fn is_paused(&mut self) -> io::Result<bool> {
        // Check the process state first: a stopped process cannot answer IPC,
        // so asking it would block until the socket timeout on every single
        // heartbeat.
        if self.any_stopped() {
            return Ok(true);
        }
        self.inner.is_paused()
    }

    fn set_paused(&mut self, paused: bool) -> io::Result<()> {
        if paused {
            // Pause first so mpv reaches a consistent state, then stop it.
            let r = self.inner.set_paused(true);
            self.signal_all(libc::SIGSTOP);
            r
        } else {
            // Wake it before talking to it, or the IPC call will hang.
            self.signal_all(libc::SIGCONT);
            self.inner.set_paused(false)
        }
    }
}
