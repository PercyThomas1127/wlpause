//! mpv's JSON IPC, as used by mpvpaper's `input-ipc-server` option.

use super::Target;
use serde_json::{json, Value};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

const IO_TIMEOUT: Duration = Duration::from_secs(2);

pub struct MpvIpc {
    path: PathBuf,
    conn: Option<BufReader<UnixStream>>,
    next_id: u64,
}

impl MpvIpc {
    pub fn new(path: impl AsRef<Path>) -> Self {
        MpvIpc {
            path: path.as_ref().to_path_buf(),
            conn: None,
            next_id: 1,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn ensure_conn(&mut self) -> io::Result<()> {
        if self.conn.is_some() {
            return Ok(());
        }
        let s = UnixStream::connect(&self.path)?;
        s.set_read_timeout(Some(IO_TIMEOUT))?;
        s.set_write_timeout(Some(IO_TIMEOUT))?;
        self.conn = Some(BufReader::new(s));
        // Silence mpv's event stream. We only ever poll properties, and an
        // unread event backlog on a long-lived connection is a slow leak that
        // eventually stalls mpv's IPC thread.
        let _ = self.raw_roundtrip(&json!(["disable_event", "all"]));
        Ok(())
    }

    fn raw_roundtrip(&mut self, command: &Value) -> io::Result<Value> {
        self.ensure_conn()?;
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);

        let conn = self
            .conn
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "mpv socket"))?;

        let req = json!({ "command": command, "request_id": id });
        let mut line = serde_json::to_string(&req)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        line.push('\n');
        conn.get_mut().write_all(line.as_bytes())?;
        conn.get_mut().flush()?;

        // Skip anything that is not our reply: mpv may still emit a stray
        // event between `disable_event` taking effect and this request.
        for _ in 0..64 {
            let mut buf = String::new();
            if conn.read_line(&mut buf)? == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "mpv closed the IPC connection",
                ));
            }
            let Ok(v) = serde_json::from_str::<Value>(&buf) else {
                continue;
            };
            if v.get("request_id").and_then(Value::as_u64) == Some(id) {
                if v.get("error").and_then(Value::as_str) != Some("success") {
                    return Err(io::Error::other(format!(
                        "mpv rejected {command}: {}",
                        v.get("error").and_then(Value::as_str).unwrap_or("unknown")
                    )));
                }
                return Ok(v.get("data").cloned().unwrap_or(Value::Null));
            }
        }
        Err(io::Error::other("no reply from mpv"))
    }

    /// Retry once on a fresh connection, so a wallpaper restart does not
    /// permanently break the pauser.
    fn roundtrip(&mut self, command: Value) -> io::Result<Value> {
        match self.raw_roundtrip(&command) {
            Ok(v) => Ok(v),
            Err(_) => {
                self.conn = None;
                self.raw_roundtrip(&command)
            }
        }
    }
}

impl Target for MpvIpc {
    fn describe(&self) -> String {
        format!("mpv at {}", self.path.display())
    }

    fn is_paused(&mut self) -> io::Result<bool> {
        Ok(self
            .roundtrip(json!(["get_property", "pause"]))?
            .as_bool()
            .unwrap_or(false))
    }

    fn set_paused(&mut self, paused: bool) -> io::Result<()> {
        self.roundtrip(json!(["set_property", "pause", paused]))?;
        Ok(())
    }
}
