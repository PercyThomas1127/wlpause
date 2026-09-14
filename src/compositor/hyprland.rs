//! Hyprland backend, speaking its IPC sockets directly.
//!
//! Deliberately no `hyprctl`: forking a process on every wake-up is most of
//! the cost of the naive approach, and the socket protocol is trivial --
//! write a command, read the reply, done.
//!
//! Coordinate space: monitors report `width`/`height` in *physical* pixels
//! plus a `scale`, while windows and layers report *logical* coordinates that
//! are absolute across the whole layout. Verified on a two-output setup: a
//! monitor placed at logical x=1600 reports its layers at x=1600, not x=0. So
//! everything here stays in absolute logical units and no scaling is needed --
//! the wallpaper surface's own rect is taken straight from the layer list,
//! which sidesteps the physical/logical conversion entirely.

use super::Compositor;
use crate::coverage::Rect;
use crate::decide::OutputState;
use serde_json::Value;
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

const IO_TIMEOUT: Duration = Duration::from_secs(2);
/// Hyprland's layer levels: 0 background, 1 bottom, 2 top, 3 overlay.
const MAX_LEVEL: i64 = 3;

pub fn available() -> bool {
    instance_dir().is_some()
}

fn instance_dir() -> Option<PathBuf> {
    let sig = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok()?;
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
        roots.push(PathBuf::from(xdg).join("hypr"));
    }
    // Hyprland used /tmp/hypr before XDG_RUNTIME_DIR became the default.
    roots.push(PathBuf::from("/tmp/hypr"));
    roots
        .into_iter()
        .map(|r| r.join(&sig))
        .find(|d| d.join(".socket.sock").exists())
}

pub struct Hyprland {
    dir: PathBuf,
    wallpaper_ns: String,
    alpha_min: f64,
    /// Non-blocking, so the supervisor can poll it. Held as a raw stream
    /// rather than a `BufReader` because event *text* is never inspected.
    events: Option<UnixStream>,
}

impl Hyprland {
    pub fn connect(wallpaper_ns: &str, alpha_min: f64) -> io::Result<Self> {
        let dir = instance_dir().ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "Hyprland IPC socket not found")
        })?;
        // An absent event socket is not fatal; we degrade to polling.
        let events = UnixStream::connect(dir.join(".socket2.sock"))
            .ok()
            .and_then(|s| s.set_nonblocking(true).ok().map(|_| s));
        Ok(Hyprland {
            dir,
            wallpaper_ns: wallpaper_ns.to_string(),
            alpha_min,
            events,
        })
    }

    fn request(&self, cmd: &str) -> io::Result<String> {
        let mut s = UnixStream::connect(self.dir.join(".socket.sock"))?;
        s.set_read_timeout(Some(IO_TIMEOUT))?;
        s.set_write_timeout(Some(IO_TIMEOUT))?;
        s.write_all(cmd.as_bytes())?;
        // Hyprland replies once the request side is closed.
        let _ = s.shutdown(Shutdown::Write);
        let mut buf = String::new();
        s.read_to_string(&mut buf)?;
        Ok(buf)
    }

    fn json(&self, cmd: &str) -> io::Result<Value> {
        let raw = self.request(cmd)?;
        serde_json::from_str(&raw).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{cmd}: bad JSON from Hyprland: {e}"),
            )
        })
    }
}

fn layer_rect(l: &Value) -> Rect {
    Rect::new(
        l.get("x").and_then(Value::as_i64).unwrap_or(0) as i32,
        l.get("y").and_then(Value::as_i64).unwrap_or(0) as i32,
        l.get("w").and_then(Value::as_i64).unwrap_or(0) as i32,
        l.get("h").and_then(Value::as_i64).unwrap_or(0) as i32,
    )
}

fn client_rect(c: &Value) -> Option<Rect> {
    let at = c.get("at")?.as_array()?;
    let size = c.get("size")?.as_array()?;
    Some(Rect::new(
        at.first()?.as_i64()? as i32,
        at.get(1)?.as_i64()? as i32,
        size.first()?.as_i64()? as i32,
        size.get(1)?.as_i64()? as i32,
    ))
}

fn levels_of<'a>(layers: &'a Value, monitor: &str) -> Option<&'a Value> {
    layers.get(monitor)?.get("levels")
}

impl Compositor for Hyprland {
    fn name(&self) -> &'static str {
        "hyprland"
    }

    fn event_driven(&self) -> bool {
        self.events.is_some()
    }

    fn snapshot(&mut self) -> io::Result<Vec<OutputState>> {
        let monitors = self.json("j/monitors")?;
        let layers = self.json("j/layers")?;
        let clients = self.json("j/clients")?;

        let empty = Vec::new();
        let monitors = monitors.as_array().unwrap_or(&empty);
        let clients = clients.as_array().unwrap_or(&empty);

        let mut out = Vec::with_capacity(monitors.len());
        for m in monitors {
            let name = m
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let disabled = m.get("disabled").and_then(Value::as_bool).unwrap_or(false);
            let dpms = m.get("dpmsStatus").and_then(Value::as_bool).unwrap_or(true);
            let mon_id = m.get("id").and_then(Value::as_i64).unwrap_or(-1);
            let active_ws = m
                .get("activeWorkspace")
                .and_then(|w| w.get("id"))
                .and_then(Value::as_i64);
            // 0 means "no special workspace open on this monitor".
            let special_ws = m
                .get("specialWorkspace")
                .and_then(|w| w.get("id"))
                .and_then(Value::as_i64)
                .filter(|id| *id != 0);

            // Locate the wallpaper surface and the level it sits on.
            let mut wallpaper = None;
            let mut wallpaper_level = 0i64;
            let mut wallpaper_addr = None;
            if let Some(levels) = levels_of(&layers, &name) {
                'outer: for lvl in 0..=MAX_LEVEL {
                    let Some(arr) = levels.get(lvl.to_string()).and_then(Value::as_array) else {
                        continue;
                    };
                    for l in arr {
                        if l.get("namespace").and_then(Value::as_str) == Some(&self.wallpaper_ns) {
                            wallpaper = Some(layer_rect(l));
                            wallpaper_level = lvl;
                            wallpaper_addr =
                                l.get("address").and_then(Value::as_str).map(str::to_string);
                            break 'outer;
                        }
                    }
                }
            }

            let mut occluders = Vec::new();
            if wallpaper.is_some() {
                // Layer surfaces stacked above the wallpaper. `alpha` is the
                // surface alpha the compositor applies; a fully transparent
                // overlay must not count as cover.
                if let Some(levels) = levels_of(&layers, &name) {
                    for lvl in (wallpaper_level + 1)..=MAX_LEVEL {
                        let Some(arr) = levels.get(lvl.to_string()).and_then(Value::as_array)
                        else {
                            continue;
                        };
                        for l in arr {
                            if l.get("address").and_then(Value::as_str).map(str::to_string)
                                == wallpaper_addr
                            {
                                continue;
                            }
                            let alpha = l.get("alpha").and_then(Value::as_f64).unwrap_or(1.0);
                            if alpha >= self.alpha_min {
                                occluders.push(layer_rect(l));
                            }
                        }
                    }
                }

                // Windows currently shown on this output.
                for c in clients {
                    if c.get("monitor").and_then(Value::as_i64) != Some(mon_id) {
                        continue;
                    }
                    if !c.get("mapped").and_then(Value::as_bool).unwrap_or(false) {
                        continue;
                    }
                    if c.get("hidden").and_then(Value::as_bool).unwrap_or(false) {
                        continue;
                    }
                    let ws = c
                        .get("workspace")
                        .and_then(|w| w.get("id"))
                        .and_then(Value::as_i64);
                    let shown = ws.is_some() && (ws == active_ws || ws == special_ws);
                    if !shown {
                        continue;
                    }
                    if let Some(r) = client_rect(c) {
                        occluders.push(r);
                    }
                }
            }

            out.push(OutputState {
                name,
                powered: dpms && !disabled,
                wallpaper,
                occluders,
            });
        }
        Ok(out)
    }

    fn event_fd(&self) -> Option<RawFd> {
        self.events.as_ref().map(|s| s.as_raw_fd())
    }

    fn drain_events(&mut self) -> io::Result<()> {
        let Some(ev) = self.events.as_mut() else {
            return Ok(());
        };
        let mut buf = [0u8; 4096];
        loop {
            match ev.read(&mut buf) {
                // EOF: Hyprland dropped us. Fall back to the heartbeat rather
                // than spinning on a dead socket that polls readable forever.
                Ok(0) => {
                    self.events = None;
                    return Ok(());
                }
                Ok(_) => continue,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    self.events = None;
                    return Err(e);
                }
            }
        }
    }

    fn wallpaper_pids(&mut self) -> io::Result<Vec<u32>> {
        let layers = self.json("j/layers")?;
        let mut pids = Vec::new();
        let Some(mons) = layers.as_object() else {
            return Ok(pids);
        };
        for (_, mon) in mons {
            let Some(levels) = mon.get("levels").and_then(Value::as_object) else {
                continue;
            };
            for (_, arr) in levels {
                for l in arr.as_array().unwrap_or(&Vec::new()) {
                    if l.get("namespace").and_then(Value::as_str) == Some(&self.wallpaper_ns) {
                        if let Some(p) = l.get("pid").and_then(Value::as_u64) {
                            let p = p as u32;
                            if !pids.contains(&p) {
                                pids.push(p);
                            }
                        }
                    }
                }
            }
        }
        Ok(pids)
    }
}

/// Pull `input-ipc-server=<path>` out of a process's command line.
///
/// mpvpaper passes mpv options through, so the socket the user configured is
/// recorded in `/proc/<pid>/cmdline` and does not need to be repeated on our
/// own command line.
pub fn ipc_socket_of(pid: u32) -> Option<PathBuf> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let text = String::from_utf8_lossy(&raw);
    text.split(['\0', ' '])
        .find_map(|a| a.trim_start_matches("--").strip_prefix("input-ipc-server="))
        .map(PathBuf::from)
        .filter(|p: &PathBuf| !p.as_os_str().is_empty() && Path::new(p).exists())
}
