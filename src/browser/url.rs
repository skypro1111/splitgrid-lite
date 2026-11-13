//! Address-bar normalisation (the Electron app's shared/browser-url.ts) and the
//! partition → data-store mapping. Pure, so both are unit-tested.
//!
//! Local files and dev servers are first-class: an absolute path becomes a
//! file:// URL and a scheme-less localhost / IP address gets http://. Bare
//! domains get https://, anything else is a Google search.

use std::sync::LazyLock;

use regex::Regex;

static KEEP_SCHEME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^(https?|file)://").unwrap());
static ABOUT_BLANK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^about:blank$").unwrap());
static LOCAL_HOST: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(localhost|127(?:\.[0-9]{1,3}){3}|0\.0\.0\.0|\[::1?\])(?::[0-9]+)?(?:[/?#]|$)").unwrap());
static IPV4_HOST: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9]{1,3}(?:\.[0-9]{1,3}){3}(?::[0-9]+)?(?:[/?#]|$)").unwrap());
static DOMAIN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_-]+\.[A-Za-z0-9_]{2,}").unwrap());
static UNC: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\\\\[^\\]+\\").unwrap());
static DRIVE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^[a-z]:[\\/]").unwrap());

/// JavaScript's `encodeURIComponent`.
pub fn encode_uri_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Percent-encode each path segment, leaving the separators alone: a file name
/// with '#', '?' or '%' must not read as a fragment, query or escape.
fn encode_path<'a>(segments: impl Iterator<Item = &'a str>) -> String {
    segments.map(encode_uri_component).collect::<Vec<_>>().join("/")
}

/// What the address bar (or `sg-browser open`) typed turns into; "" for blank input.
pub fn normalize_input(input: &str) -> String {
    let text = input.trim();
    if text.is_empty() {
        return String::new();
    }
    if KEEP_SCHEME.is_match(text) || ABOUT_BLANK.is_match(text) {
        return text.to_owned();
    }
    // Windows UNC (\\server\share\…).
    if UNC.is_match(text) {
        let mut parts = text[2..].split('\\');
        let host = parts.next().unwrap_or_default();
        return format!("file://{host}/{}", encode_path(parts));
    }
    // Windows drive path: C:\dir\file.html or C:/dir/file.html.
    if DRIVE.is_match(text) {
        let mut parts = text.split(['\\', '/']);
        let drive = parts.next().unwrap_or_default();
        return format!("file:///{drive}/{}", encode_path(parts));
    }
    // POSIX absolute path.
    if text.starts_with('/') {
        return format!("file://{}", encode_path(text.split('/')));
    }
    if !text.contains(' ') {
        if LOCAL_HOST.is_match(text) || IPV4_HOST.is_match(text) {
            return format!("http://{text}");
        }
        if DOMAIN.is_match(text) {
            return format!("https://{text}");
        }
    }
    format!("https://www.google.com/search?q={}", encode_uri_component(text))
}

/// The address bar shows nothing for the blank page.
pub fn display_url(url: &str) -> &str {
    if url == "about:blank" { "" } else { url }
}

/// The partition every pane uses unless its content names another (Electron's default).
pub const DEFAULT_PARTITION: &str = "persist:browser";

/// A stable WKWebsiteDataStore identifier for a partition name: the first 16
/// bytes of its SHA-256, stamped as an RFC 4122 name-based UUID.
pub fn partition_store_id(partition: &str) -> [u8; 16] {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(format!("splitgrid-browser-partition:{partition}").as_bytes());
    let mut id = [0u8; 16];
    id.copy_from_slice(&digest[..16]);
    id[6] = (id[6] & 0x0f) | 0x50;
    id[8] = (id[8] & 0x3f) | 0x80;
    id
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_http_file_and_about_blank() {
        assert_eq!(normalize_input("http://localhost:3000/login"), "http://localhost:3000/login");
        assert_eq!(normalize_input("  https://splitgrid.dev "), "https://splitgrid.dev");
        assert_eq!(normalize_input("file:///tmp/a.html"), "file:///tmp/a.html");
        assert_eq!(normalize_input("about:blank"), "about:blank");
        assert_eq!(normalize_input("HTTPS://Example.com"), "HTTPS://Example.com");
    }

    #[test]
    fn posix_paths_become_escaped_file_urls() {
        assert_eq!(normalize_input("/Users/me/site/index.html"), "file:///Users/me/site/index.html");
        assert_eq!(normalize_input("/Users/me/My Drive/plan #2.html"), "file:///Users/me/My%20Drive/plan%20%232.html");
    }

    #[test]
    fn windows_drive_and_unc_paths() {
        assert_eq!(normalize_input("C:\\work\\site\\index.html"), "file:///C:/work/site/index.html");
        assert_eq!(normalize_input("d:/a b/x.html"), "file:///d:/a%20b/x.html");
        assert_eq!(normalize_input("\\\\wsl.localhost\\Ubuntu\\home\\me\\x.html"), "file://wsl.localhost/Ubuntu/home/me/x.html");
    }

    #[test]
    fn local_hosts_and_ips_get_http() {
        assert_eq!(normalize_input("localhost:5173"), "http://localhost:5173");
        assert_eq!(normalize_input("localhost"), "http://localhost");
        assert_eq!(normalize_input("127.0.0.1:8000/plans/x.html"), "http://127.0.0.1:8000/plans/x.html");
        assert_eq!(normalize_input("0.0.0.0:3000"), "http://0.0.0.0:3000");
        assert_eq!(normalize_input("[::1]:8080"), "http://[::1]:8080");
        assert_eq!(normalize_input("192.168.1.20:8080"), "http://192.168.1.20:8080");
    }

    #[test]
    fn bare_domains_get_https_and_the_rest_is_searched() {
        assert_eq!(normalize_input("splitgrid.dev/docs"), "https://splitgrid.dev/docs");
        assert_eq!(normalize_input("electron webview file url"), "https://www.google.com/search?q=electron%20webview%20file%20url");
        assert_eq!(normalize_input("javascript:alert(1)"), "https://www.google.com/search?q=javascript%3Aalert(1)");
        assert_eq!(normalize_input("привіт"), "https://www.google.com/search?q=%D0%BF%D1%80%D0%B8%D0%B2%D1%96%D1%82");
    }

    #[test]
    fn blank_input_is_empty() {
        assert_eq!(normalize_input("   "), "");
    }

    #[test]
    fn display_hides_about_blank() {
        assert_eq!(display_url("about:blank"), "");
        assert_eq!(display_url("https://a.b"), "https://a.b");
    }

    #[test]
    fn partition_ids_are_stable_distinct_uuids() {
        let a = partition_store_id(DEFAULT_PARTITION);
        assert_eq!(a, partition_store_id(DEFAULT_PARTITION));
        assert_ne!(a, partition_store_id("persist:other"));
        assert_eq!(a[6] >> 4, 5);
        assert_eq!(a[8] >> 6, 0b10);
    }
}
