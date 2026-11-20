//! sway IPC over `SWAYSOCK` (no `swaymsg` subprocess).

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use serde_json::Value;

use crate::ctx::Disp;
use crate::util::{Error, Result, py_repr};

pub const IPC_MAGIC: &[u8] = b"i3-ipc";
pub const IPC_RUN_COMMAND: u32 = 0;
pub const IPC_SUBSCRIBE: u32 = 2;
pub const IPC_GET_OUTPUTS: u32 = 3;
pub const IPC_GET_TREE: u32 = 4;
pub const IPC_EVENT_INPUT: u32 = 0x8000_0015;

pub fn ipc_message(kind: u32, payload: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(14 + payload.len());
    m.extend_from_slice(IPC_MAGIC);
    m.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    m.extend_from_slice(&kind.to_le_bytes());
    m.extend_from_slice(payload);
    m
}

/// Incremental sway IPC reply/event parser.
#[derive(Default)]
pub struct IpcReader {
    pub buf: Vec<u8>,
}

impl IpcReader {
    /// Append bytes, return the complete (kind, body) messages.
    pub fn feed(&mut self, chunk: &[u8]) -> Result<Vec<(u32, Value)>> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while self.buf.len() >= 14 {
            let len = u32::from_le_bytes(self.buf[6..10].try_into().unwrap()) as usize;
            let kind = u32::from_le_bytes(self.buf[10..14].try_into().unwrap());
            if self.buf.len() < 14 + len {
                break;
            }
            let body: Vec<u8> = self.buf.drain(..14 + len).skip(14).collect();
            let v = if body.is_empty() {
                Value::Null
            } else {
                serde_json::from_slice(&body).map_err(|e| Error::new(format!("sway IPC: {e}")))?
            };
            out.push((kind, v));
        }
        Ok(out)
    }
}

fn ipc_err(e: std::io::Error) -> Error {
    Error::new(format!("sway IPC: {}", crate::util::os_error_text(&e)))
}

/// One request/reply on a fresh connection.
pub fn request(sock: &str, kind: u32, payload: &[u8]) -> Result<Value> {
    let mut s = UnixStream::connect(sock).map_err(ipc_err)?;
    s.set_read_timeout(Some(Duration::from_secs(15))).map_err(ipc_err)?;
    s.write_all(&ipc_message(kind, payload)).map_err(ipc_err)?;
    let mut reader = IpcReader::default();
    let mut buf = vec![0u8; 65536];
    loop {
        let n = s.read(&mut buf).map_err(ipc_err)?;
        if n == 0 {
            return Err(Error::new("sway closed the IPC connection"));
        }
        if let Some((k, v)) = reader.feed(&buf[..n])?.into_iter().next() {
            if k != kind {
                return Err(Error::new(format!("sway IPC: unexpected reply type {k}")));
            }
            return Ok(v);
        }
    }
}

pub fn sway_json(disp: &Disp, kind: &str) -> Result<Value> {
    let k = match kind {
        "get_outputs" => IPC_GET_OUTPUTS,
        "get_tree" => IPC_GET_TREE,
        other => return Err(Error::new(format!("sway IPC: unsupported query {other}"))),
    };
    request(&disp.swaysock, k, b"")
}

/// Run a sway command; any failed result is an error, like the Python.
pub fn sway_cmd(disp: &Disp, command: &str) -> Result<()> {
    let results = request(&disp.swaysock, IPC_RUN_COMMAND, command.as_bytes())?;
    let Some(list) = results.as_array() else {
        return Err(Error::new(format!("swaymsg: {results}")));
    };
    let bad: Vec<String> = list
        .iter()
        .filter(|r| r.get("success").and_then(Value::as_bool) != Some(true))
        .map(|r| r.get("error").and_then(Value::as_str).unwrap_or("failed").to_string())
        .collect();
    if !bad.is_empty() {
        return Err(Error::new(format!("sway rejected {}: {}", py_repr(command), bad.join("; "))));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reader_parses_split_messages() {
        let msg = ipc_message(IPC_EVENT_INPUT, br#"{"change":"removed"}"#);
        let mut r = IpcReader::default();
        assert!(r.feed(&msg[..10]).unwrap().is_empty());
        let got = r.feed(&msg[10..]).unwrap();
        assert_eq!(got, vec![(IPC_EVENT_INPUT, serde_json::json!({"change": "removed"}))]);
    }

    #[test]
    fn command_round_trip_over_a_fake_socket() {
        let dir = std::env::temp_dir().join(format!("adipc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ipc.sock");
        let _ = std::fs::remove_file(&path);
        let l = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let t = std::thread::spawn(move || {
            for reply in [&br#"[{"success": true}]"#[..], br#"[{"success": false, "error": "nope"}]"#] {
                let (mut c, _) = l.accept().unwrap();
                let mut head = [0u8; 14];
                c.read_exact(&mut head).unwrap();
                let n = u32::from_le_bytes(head[6..10].try_into().unwrap()) as usize;
                let mut body = vec![0; n];
                c.read_exact(&mut body).unwrap();
                assert_eq!(&head[..6], IPC_MAGIC);
                c.write_all(&ipc_message(IPC_RUN_COMMAND, reply)).unwrap();
            }
        });
        let disp = Disp { runtime: String::new(), wayland: String::new(), swaysock: path.to_string_lossy().into(), bus: String::new() };
        sway_cmd(&disp, "focus output HEADLESS-1").unwrap();
        let e = sway_cmd(&disp, "output HEADLESS-2 unplug").unwrap_err();
        assert_eq!(e.msg, "sway rejected 'output HEADLESS-2 unplug': nope");
        t.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
