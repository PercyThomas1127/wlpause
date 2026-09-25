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
use std::path::PathBuf;

pub struct Freeze<T: Target> {
    inner: T,
    pids: Vec<u32>,
    reclaim: bool,
}

impl<T: Target> Freeze<T> {
    pub fn new(inner: T, pids: Vec<u32>) -> Self {
        Freeze {
            inner,
            pids,
            reclaim: false,
        }
    }

    /// Also push the frozen process's memory out to swap. See [`reclaim`].
    pub fn with_reclaim(mut self, on: bool) -> Self {
        self.reclaim = on;
        self
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

    fn sync(&mut self, paused: bool) -> io::Result<bool> {
        let stopped = self.any_stopped();
        if paused {
            if stopped {
                // Already fully frozen. Do not touch the player: a stopped
                // process cannot answer IPC, so asking would simply block
                // until the socket timeout on every heartbeat.
                return Ok(false);
            }
            // Pause first so mpv reaches a consistent state, then stop it.
            // Reached both from "playing" and from the half state where the
            // player is paused but the process is still running and burning
            // CPU redrawing the same frame.
            let _ = self.inner.sync(true);
            self.signal_all(libc::SIGSTOP);
            if self.reclaim {
                reclaim(&self.pids);
            }
            Ok(true)
        } else {
            // Wake it before talking to it, or the IPC call hangs.
            if stopped {
                self.signal_all(libc::SIGCONT);
            }
            let unpaused = self.inner.sync(false)?;
            Ok(stopped || unpaused)
        }
    }
}

/// The cgroup v2 directory of `pid`, from `/proc/<pid>/cgroup`.
fn cgroup_of(pid: u32) -> Option<PathBuf> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    let rel = s.lines().find_map(|l| l.strip_prefix("0::"))?;
    // "/" is the root cgroup, which has no memory.reclaim and is never ours.
    (rel != "/").then(|| PathBuf::from(format!("/sys/fs/cgroup{rel}")))
}

/// Ask the kernel to swap out everything the frozen wallpaper holds.
///
/// A stopped process keeps all its memory resident; frozen mpvpaper is
/// ~95MB of anonymous decoder state doing nothing. cgroup v2's
/// `memory.reclaim` pushes it to swap (zswap first, if enabled). Measured
/// frozen: anon 96MB -> 0 in 0.5s, and on resume only 62MB faulted back, in
/// well under a frame. Pinned GPU buffers are unevictable and stay put.
///
/// Only reclaims a cgroup whose members are all wallpaper pids, so it has to
/// be launched in a scope of its own (`systemd-run --user --scope`); in a
/// shared cgroup, reclaiming would swap out the compositor too. That case is
/// skipped with a warning rather than done wrong.
///
/// Runs on a thread because the write blocks until reclaim finishes, and it
/// must not stall the event loop. EAGAIN -- "could not reclaim all of it",
/// which is normal given the unevictable part -- is not an error.
fn reclaim(pids: &[u32]) {
    let mut groups: Vec<PathBuf> = pids.iter().filter_map(|p| cgroup_of(*p)).collect();
    groups.sort();
    groups.dedup();
    for cg in groups {
        let members = std::fs::read_to_string(cg.join("cgroup.procs")).unwrap_or_default();
        let foreign = members
            .lines()
            .filter_map(|l| l.trim().parse::<u32>().ok())
            .any(|p| !pids.contains(&p));
        if foreign || members.trim().is_empty() {
            eprintln!(
                "wlpause: --reclaim skipped: {} holds processes besides the wallpaper; \
                 launch it in its own scope",
                cg.display()
            );
            continue;
        }
        std::thread::spawn(move || {
            let Ok(current) = std::fs::read_to_string(cg.join("memory.current")) else {
                return;
            };
            if let Err(e) = std::fs::write(cg.join("memory.reclaim"), current.trim()) {
                if e.raw_os_error() != Some(libc::EAGAIN) {
                    eprintln!("wlpause: reclaim {}: {e}", cg.display());
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockPlayer {
        paused: bool,
    }

    impl Target for MockPlayer {
        fn describe(&self) -> String {
            "mock".into()
        }
        fn sync(&mut self, paused: bool) -> io::Result<bool> {
            let changed = self.paused != paused;
            self.paused = paused;
            Ok(changed)
        }
    }

    /// Regression: the player being paused must NOT be mistaken for the
    /// whole target being paused. With an is_paused()/set_paused() pair this
    /// looked like "already correct", so the SIGSTOP half was never applied
    /// and the wallpaper sat burning CPU redrawing a frozen frame.
    ///
    /// An empty pid list keeps the signalling inert, so this exercises the
    /// decision rather than the kill(2).
    #[test]
    fn paused_player_that_is_not_stopped_still_needs_freezing() {
        let mut t = Freeze::new(MockPlayer { paused: true }, vec![]);
        assert!(
            t.sync(true).unwrap(),
            "process is not stopped yet, so there is still work to do"
        );
    }

    /// The other direction of the same hole: a paused player with a running
    /// process must still be unpaused when we want it playing.
    #[test]
    fn paused_player_is_resumed_even_when_not_stopped() {
        let mut t = Freeze::new(MockPlayer { paused: true }, vec![]);
        assert!(t.sync(false).unwrap());
    }

    #[test]
    fn resuming_an_already_playing_target_is_a_no_op() {
        let mut t = Freeze::new(MockPlayer { paused: false }, vec![]);
        assert!(!t.sync(false).unwrap());
    }
}
