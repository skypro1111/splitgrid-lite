//! How well a typed prefix matches a candidate name: case-insensitive prefix,
//! then camel/snake "humps" (`uid` → `user_id`, `oI` → `orderItems`), then substring.

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Quality {
    Exact,
    Prefix,
    Humps,
    Substring,
}

pub fn quality(query: &str, name: &str) -> Option<Quality> {
    let (q, n) = (query.as_bytes(), name.as_bytes());
    if q.is_empty() {
        return Some(Quality::Prefix);
    }
    if q.len() > n.len() {
        return None;
    }
    if n[..q.len()].eq_ignore_ascii_case(q) {
        return Some(if q.len() == n.len() {
            Quality::Exact
        } else {
            Quality::Prefix
        });
    }
    if q.len() <= 32 && humps(q, n) {
        return Some(Quality::Humps);
    }
    n.windows(q.len())
        .any(|w| w.eq_ignore_ascii_case(q))
        .then_some(Quality::Substring)
}

/// Word starts: the first character, after `_`/`$`/digits, and lower→upper case changes.
fn is_word_start(n: &[u8], i: usize) -> bool {
    i == 0 || {
        let (p, c) = (n[i - 1], n[i]);
        ((p == b'_' || p == b'$') && c != b'_')
            || (p.is_ascii_lowercase() && c.is_ascii_uppercase())
            || (p.is_ascii_digit() != c.is_ascii_digit())
    }
}

/// Every query character either continues the current word of `n` or starts a later word.
fn humps(q: &[u8], n: &[u8]) -> bool {
    fn go(q: &[u8], n: &[u8], ni: usize, budget: &mut u32) -> bool {
        let Some(&c) = q.first() else { return true };
        if *budget == 0 {
            return false;
        }
        *budget -= 1;
        // Continue the current word.
        if ni > 0
            && ni < n.len()
            && n[ni].eq_ignore_ascii_case(&c)
            && go(&q[1..], n, ni + 1, budget)
        {
            return true;
        }
        // Jump to a later word start.
        (ni.max(1)..n.len()).any(|j| {
            is_word_start(n, j) && n[j].eq_ignore_ascii_case(&c) && go(&q[1..], n, j + 1, budget)
        })
    }
    // The first character must start the name's first word.
    !n.is_empty() && n[0].eq_ignore_ascii_case(&q[0]) && go(&q[1..], n, 1, &mut 2_000)
}

#[cfg(test)]
mod tests {
    use super::Quality::*;
    use super::*;

    #[test]
    fn match_quality() {
        let cases: &[(&str, &str, Option<Quality>)] = &[
            ("", "users", Some(Prefix)),
            ("users", "USERS", Some(Exact)),
            ("us", "users", Some(Prefix)),
            ("US", "user_sessions", Some(Prefix)),
            ("uid", "user_id", Some(Humps)),
            ("usid", "user_id", Some(Humps)),
            ("oi", "orderItems", Some(Humps)),
            ("ui", "UserId", Some(Humps)),
            ("ci", "created_at_id", Some(Humps)),
            ("ser", "users", Some(Substring)),
            ("id", "user_id", Some(Substring)),
            ("xyz", "users", None),
            ("usersx", "users", None),
            ("ua", "users", None),
        ];
        for (q, n, want) in cases {
            assert_eq!(quality(q, n), *want, "{q} vs {n}");
        }
    }

    #[test]
    fn pathological_names_stay_cheap() {
        let n = "a_".repeat(60);
        assert!(quality(&"a".repeat(30), &n).is_some());
        assert_eq!(quality(&format!("{}b", "a".repeat(30)), &n), None);
    }
}
