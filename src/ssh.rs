//! SSH shells for terminal panes (russh), behaving like the Electron app's
//! ssh-manager.ts: keepalive every 10s (3 misses), xterm-256color PTY, the two
//! SHA-1 key exchanges it adds for old servers, TOFU host keys that fail closed
//! on a change, password (with keyboard-interactive fallback) or key auth.
//!
//! Each session is a task on a small shared tokio runtime. It feeds the
//! channel's output to the terminal's VT parser and reports its state through
//! the terminal's Listener; input and resizes come in as RemoteCmds.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use alacritty_terminal::event::{Event, EventListener as _};
use alacritty_terminal::vte::ansi::Processor;
use anyhow::{Result, anyhow};
use russh::client::{self, Handle, KeyboardInteractiveAuthResponse};
use russh::keys::{HashAlg, PrivateKeyWithHashAlg};
use russh::{ChannelMsg, MethodKind, Preferred, kex, mac};

use crate::model::connections::HostKeyCheck;
use crate::terminal::{RemoteCmd, RemoteEnds, RemoteStatus};

const KEEPALIVE: Duration = Duration::from_secs(10);
const KEEPALIVE_MAX: usize = 3;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Clone)]
pub enum SshAuth {
    Password(String),
    Key { path: PathBuf, passphrase: Option<String> },
}

#[derive(Clone)]
pub struct SshParams {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: SshAuth,
    /// The password to offer at sudo / password prompts (when the connection opts in).
    /// It stays inside the session task; the UI only asks for it to be typed.
    pub offer_password: Option<String>,
}

/// Checks (and on first sight pins) a host key fingerprint, `SHA256:…`.
pub type HostKeyChecker = Arc<dyn Fn(&str) -> HostKeyCheck + Send + Sync>;

pub(crate) fn runtime() -> &'static tokio::runtime::Runtime {
    static RT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("ssh")
            .enable_all()
            .build()
            .expect("tokio runtime for SSH")
    })
}

/// The message for a changed host key — the Electron app's wording, which its
/// UI (and ours) parses back into the "Host key changed" card.
pub fn host_key_changed_message(host: &str, port: u16, expected: &str, actual: &str) -> String {
    format!(
        "Host key verification failed for {host}:{port}: the server's key changed since you last connected \
         (possible man-in-the-middle). Expected {expected}, got {actual}."
    )
}

/// `(host, port, expected, actual)` from [`host_key_changed_message`].
pub fn parse_host_key_changed(msg: &str) -> Option<(String, u16, String, String)> {
    let rest = msg.strip_prefix("Host key verification failed for ")?;
    let (addr, rest) = rest.split_once(": ")?;
    let (host, port) = addr.rsplit_once(':')?;
    let rest = rest.split_once("Expected ")?.1;
    let (expected, rest) = rest.split_once(", got ")?;
    let actual = rest.strip_suffix('.')?;
    Some((host.to_owned(), port.parse().ok()?, expected.to_owned(), actual.to_owned()))
}

pub(crate) struct Client {
    check: HostKeyChecker,
    /// Set when the key didn't match, to explain the refusal.
    mismatch: Arc<Mutex<Option<(String, String)>>>,
}

impl client::Handler for Client {
    type Error = russh::Error;

    async fn check_server_key(&mut self, key: &russh::keys::PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        let fingerprint = key.public_key().fingerprint(HashAlg::Sha256).to_string();
        match (self.check)(&fingerprint) {
            HostKeyCheck::Match | HostKeyCheck::New => Ok(true),
            HostKeyCheck::Mismatch { expected, actual } => {
                *self.mismatch.lock().unwrap() = Some((expected, actual));
                Ok(false)
            }
        }
    }
}

fn config() -> client::Config {
    let mut preferred = Preferred::default();
    // Like the Electron app (ssh-legacy-algorithms.ts): reach old servers with
    // the SHA-1 Diffie-Hellman exchanges, never group1 or DSA.
    let mut kex_list = preferred.kex.to_vec();
    let at = kex_list.iter().position(|k| k.as_ref().starts_with("ext-info")).unwrap_or(kex_list.len());
    kex_list.insert(at, kex::DH_G14_SHA1);
    kex_list.insert(at + 1, kex::DH_GEX_SHA1);
    preferred.kex = kex_list.into();
    let mut macs = preferred.mac.to_vec();
    macs.push(mac::HMAC_SHA1);
    preferred.mac = macs.into();
    client::Config {
        preferred,
        keepalive_interval: Some(KEEPALIVE),
        keepalive_max: KEEPALIVE_MAX,
        nodelay: true,
        ..Default::default()
    }
}

/// Start the session in the background; it reports through `ends.events`.
pub fn start(params: SshParams, check: HostKeyChecker, ends: RemoteEnds) {
    runtime().spawn(async move {
        let events = ends.events.clone();
        events.status(RemoteStatus::Connecting);
        match run(params, check, ends).await {
            Ok(clean) => events.status(RemoteStatus::Closed { clean }),
            Err(e) => events.status(RemoteStatus::Failed(format!("{e:#}"))),
        }
        events.send_event(Event::Wakeup);
    });
}

pub(crate) async fn connect(params: &SshParams, check: HostKeyChecker) -> Result<Handle<Client>> {
    let mismatch = Arc::new(Mutex::new(None));
    let client = Client { check, mismatch: mismatch.clone() };
    let addr = (params.host.as_str(), params.port);
    let connecting = client::connect(Arc::new(config()), addr, client);
    let mut handle = match tokio::time::timeout(CONNECT_TIMEOUT, connecting).await {
        Err(_) => return Err(anyhow!("Timed out connecting to {}:{}", params.host, params.port)),
        Ok(Err(e)) => {
            if let Some((expected, actual)) = mismatch.lock().unwrap().take() {
                return Err(anyhow!(host_key_changed_message(&params.host, params.port, &expected, &actual)));
            }
            return Err(anyhow!(e));
        }
        Ok(Ok(h)) => h,
    };

    let ok = match &params.auth {
        SshAuth::Password(password) => {
            match handle.authenticate_password(&params.username, password).await? {
                client::AuthResult::Success => true,
                client::AuthResult::Failure { remaining_methods, .. } if remaining_methods.contains(&MethodKind::KeyboardInteractive) => {
                    keyboard_interactive(&mut handle, &params.username, password).await?
                }
                _ => false,
            }
        }
        SshAuth::Key { path, passphrase } => {
            let key = russh::keys::load_secret_key(path, passphrase.as_deref())
                .map_err(|e| anyhow!("Failed to read private key: {e}"))?;
            let hash = handle.best_supported_rsa_hash().await?.flatten();
            let key = PrivateKeyWithHashAlg::new(Arc::new(key), hash);
            handle.authenticate_publickey(&params.username, key).await?.success()
        }
    };
    if !ok {
        return Err(anyhow!("All configured authentication methods failed"));
    }
    Ok(handle)
}

/// Settings › Test: connect and authenticate, then hang up. Err carries the reason.
pub async fn test(params: SshParams, check: HostKeyChecker) -> Result<(), String> {
    let handle = runtime()
        .spawn(async move {
            let handle = connect(&params, check).await.map_err(|e| format!("{e:#}"))?;
            let _ = handle.disconnect(russh::Disconnect::ByApplication, "", "en").await;
            Ok::<(), String>(())
        })
        .await;
    handle.map_err(|e| e.to_string())?
}

/// Servers that only take keyboard-interactive: answer every prompt with the password.
async fn keyboard_interactive(handle: &mut Handle<Client>, user: &str, password: &str) -> Result<bool> {
    let mut reply = handle.authenticate_keyboard_interactive_start(user, None::<String>).await?;
    for _ in 0..5 {
        match reply {
            KeyboardInteractiveAuthResponse::Success => return Ok(true),
            KeyboardInteractiveAuthResponse::Failure { .. } => return Ok(false),
            KeyboardInteractiveAuthResponse::InfoRequest { prompts, .. } => {
                let answers = prompts.iter().map(|_| password.to_owned()).collect();
                reply = handle.authenticate_keyboard_interactive_respond(answers).await?;
            }
        }
    }
    Ok(false)
}

/// Ok(true) when the shell ended by itself (exit / logout), Ok(false) when we closed it.
async fn run(params: SshParams, check: HostKeyChecker, mut ends: RemoteEnds) -> Result<bool> {
    let handle = connect(&params, check).await?;
    let mut channel = handle.channel_open_session().await?;
    channel
        .request_pty(false, "xterm-256color", ends.size.cols as u32, ends.size.rows as u32, 0, 0, &[])
        .await?;
    channel.request_shell(false).await?;
    ends.events.status(RemoteStatus::Connected);

    let mut parser: Processor = Processor::new();
    let mut prompt = crate::ssh_prompt::PromptWatch::default();
    loop {
        tokio::select! {
            msg = channel.wait() => match msg {
                Some(ChannelMsg::Data { data }) | Some(ChannelMsg::ExtendedData { data, .. }) => {
                    parser.advance(&mut *ends.term.lock(), &data);
                    if params.offer_password.is_some() {
                        match prompt.feed(&data) {
                            crate::ssh_prompt::Change::Appeared => ends.events.password_prompt(true),
                            crate::ssh_prompt::Change::Gone => ends.events.password_prompt(false),
                            crate::ssh_prompt::Change::None => {}
                        }
                    }
                    ends.events.send_event(Event::Wakeup);
                }
                Some(ChannelMsg::ExitStatus { .. }) | Some(ChannelMsg::Eof) => {}
                Some(ChannelMsg::Close) | None => {
                    // The link is gone if the transport died; a closed channel on a live
                    // transport is the shell exiting.
                    return if handle.is_closed() { Err(anyhow!("Connection lost")) } else { Ok(true) };
                }
                Some(_) => {}
            },
            cmd = ends.commands.recv() => match cmd {
                Some(RemoteCmd::Input(bytes)) => channel.data_bytes(bytes).await?,
                Some(RemoteCmd::Resize { cols, rows }) => channel.window_change(cols as u32, rows as u32, 0, 0).await?,
                Some(RemoteCmd::ApplyPassword) => {
                    if let Some(pw) = &params.offer_password && prompt.pending {
                        prompt.pending = false;
                        ends.events.password_prompt(false);
                        channel.data_bytes(format!("{pw}\n").into_bytes()).await?;
                    }
                }
                Some(RemoteCmd::Close) | None => {
                    let _ = channel.close().await;
                    let _ = handle.disconnect(russh::Disconnect::ByApplication, "", "en").await;
                    return Ok(false);
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_key_message_round_trips() {
        let msg = host_key_changed_message("10.0.0.5", 2222, "SHA256:old", "SHA256:new");
        assert_eq!(
            parse_host_key_changed(&msg),
            Some(("10.0.0.5".into(), 2222, "SHA256:old".into(), "SHA256:new".into()))
        );
        assert!(parse_host_key_changed("Connection refused").is_none());
    }

    #[test]
    fn legacy_algorithms_are_offered_but_not_first() {
        let c = config();
        let kex: Vec<&str> = c.preferred.kex.iter().map(|k| k.as_ref()).collect();
        let g14 = kex.iter().position(|k| *k == "diffie-hellman-group14-sha1").unwrap();
        assert!(g14 > 0);
        assert!(kex.iter().position(|k| k.starts_with("ext-info")).unwrap() > g14, "before the extension markers");
        assert!(!kex.contains(&"diffie-hellman-group1-sha1"));
        assert_eq!(c.keepalive_interval, Some(Duration::from_secs(10)));
    }
}
