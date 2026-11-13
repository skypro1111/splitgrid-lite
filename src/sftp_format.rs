//! How the file tables print sizes, times and permissions (the Electron app's
//! shared/sftp-format.ts). Pure: the clock is passed in.

/// 1024-based, one decimal from KB up; "0 B" for nothing.
pub fn size(bytes: u64) -> String {
    if bytes == 0 {
        return "0 B".into();
    }
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let units = ["KB", "MB", "GB", "TB"];
    let mut value = bytes as f64 / 1024.;
    let mut unit = 0;
    while value >= 1024. && unit < units.len() - 1 {
        value /= 1024.;
        unit += 1;
    }
    format!("{value:.1} {}", units[unit])
}

/// "just now", "5m ago", "3h ago", "2d ago" within a week, else the local date "YYYY-MM-DD".
pub fn mtime(mtime_secs: u64, now_secs: u64) -> String {
    let diff = now_secs.saturating_sub(mtime_secs);
    match diff {
        d if d < 60 => "just now".into(),
        d if d < 3600 => format!("{}m ago", d / 60),
        d if d < 86_400 => format!("{}h ago", d / 3600),
        d if d < 7 * 86_400 => format!("{}d ago", d / 86_400),
        _ => local_date(mtime_secs),
    }
}

fn local_date(secs: u64) -> String {
    let t = secs as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: localtime_r fills `tm` from a valid time_t.
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
        return String::new();
    }
    format!("{:04}-{:02}-{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday)
}

/// "drwxr-xr-x": 'l' for a symlink, 'd' for a directory, then the low nine bits.
pub fn mode(mode: u32) -> String {
    const S_IFMT: u32 = 0o170000;
    let kind = match mode & S_IFMT {
        0o120000 => 'l',
        0o040000 => 'd',
        _ => '-',
    };
    let mut out = String::from(kind);
    for i in (0..9).rev() {
        out.push(if mode & (1 << i) != 0 { ['r', 'w', 'x'][(8 - i) % 3] } else { '-' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes() {
        assert_eq!(size(0), "0 B");
        assert_eq!(size(512), "512 B");
        assert_eq!(size(1536), "1.5 KB");
        assert_eq!(size(5 * 1024 * 1024), "5.0 MB");
    }

    #[test]
    fn times() {
        let now = 1_000_000_000;
        assert_eq!(mtime(now - 10, now), "just now");
        assert_eq!(mtime(now - 300, now), "5m ago");
        assert_eq!(mtime(now - 3 * 3600, now), "3h ago");
        assert_eq!(mtime(now - 2 * 86_400, now), "2d ago");
        assert_eq!(mtime(now - 30 * 86_400, now).len(), 10, "an absolute date");
    }

    #[test]
    fn modes() {
        assert_eq!(mode(0o040755), "drwxr-xr-x");
        assert_eq!(mode(0o100644), "-rw-r--r--");
        assert_eq!(mode(0o120777), "lrwxrwxrwx");
    }
}
