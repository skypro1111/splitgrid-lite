//! macOS system notifications for agents that need the user.
//!
//! Posted through `osascript` for now: it needs no signing or entitlement, at
//! the cost of the banner carrying Script Editor's icon and not reopening the
//! terminal on click. A native UNUserNotificationCenter post is the upgrade
//! once the app is properly signed.

const MAX_TEXT: usize = 200;

fn applescript_string(s: &str) -> String {
    let clipped: String = s.chars().take(MAX_TEXT).collect();
    format!("\"{}\"", clipped.replace('\\', "\\\\").replace('"', "\\\"").replace(['\n', '\r'], " "))
}

/// Post one banner, silently (the user's sound settings govern sound).
pub fn post(title: &str, body: &str) {
    let script = format!("display notification {} with title {}", applescript_string(body), applescript_string(title));
    std::thread::spawn(move || {
        if let Err(e) = std::process::Command::new("/usr/bin/osascript").args(["-e", &script]).output() {
            log::warn!("posting a notification: {e}");
        }
    });
}

#[cfg(test)]
mod tests {
    #[test]
    fn escapes_for_applescript() {
        assert_eq!(super::applescript_string("say \"hi\"\\\nbye"), r#""say \"hi\"\\ bye""#);
    }
}
