//! The loopback receiver the bundled helpers talk to (the Electron app's
//! agent-activity-receiver): `POST /<kind>` with a JSON body, one JSON reply.
//! Each request is handed to the UI thread (which owns the layout and the
//! terminals) and the connection waits for its answer.
//!
//! It listens on its own port, so it runs beside the Electron app's (:19558);
//! every terminal we spawn is told where it is through its env.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};

use super::ext_channel::{Channel as ExtChannel, Upgrade};
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
/// `ext`: the browser-extension channel, served as a WebSocket on `/ext`.
pub fn start(ext: Option<Arc<ExtChannel>>) -> std::io::Result<(u16, UnboundedReceiver<Request>)> {
    let listener = TcpListener::bind(("127.0.0.1", PREFERRED_PORT)).or_else(|_| TcpListener::bind(("127.0.0.1", 0)))?;
    serve_on(listener, ext)
}

/// Serve on an already-bound listener (tests bind an ephemeral port).
pub fn serve_on(listener: TcpListener, ext: Option<Arc<ExtChannel>>) -> std::io::Result<(u16, UnboundedReceiver<Request>)> {
    let port = listener.local_addr()?.port();
    let (tx, rx) = unbounded();
    std::thread::Builder::new().name("agent-receiver".into()).spawn(move || {
        for stream in listener.incoming().flatten() {
            let tx = tx.clone();
            let ext = ext.clone();
            let _ = std::thread::Builder::new().name("agent-request".into()).spawn(move || {
                if let Err(e) = serve(stream, &tx, ext) {
                    log::debug!("agent request: {e}");
                }
            });
        }
    })?;
    Ok((port, rx))
}

fn serve(stream: TcpStream, tx: &UnboundedSender<Request>, ext: Option<Arc<ExtChannel>>) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let (method, target) = (parts.next().unwrap_or(""), parts.next().unwrap_or("/"));

    let mut length = 0usize;
    let mut upgrade = Upgrade::default();
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 || header.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            let (name, value) = (name.trim(), value.trim());
            if name.eq_ignore_ascii_case("content-length") {
                length = value.parse().unwrap_or(0);
            } else if name.eq_ignore_ascii_case("origin") {
                upgrade.origin = Some(value.to_owned());
            } else if name.eq_ignore_ascii_case("upgrade") {
                upgrade.upgrade = Some(value.to_owned());
            } else if name.eq_ignore_ascii_case("sec-websocket-key") {
                upgrade.key = Some(value.to_owned());
            }
        }
    }
    // The browser extension's WebSocket (ext_channel: Origin gate, pairing, commands).
    if method == "GET"
        && target.split('?').next() == Some(super::ext_protocol::WS_PATH)
        && let Some(ext) = ext
    {
        let leftover = reader.buffer().to_vec();
        return super::ext_channel::accept(ext, stream, leftover, &upgrade);
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
    // Transfers outlive the other bridges' budget (Electron gives /sftp 180s).
    let timeout = if request.kind == "sftp" { super::sftp_cmd::REQUEST_TIMEOUT + Duration::from_secs(2) } else { REPLY_TIMEOUT };
    if tx.unbounded_send(request).is_err() {
        return write_response(&mut out, 503, &json!({ "ok": false, "error": "shutting_down" }));
    }
    let value = answer.recv_timeout(timeout).unwrap_or_else(|_| json!({ "ok": false, "error": "timeout" }));
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
        let (port, mut rx) = serve_on(TcpListener::bind(("127.0.0.1", 0)).unwrap(), None).unwrap();
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

    /// The extension WebSocket's gates, over a real socket on an ephemeral port.
    #[test]
    fn ext_websocket_gates() {
        use tungstenite::client::IntoClientRequest as _;
        use tungstenite::{Message, protocol::frame::coding::CloseCode};

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("app-settings.json"), r#"{"agentChromeControl":true,"agentChromeWrite":false}"#).unwrap();
        let ext = Arc::new(ExtChannel::new(dir.path()));
        let (port, _rx) = serve_on(TcpListener::bind(("127.0.0.1", 0)).unwrap(), Some(ext.clone())).unwrap();
        let url = format!("ws://127.0.0.1:{port}/ext");
        let open = |origin: Option<&str>| {
            let mut req = url.as_str().into_client_request().unwrap();
            if let Some(o) = origin {
                req.headers_mut().insert("Origin", o.parse().unwrap());
            }
            tungstenite::connect(req)
        };
        let status = |r: Result<_, tungstenite::Error>| match r {
            Err(tungstenite::Error::Http(resp)) => resp.status().as_u16(),
            Err(e) => panic!("unexpected error {e}"),
            Ok(_) => 101,
        };
        // Any page can reach loopback and CORS doesn't apply: this is the gate.
        assert_eq!(status(open(Some("https://evil.example"))), 403);
        assert_eq!(status(open(Some("http://localhost:3000"))), 403);
        assert_eq!(status(open(None)), 403);

        let ext_id = "abcdefghijklmnopabcdefghijklmnop";
        let origin = format!("chrome-extension://{ext_id}");
        let identity = json!({ "extensionId": ext_id, "browser": "Chrome 141", "version": "1.0.0" });
        let read_json = |ws: &mut tungstenite::WebSocket<_>| -> Value {
            loop {
                match ws.read().unwrap() {
                    Message::Text(t) => return serde_json::from_str(t.as_str()).unwrap(),
                    Message::Close(f) => return json!({ "closed": f.map(|f| u16::from(f.code)) }),
                    _ => {}
                }
            }
        };

        // A bad token: closed with 4001.
        let (mut ws, _) = open(Some(&origin)).unwrap();
        ws.send(Message::text(json!({ "t": "hello", "protocol": 1, "token": "0".repeat(64), "identity": identity }).to_string())).unwrap();
        assert_eq!(read_json(&mut ws), json!({ "closed": u16::from(CloseCode::from(4001)) }));

        // Seeding tabs before authenticating: closed too.
        let (mut ws, _) = open(Some(&origin)).unwrap();
        ws.send(Message::text(r#"{"t":"tabs","tabs":[{"tabId":1}]}"#)).unwrap();
        assert_eq!(read_json(&mut ws), json!({ "closed": 4001 }));
        assert!(ext.surfaces().is_empty());

        // Pair with the code, push a tab, answer a command.
        let (code, _) = ext.begin_pairing(super::super::ext_channel::now_ms());
        let (mut ws, _) = open(Some(&origin)).unwrap();
        ws.send(Message::text(json!({ "t": "pair", "protocol": 1, "code": code, "identity": identity }).to_string())).unwrap();
        let ok = read_json(&mut ws);
        assert_eq!(ok["t"], "pair-ok");
        ws.send(Message::text(json!({ "t": "tabs", "tabs": [{ "tabId": 7, "title": "T", "url": "http://localhost:3000/", "origin": "http://localhost:3000", "writable": true }] }).to_string())).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while ext.surfaces().is_empty() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(ext.surfaces()[0].surface, "tab:c1.7");
        // agentChromeWrite is off: a write op never reaches the socket.
        let denied = ext.run_command("tab:c1.7", &super::super::ext_protocol::Command::Reload, 0).unwrap_err();
        assert_eq!(denied.error.as_deref(), Some("write_denied"));
        let rx = ext.run_command("tab:c1.7", &super::super::ext_protocol::Command::Info, super::super::ext_channel::now_ms()).unwrap();
        let cmd = read_json(&mut ws);
        assert_eq!((cmd["t"].as_str(), cmd["command"]["op"].as_str()), (Some("cmd"), Some("info")));
        ws.send(Message::text(json!({ "t": "result", "reqId": cmd["reqId"], "ok": true, "data": { "url": "http://localhost:3000/" } }).to_string())).unwrap();
        let res = futures::executor::block_on(rx).unwrap();
        assert!(res.ok);
        assert_eq!(res.data["url"], "http://localhost:3000/");
        // The token was persisted in Lite's own file, not printed anywhere.
        assert!(dir.path().join(super::super::ext_channel::PAIRINGS_FILE).is_file());
        ws.close(None).unwrap();
    }
}
