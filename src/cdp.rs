//! Minimal CDP client: HTTP GET for `/json/version` and `/json/list`, and a
//! WebSocket to the browser endpoint. Page commands go through flat
//! sessions (`Target.attachToTarget` with `flatten`), so one socket serves
//! every tab. Blocking I/O with per-call deadlines; no async runtime.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::ctx::Ctx;
use crate::util::{Error, Result, b64encode, os_error_text, py_bytes_repr, random_bytes};

// ------------------------------------------------------------ HTTP

fn urlopen_error(e: &io::Error) -> Error {
    Error::os(format!("<urlopen error {}>", os_error_text(e)))
}

fn dur(secs: f64) -> Duration {
    Duration::from_secs_f64(secs.clamp(0.001, 1e6))
}

/// GET `http://127.0.0.1:{port}{path}` and parse the JSON body. Errors read
/// like urllib's (`<urlopen error ...>`, `timed out`, `HTTP Error N: ...`).
pub fn http_get_json(port: u16, path: &str, timeout: f64) -> Result<Value> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let mut s = TcpStream::connect_timeout(&addr, dur(timeout)).map_err(|e| urlopen_error(&e))?;
    s.set_read_timeout(Some(dur(timeout))).map_err(|e| urlopen_error(&e))?;
    s.set_write_timeout(Some(dur(timeout))).map_err(|e| urlopen_error(&e))?;
    s.set_nodelay(true).ok();
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAccept-Encoding: identity\r\nConnection: close\r\nUser-Agent: agent-display\r\n\r\n"
    );
    s.write_all(req.as_bytes()).map_err(|e| urlopen_error(&e))?;
    let mut buf = Vec::with_capacity(8192);
    let mut chunk = [0u8; 16384];
    let head_end = loop {
        if let Some(i) = find(&buf, b"\r\n\r\n") {
            break i;
        }
        let n = s.read(&mut chunk).map_err(|e| Error::os(os_error_text(&e)))?;
        if n == 0 {
            return Err(Error::os("Remote end closed connection without response"));
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut body = buf[head_end + 4..].to_vec();
    let mut lines = head.split("\r\n");
    let status = lines.next().unwrap_or("");
    let mut parts = status.splitn(3, ' ');
    let _ = parts.next();
    let code: u16 = parts.next().and_then(|c| c.parse().ok()).unwrap_or(0);
    let reason = parts.next().unwrap_or("");
    let mut length = None;
    let mut chunked = false;
    for l in lines {
        if let Some((k, v)) = l.split_once(':') {
            let k = k.trim().to_ascii_lowercase();
            if k == "content-length" {
                length = v.trim().parse::<usize>().ok();
            } else if k == "transfer-encoding" && v.to_ascii_lowercase().contains("chunked") {
                chunked = true;
            }
        }
    }
    let complete = |body: &[u8]| match (chunked, length) {
        (true, _) => find(body, b"0\r\n\r\n").is_some(),
        (false, Some(n)) => body.len() >= n,
        (false, None) => false,
    };
    while !complete(&body) {
        let n = s.read(&mut chunk).map_err(|e| Error::os(os_error_text(&e)))?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    if chunked {
        body = dechunk(&body);
    } else if let Some(n) = length {
        body.truncate(n);
    }
    if !(200..300).contains(&code) {
        return Err(Error::os(format!("HTTP Error {code}: {reason}")));
    }
    serde_json::from_slice(&body).map_err(|e| Error::new(format!("invalid JSON from CDP: {e}")))
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn dechunk(mut b: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(i) = find(b, b"\r\n") {
        let size = std::str::from_utf8(&b[..i]).ok().and_then(|s| usize::from_str_radix(s.split(';').next()?.trim(), 16).ok());
        let Some(size) = size else { break };
        if size == 0 || b.len() < i + 2 + size {
            break;
        }
        out.extend_from_slice(&b[i + 2..i + 2 + size]);
        b = &b[(i + 4 + size).min(b.len())..];
    }
    out
}

pub fn cdp_version(port: u16, timeout: f64) -> Result<Value> {
    http_get_json(port, "/json/version", timeout)
}

pub fn cdp_targets(port: u16, timeout: f64) -> Result<Vec<Value>> {
    match http_get_json(port, "/json/list", timeout)? {
        Value::Array(a) => Ok(a),
        other => Err(Error::new(format!("unexpected /json/list reply: {other}"))),
    }
}

// ------------------------------------------------------------ WebSocket

/// Client WebSocket frame (masked, as RFC 6455 requires).
pub fn ws_frame(payload: &[u8], opcode: u8) -> Vec<u8> {
    let n = payload.len();
    let mut f = Vec::with_capacity(n + 14);
    f.push(0x80 | opcode);
    if n < 126 {
        f.push(0x80 | n as u8);
    } else if n < 1 << 16 {
        f.push(0x80 | 126);
        f.extend_from_slice(&(n as u16).to_be_bytes());
    } else {
        f.push(0x80 | 127);
        f.extend_from_slice(&(n as u64).to_be_bytes());
    }
    let mask = random_bytes(4);
    f.extend_from_slice(&mask);
    f.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
    f
}

/// A byte stream with settable read timeouts (TCP, or a socketpair in tests).
pub trait Sock: Read + Write {
    fn set_timeout(&self, d: Option<Duration>) -> io::Result<()>;
}
impl Sock for TcpStream {
    fn set_timeout(&self, d: Option<Duration>) -> io::Result<()> {
        self.set_read_timeout(d)
    }
}
impl Sock for UnixStream {
    fn set_timeout(&self, d: Option<Duration>) -> io::Result<()> {
        self.set_read_timeout(d)
    }
}

/// The operations the tab commands need from a CDP connection.
pub trait CdpConn {
    fn port(&self) -> u16;
    fn send(&mut self, method: &str, params: Value, session: Option<&str>) -> Result<u64>;
    fn call(&mut self, method: &str, params: Value, session: Option<&str>, timeout: Option<f64>) -> Result<Value>;
    fn wait_event(&mut self, method: &str, session: Option<&str>, timeout: Option<f64>) -> Result<Value>;
    fn close(&mut self);
}

pub struct Cdp {
    pub port: u16,
    pub timeout: f64,
    next_id: u64,
    events: Vec<Value>,
    buf: Vec<u8>,
    parts: Vec<u8>,
    sock: Box<dyn Sock>,
    closed: bool,
}

struct Frame {
    fin: bool,
    op: u8,
    data: Vec<u8>,
}

impl Cdp {
    /// Connect to the browser endpoint of a port the caller has validated.
    pub fn connect(ctx: &Ctx, port: u16, timeout: f64) -> Result<Cdp> {
        let ver = ctx.sys.cdp_version(port, timeout)?;
        let url = ver.get("webSocketDebuggerUrl").and_then(Value::as_str).unwrap_or("").to_string();
        let path = url
            .strip_prefix("ws://127.0.0.1:")
            .and_then(|rest| rest.split_once('/'))
            .filter(|(p, _)| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) && p.parse::<u32>().ok() == Some(port as u32))
            .map(|(_, path)| format!("/{path}"))
            .filter(|path| path.len() > 1 && !path.chars().any(char::is_whitespace));
        let Some(path) = path else {
            return Err(Error::new(format!("unexpected CDP endpoint {url}")));
        };
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        let s = TcpStream::connect_timeout(&addr, dur(timeout))?;
        s.set_nodelay(true).ok();
        s.set_write_timeout(Some(dur(timeout))).ok();
        s.set_read_timeout(Some(dur(timeout)))?;
        let mut cdp = Cdp::with_sock(Box::new(s), port, timeout);
        cdp.handshake(port, &path)?;
        Ok(cdp)
    }

    pub fn with_sock(sock: Box<dyn Sock>, port: u16, timeout: f64) -> Cdp {
        Cdp { port, timeout, next_id: 0, events: vec![], buf: vec![], parts: vec![], sock, closed: false }
    }

    fn handshake(&mut self, port: u16, path: &str) -> Result<()> {
        let key = b64encode(&random_bytes(16));
        let req = format!(
            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
        );
        self.sock.write_all(req.as_bytes())?;
        let mut chunk = [0u8; 4096];
        let end = loop {
            if let Some(i) = find(&self.buf, b"\r\n\r\n") {
                break i;
            }
            let n = self.sock.read(&mut chunk)?;
            if n == 0 {
                return Err(Error::new("CDP closed during WebSocket handshake"));
            }
            self.buf.extend_from_slice(&chunk[..n]);
        };
        let head: Vec<u8> = self.buf.drain(..end + 4).take(end).collect();
        if !head.starts_with(b"HTTP/1.1 101") {
            let first = head.split(|&b| b == b'\n').next().unwrap_or(&[]);
            let first = first.strip_suffix(b"\r").unwrap_or(first);
            return Err(Error::new(format!("CDP refused WebSocket: {}", py_bytes_repr(first))));
        }
        Ok(())
    }

    /// Buffer at least n bytes without consuming any. A timeout leaves the
    /// buffer intact, so the stream stays in sync for a later call.
    fn fill(&mut self, n: usize, deadline: Instant, what: &str) -> Result<()> {
        let mut chunk = vec![0u8; 65536.max(n.saturating_sub(self.buf.len()).min(1 << 22))];
        while self.buf.len() < n {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(Error::new(format!("timed out waiting for {what}")));
            }
            self.sock.set_timeout(Some(left.max(Duration::from_millis(1))))?;
            match self.sock.read(&mut chunk) {
                Ok(0) => return Err(Error::new("CDP closed the connection")),
                Ok(k) => self.buf.extend_from_slice(&chunk[..k]),
                Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
                    return Err(Error::new(format!("timed out waiting for {what}")));
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    /// Next whole frame, consumed atomically.
    fn frame(&mut self, deadline: Instant, what: &str) -> Result<Frame> {
        self.fill(2, deadline, what)?;
        let (b0, b1) = (self.buf[0], self.buf[1]);
        let (mut n, mut head) = ((b1 & 0x7f) as usize, 2usize);
        if n == 126 {
            self.fill(4, deadline, what)?;
            n = u16::from_be_bytes([self.buf[2], self.buf[3]]) as usize;
            head = 4;
        } else if n == 127 {
            self.fill(10, deadline, what)?;
            n = u64::from_be_bytes(self.buf[2..10].try_into().unwrap()) as usize;
            head = 10;
        }
        let mut mask = None;
        if b1 & 0x80 != 0 {
            self.fill(head + 4, deadline, what)?;
            mask = Some([self.buf[head], self.buf[head + 1], self.buf[head + 2], self.buf[head + 3]]);
            head += 4;
        }
        self.fill(head + n, deadline, what)?;
        let mut data: Vec<u8> = self.buf[head..head + n].to_vec();
        self.buf.drain(..head + n);
        if let Some(m) = mask {
            for (i, b) in data.iter_mut().enumerate() {
                *b ^= m[i % 4];
            }
        }
        Ok(Frame { fin: b0 & 0x80 != 0, op: b0 & 0x0f, data })
    }

    /// Next complete text message (handles fragments, ping, close).
    pub fn message(&mut self, deadline: Instant, what: &str) -> Result<Value> {
        loop {
            let f = self.frame(deadline, what)?;
            match f.op {
                8 => return Err(Error::new("CDP closed the connection")),
                9 => {
                    self.sock.write_all(&ws_frame(&f.data, 10))?;
                    continue;
                }
                10 => continue,
                _ => {}
            }
            // Kept on self so a timeout between fragments loses nothing.
            self.parts.extend_from_slice(&f.data);
            if f.fin {
                let data = std::mem::take(&mut self.parts);
                return serde_json::from_slice(&data).map_err(|e| Error::new(format!("invalid CDP message: {e}")));
            }
        }
    }

    fn recv(&mut self, deadline: Instant, what: &str) -> Result<Value> {
        if Instant::now() >= deadline {
            return Err(Error::new(format!("timed out waiting for {what}")));
        }
        self.message(deadline, what)
    }
}

fn session_eq(ev: &Value, session: Option<&str>) -> bool {
    ev.get("sessionId").and_then(Value::as_str) == session
}

impl CdpConn for Cdp {
    fn port(&self) -> u16 {
        self.port
    }

    fn send(&mut self, method: &str, params: Value, session: Option<&str>) -> Result<u64> {
        self.next_id += 1;
        let mut msg = json!({"id": self.next_id, "method": method, "params": params});
        if let Some(s) = session {
            msg["sessionId"] = s.into();
        }
        let text = serde_json::to_vec(&msg).unwrap_or_default();
        self.sock.write_all(&ws_frame(&text, 1))?;
        Ok(self.next_id)
    }

    fn call(&mut self, method: &str, params: Value, session: Option<&str>, timeout: Option<f64>) -> Result<Value> {
        let mid = self.send(method, params, session)?;
        let deadline = Instant::now() + dur(timeout.unwrap_or(self.timeout));
        loop {
            let msg = self.recv(deadline, method)?;
            if msg.get("id").and_then(Value::as_u64) == Some(mid) {
                if let Some(err) = msg.get("error") {
                    let m = match err.get("message") {
                        Some(Value::String(s)) => s.clone(),
                        Some(v) => v.to_string(),
                        None => "None".into(),
                    };
                    let d = match err.get("data") {
                        Some(Value::String(s)) => s.clone(),
                        Some(v) => v.to_string(),
                        None => String::new(),
                    };
                    return Err(Error::new(format!("{method}: {m} {d}").trim().to_string()));
                }
                return Ok(msg.get("result").cloned().unwrap_or_else(|| json!({})));
            }
            if msg.get("method").is_some() {
                self.events.push(msg);
            }
        }
    }

    fn wait_event(&mut self, method: &str, session: Option<&str>, timeout: Option<f64>) -> Result<Value> {
        let deadline = Instant::now() + dur(timeout.unwrap_or(self.timeout));
        loop {
            if let Some(i) = self
                .events
                .iter()
                .position(|e| e.get("method").and_then(Value::as_str) == Some(method) && session_eq(e, session))
            {
                return Ok(self.events.remove(i));
            }
            let msg = self.recv(deadline, method)?;
            if msg.get("method").is_some() {
                self.events.push(msg);
            }
        }
    }

    fn close(&mut self) {
        if !self.closed {
            self.closed = true;
            let _ = self.sock.write_all(&ws_frame(b"", 8));
        }
    }
}

impl Drop for Cdp {
    fn drop(&mut self) {
        self.close();
    }
}

/// Send one CDP command to the browser target without waiting.
pub fn cdp_send(ctx: &Ctx, port: u16, method: &str, timeout: f64) -> Result<()> {
    let mut c = ctx.sys.cdp_connect(ctx, port, timeout)?;
    let r = c.send(method, json!({}), None).map(|_| ());
    c.close();
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unmask(frame: &[u8], head: usize) -> Vec<u8> {
        let (mask, data) = frame[head..].split_at(4);
        data.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]).collect()
    }

    #[test]
    fn frame_lengths_and_mask() {
        for (n, head) in [(5usize, 2usize), (200, 4), (70000, 10)] {
            let payload = vec![b'a'; n];
            let f = ws_frame(&payload, 1);
            assert_eq!(f[0], 0x81);
            assert!(f[1] & 0x80 != 0);
            assert_eq!(unmask(&f, head), payload);
        }
    }

    fn pair() -> (UnixStream, Cdp) {
        let (a, b) = UnixStream::pair().unwrap();
        (a, Cdp::with_sock(Box::new(b), 1, 5.0))
    }

    #[test]
    fn reads_fragmented_large_and_ping() {
        let (mut a, mut cdp) = pair();
        let big = serde_json::to_vec(&json!({"id": 1, "result": {"data": "x".repeat(70000)}})).unwrap();
        let half = big.len() / 2;
        let mut wire = vec![0x89, 0]; // ping
        wire.extend([0x01, 127]);
        wire.extend((half as u64).to_be_bytes());
        wire.extend(&big[..half]);
        wire.extend([0x80, 126]);
        wire.extend(((big.len() - half) as u16).to_be_bytes());
        wire.extend(&big[half..]);
        let t = std::thread::spawn(move || {
            a.write_all(&wire).unwrap();
            let mut pong = [0u8; 2];
            a.read_exact(&mut pong).unwrap();
            pong[0]
        });
        let msg = cdp.message(Instant::now() + Duration::from_secs(2), "x").unwrap();
        assert_eq!(msg["result"]["data"].as_str().unwrap().len(), 70000);
        assert_eq!(t.join().unwrap(), 0x8a);
    }

    #[test]
    fn timeout_mid_frame_keeps_stream_in_sync() {
        let (mut a, mut cdp) = pair();
        let msg = serde_json::to_vec(&json!({"id": 1, "result": {"data": "y".repeat(300)}})).unwrap();
        let mut frame = vec![0x81, 126];
        frame.extend((msg.len() as u16).to_be_bytes());
        frame.extend(&msg);
        a.write_all(&frame[..100]).unwrap();
        let e = cdp.recv(Instant::now() + Duration::from_millis(100), "x").unwrap_err();
        assert!(e.is_timeout());
        a.write_all(&frame[100..]).unwrap();
        let got = cdp.recv(Instant::now() + Duration::from_secs(2), "x").unwrap();
        assert_eq!(got, serde_json::from_slice::<Value>(&msg).unwrap());
    }

    #[test]
    fn timeout_between_fragments_keeps_first() {
        let (mut a, mut cdp) = pair();
        let msg = br#"{"id": 2}"#;
        let mut first = vec![0x01, 4];
        first.extend(&msg[..4]);
        a.write_all(&first).unwrap();
        assert!(cdp.recv(Instant::now() + Duration::from_millis(100), "x").is_err());
        let mut second = vec![0x80, (msg.len() - 4) as u8];
        second.extend(&msg[4..]);
        a.write_all(&second).unwrap();
        assert_eq!(cdp.recv(Instant::now() + Duration::from_secs(2), "x").unwrap(), json!({"id": 2}));
    }

    #[test]
    fn call_queues_events_and_reports_errors() {
        let (mut a, mut cdp) = pair();
        let t = std::thread::spawn(move || {
            let mut head = [0u8; 2];
            for reply in [
                json!({"method": "Page.loadEventFired", "sessionId": "S"}),
                json!({"id": 1, "result": {"ok": true}}),
                json!({"id": 2, "error": {"message": "No target", "data": "x"}}),
            ] {
                if reply.get("id").is_some() {
                    // Drain the request frame (short, masked).
                    a.read_exact(&mut head).unwrap();
                    let mut rest = vec![0u8; 4 + (head[1] & 0x7f) as usize];
                    if head[1] & 0x7f == 126 {
                        let mut ext = [0u8; 2];
                        a.read_exact(&mut ext).unwrap();
                        rest = vec![0u8; 4 + u16::from_be_bytes(ext) as usize];
                    }
                    a.read_exact(&mut rest).unwrap();
                }
                let body = serde_json::to_vec(&reply).unwrap();
                let mut f = vec![0x81, body.len() as u8];
                f.extend(body);
                a.write_all(&f).unwrap();
            }
        });
        assert_eq!(cdp.call("A", json!({}), None, Some(2.0)).unwrap(), json!({"ok": true}));
        let e = cdp.call("B", json!({}), None, Some(2.0)).unwrap_err();
        assert_eq!(e.msg, "B: No target x");
        let ev = cdp.wait_event("Page.loadEventFired", Some("S"), Some(0.5)).unwrap();
        assert_eq!(ev["sessionId"], "S");
        t.join().unwrap();
    }

    #[test]
    fn http_get_parses_content_length_and_errors() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let t = std::thread::spawn(move || {
            for resp in [
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 13\r\n\r\n{\"Browser\":1}",
                "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n",
            ] {
                let (mut c, _) = l.accept().unwrap();
                let mut b = [0u8; 1024];
                let _ = c.read(&mut b).unwrap();
                c.write_all(resp.as_bytes()).unwrap();
            }
        });
        assert_eq!(cdp_version(port, 2.0).unwrap(), json!({"Browser": 1}));
        assert_eq!(cdp_version(port, 2.0).unwrap_err().msg, "HTTP Error 404: Not Found");
        t.join().unwrap();
        let e = cdp_version(port, 2.0).unwrap_err();
        assert_eq!(e.msg, "<urlopen error [Errno 111] Connection refused>");
        assert!(e.os);
    }
}
