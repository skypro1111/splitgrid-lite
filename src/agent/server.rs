//! The loopback receiver the bundled helpers talk to (the Electron app's
//! agent-activity-receiver): `POST /<kind>` with a JSON body, one JSON reply.
//! Each request is handed to the UI thread (which owns the layout and the
//! terminals) and the connection waits for its answer.
//!
//! It listens on its own port, so it runs beside the Electron app's (:19558);
//! every terminal we spawn is told where it is through its env.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::time::Duration;

use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use serde_json::{Value, json};

/// The Electron app keeps :19558; ours sits next to it (any free port if taken).
pub const PREFERRED_PORT: u16 = 19568;
const MAX_BODY: usize = 5 * 1024 * 1024;
/// Past the helpers' own 35s budget, so a slow command still answers them.
const REPLY_TIMEOUT: Duration = Duration::from_secs(36);

pub struct Request {
    /// The route without its slash: "terminal", "hook", "sftp", …
    pub kind: String,
    /// The query string's pairs (`/hook?terminal=…&event=…`).
    pub query: Vec<(String, String)>,
    /// The body parsed as JSON (Null when empty or not JSON).
    pub body: Value,
    reply: mpsc::Sender<Value>,
}

impl Request {
    pub fn respond(self, value: Value) {
        let _ = self.reply.send(value);
    }

    pub fn query(&self, key: &str) -> Option<&str> {
        self.query.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }
}

/// Bind and serve in background threads; returns the port and the request stream.
pub fn start() -> std::io::Result<(u16, UnboundedReceiver<Request>)> {
    let listener = TcpListener::bind(("127.0.0.1", PREFERRED_PORT)).or_else(|_| TcpListener::bind(("127.0.0.1", 0)))?;
    let port = listener.local_addr()?.port();
    let (tx, rx) = unbounded();
    std::thread::Builder::new().name("agent-receiver".into()).spawn(move || {
        for stream in listener.incoming().flatten() {
            let tx = tx.clone();
            let _ = std::thread::Builder::new().name("agent-request".into()).spawn(move || {
                if let Err(e) = serve(stream, &tx) {
                    log::debug!("agent request: {e}");
                }
            });
        }
    })?;
    Ok((port, rx))
}

fn serve(stream: TcpStream, tx: &UnboundedSender<Request>) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or("/"));

    let mut length = 0usize;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 || header.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':')
            && name.trim().eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().unwrap_or(0);
        }
    }
    let mut out = stream;
    if method != "POST" {
        return write_response(&mut out, 405, &json!({ "ok": false, "error": "method_not_allowed" }));
    }
    if length > MAX_BODY {
        return write_response(&mut out, 413, &json!({ "ok": false, "error": "too_large" }));
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;

    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let (reply, answer) = mpsc::channel();
    let request = Request {
        kind: path.trim_start_matches('/').split('/').next().unwrap_or("").to_owned(),
        query: parse_query(query),
        body: serde_json::from_slice(&body).unwrap_or(Value::Null),
        reply,
    };
    if tx.unbounded_send(request).is_err() {
        return write_response(&mut out, 503, &json!({ "ok": false, "error": "shutting_down" }));
    }
    let value = answer.recv_timeout(REPLY_TIMEOUT).unwrap_or_else(|_| json!({ "ok": false, "error": "timeout" }));
    write_response(&mut out, 200, &value)
}

fn write_response(out: &mut TcpStream, status: u16, body: &Value) -> std::io::Result<()> {
    let text = body.to_string();
    let reason = match status {
        200 => "OK",
        405 => "Method Not Allowed",
        413 => "Payload Too Large",
        _ => "Service Unavailable",
    };
    write!(out, "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}", text.len())?;
    out.flush()
}

fn parse_query(q: &str) -> Vec<(String, String)> {
    q.split('&')
        .filter_map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            (!k.is_empty()).then(|| (percent_decode(k), percent_decode(v)))
        })
        .collect()
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = (bytes[i] == b'%' && i + 3 <= bytes.len())
            .then(|| std::str::from_utf8(&bytes[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()))
            .flatten();
        match (hex, bytes[i]) {
            (Some(b), _) => {
                out.push(b);
                i += 3;
                continue;
            }
            (None, b'+') => out.push(b' '),
            (None, b) => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queries() {
        assert_eq!(parse_query("terminal=c-1&event=Stop"), [("terminal".into(), "c-1".into()), ("event".into(), "Stop".into())]);
        assert_eq!(percent_decode("a%20b+c%2"), "a b c%2");
    }

    #[test]
    fn round_trip_over_loopback() {
        let (port, mut rx) = start().unwrap();
        std::thread::spawn(move || {
            let req = futures::executor::block_on(futures::StreamExt::next(&mut rx)).unwrap();
            let echo = json!({ "ok": true, "kind": req.kind, "argv": req.body["argv"], "event": req.query("event") });
            req.respond(echo);
        });
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let body = r#"{"argv":["list"]}"#;
        write!(s, "POST /terminal?event=x HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n{body}", body.len()).unwrap();
        let mut resp = String::new();
        s.read_to_string(&mut resp).unwrap();
        let json: Value = serde_json::from_str(resp.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(json, json!({ "ok": true, "kind": "terminal", "argv": ["list"], "event": "x" }));
    }
}
