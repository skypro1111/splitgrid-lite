//! "Why is this file / line like this?" (port of src/main/history/git-why.ts).
//!
//! Git says WHEN a line or file last changed, the index says WHICH sessions
//! edited that file and when; the edits between the previous commit and this
//! one are the conversation that produced the change. Timestamps are the join
//! key, so this is a strong hint, not proof.

use std::collections::HashSet;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use crate::util::run_command;
use crate::{HistoryWhyEntry, WhyCommit, WhyEdit};

/// Edits shortly after the commit time still belong to it (clock skew, amend).
pub const WHY_GRACE_MS: i64 = 10 * 60_000;
/// Commits examined for a whole-file `why`.
pub const WHY_MAX_COMMITS: usize = 5;
/// Session edits reported per commit.
pub const WHY_EDITS_PER_COMMIT: usize = 3;

#[derive(Clone, Debug, PartialEq)]
pub struct FileCommits {
    /// Newest first.
    pub commits: Vec<WhyCommit>,
    /// Time of the file's commit just older than the oldest in `commits`, or
    /// None when that oldest commit is the file's first.
    pub older_ts: Option<i64>,
}

fn is_sha(s: &str) -> bool {
    s.len() == 40
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// First commit block of `git blame --porcelain -L n,n`.
pub fn parse_blame_porcelain(out: &str) -> Option<WhyCommit> {
    let lines: Vec<&str> = out.split('\n').collect();
    let mut head = lines.first()?.split(' ');
    let sha = head.next()?;
    let a = head.next()?;
    let b = head.next()?;
    if !is_sha(sha)
        || a.is_empty()
        || b.is_empty()
        || !a.bytes().all(|c| c.is_ascii_digit())
        || !b.bytes().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    let field = |name: &str| {
        let prefix = format!("{name} ");
        lines
            .iter()
            .find_map(|l| l.strip_prefix(prefix.as_str()))
            .unwrap_or("")
            .to_string()
    };
    let uncommitted = sha.bytes().all(|c| c == b'0');
    Some(WhyCommit {
        sha: sha.to_string(),
        ts: field("author-time")
            .trim()
            .parse::<i64>()
            .map(|s| s * 1000)
            .unwrap_or(0),
        subject: if uncommitted {
            "(not committed yet)".into()
        } else {
            field("summary")
        },
        author: field("author"),
        uncommitted,
    })
}

/// `git log --format=%H%x1f%at%x1f%an%x1f%s`, newest first.
pub fn parse_log(out: &str) -> Vec<WhyCommit> {
    out.split('\n')
        .filter(|l| !l.is_empty())
        .map(|l| l.split('\x1f').collect::<Vec<_>>())
        .filter(|p| p.len() >= 4 && is_sha(p[0]))
        .map(|p| WhyCommit {
            sha: p[0].to_string(),
            ts: p[1].trim().parse::<i64>().map(|s| s * 1000).unwrap_or(0),
            author: p[2].to_string(),
            subject: p[3..].join("\x1f"),
            uncommitted: false,
        })
        .collect()
}

/// Runs git and reports success even on failure (callers decide), like git-manager's runGit.
fn run_git(cwd: &Path, args: &[&str]) -> (bool, String) {
    let mut cmd = Command::new("git");
    cmd.args(args)
        .current_dir(cwd)
        // Never block on a credential prompt, never fight the user's git for the index lock.
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("LC_ALL", "C")
        .env("GIT_PAGER", "cat")
        .env("PAGER", "cat");
    let out = run_command(cmd, Duration::from_secs(30));
    (out.ok, out.stdout)
}

fn last_commit_ts(cwd: &Path, rev: &str, name: &str) -> Option<i64> {
    let (ok, out) = run_git(
        cwd,
        &[
            "--literal-pathspecs",
            "log",
            "-n1",
            "--format=%at",
            rev,
            "--",
            name,
        ],
    );
    let t: i64 = if ok {
        out.trim().parse().ok()?
    } else {
        return None;
    };
    (t > 0).then_some(t * 1000)
}

/// The commits that explain `file` (absolute path): the one that last touched
/// `line` when given, else the file's last few commits, plus the time bound
/// below them. A file outside a repo (or any git failure) yields no commits.
pub fn commits_for_file(file: &Path, line: Option<u32>) -> FileCommits {
    let empty = FileCommits {
        commits: vec![],
        older_ts: None,
    };
    let (Some(cwd), Some(name)) = (file.parent(), file.file_name()) else {
        return empty;
    };
    let name = name.to_string_lossy();
    if let Some(line) = line {
        let range = format!("{line},{line}");
        let (ok, out) = run_git(
            cwd,
            &[
                "--literal-pathspecs",
                "blame",
                "--porcelain",
                "-L",
                &range,
                "--",
                &name,
            ],
        );
        let Some(c) = (if ok {
            parse_blame_porcelain(&out)
        } else {
            None
        }) else {
            return empty;
        };
        // Uncommitted: the work since the file's last commit. Committed: since
        // the file's previous commit before the blamed one.
        let rev = if c.uncommitted {
            "HEAD".to_string()
        } else {
            format!("{}^", c.sha)
        };
        let older_ts = last_commit_ts(cwd, &rev, &name);
        return FileCommits {
            commits: vec![c],
            older_ts,
        };
    }
    // One extra commit only to learn the bound below the ones reported.
    let n = format!("-n{}", WHY_MAX_COMMITS + 1);
    let (ok, out) = run_git(
        cwd,
        &[
            "--literal-pathspecs",
            "log",
            &n,
            "--follow",
            "--format=%H%x1f%at%x1f%an%x1f%s",
            "--",
            &name,
        ],
    );
    let mut all = if ok { parse_log(&out) } else { vec![] };
    let older_ts = all.get(WHY_MAX_COMMITS).map(|c| c.ts);
    all.truncate(WHY_MAX_COMMITS);
    FileCommits {
        commits: all,
        older_ts,
    }
}

/// Attribute session edits of the file to commits (newest first): an edit
/// belongs to the newest commit it precedes (plus a grace), as long as it is
/// newer than the next-older commit. One entry per session, nearest first.
pub fn match_edits_to_commits(
    fc: &FileCommits,
    edits: &[WhyEdit],
    now: i64,
) -> Vec<HistoryWhyEntry> {
    fc.commits
        .iter()
        .enumerate()
        .map(|(i, commit)| {
            let older = fc.commits.get(i + 1).map(|c| c.ts).or(fc.older_ts);
            let upper = (if commit.uncommitted { now } else { commit.ts }) + WHY_GRACE_MS;
            let lower = older.map(|o| o + WHY_GRACE_MS);
            let mut in_window: Vec<&WhyEdit> = edits
                .iter()
                .filter(|e| lower.is_none_or(|l| e.ts > l) && e.ts <= upper)
                .collect();
            in_window.sort_by_key(|e| std::cmp::Reverse(e.ts));
            let mut seen: HashSet<&str> = HashSet::new();
            let mut picked: Vec<WhyEdit> = Vec::new();
            for e in in_window {
                if !seen.insert(e.session_id.as_str()) {
                    continue;
                }
                picked.push(e.clone());
                if picked.len() >= WHY_EDITS_PER_COMMIT {
                    break;
                }
            }
            HistoryWhyEntry {
                commit: commit.clone(),
                edits: picked,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: i64 = 3_600_000;

    fn sha(c: char) -> String {
        c.to_string().repeat(40)
    }
    fn commit(c: char, ts: i64) -> WhyCommit {
        WhyCommit {
            sha: sha(c),
            ts,
            subject: c.to_string(),
            author: "me".into(),
            uncommitted: false,
        }
    }
    fn edit(sid: &str, ts: i64) -> WhyEdit {
        WhyEdit {
            session_id: sid.into(),
            chunk_id: format!("{sid}:{ts}"),
            ts,
            title: None,
            snippet: String::new(),
        }
    }

    #[test]
    fn reads_blame_including_uncommitted() {
        let out = format!(
            "{} 3 3 1\nauthor Ann\nauthor-time 1700000000\nsummary Fix the retry\nfilename a.ts\n\tcode\n",
            sha('a')
        );
        assert_eq!(
            parse_blame_porcelain(&out),
            Some(WhyCommit {
                sha: sha('a'),
                ts: 1_700_000_000_000,
                subject: "Fix the retry".into(),
                author: "Ann".into(),
                uncommitted: false
            })
        );
        let dirty = format!(
            "{} 1 1 1\nauthor Not Committed Yet\nauthor-time 1700000100\nsummary Version of a.ts from a.ts\n",
            sha('0')
        );
        let c = parse_blame_porcelain(&dirty).unwrap();
        assert!(c.uncommitted);
        assert_eq!(c.subject, "(not committed yet)");
        assert!(parse_blame_porcelain("fatal: no such path").is_none());
    }

    #[test]
    fn reads_log_lines_with_separator_in_subject() {
        let out = format!(
            "{}\x1f1700000000\x1fBob\x1fAdd x\n{}\x1f1600000000\x1fCy\x1fa\x1fb\n\n",
            sha('b'),
            sha('c')
        );
        assert_eq!(
            parse_log(&out),
            vec![
                WhyCommit {
                    sha: sha('b'),
                    ts: 1_700_000_000_000,
                    author: "Bob".into(),
                    subject: "Add x".into(),
                    uncommitted: false
                },
                WhyCommit {
                    sha: sha('c'),
                    ts: 1_600_000_000_000,
                    author: "Cy".into(),
                    subject: "a\x1fb".into(),
                    uncommitted: false
                },
            ]
        );
    }

    #[test]
    fn attributes_work_between_commits() {
        let fc = FileCommits {
            commits: vec![commit('b', 100 * H), commit('a', 50 * H)],
            older_ts: Some(30 * H),
        };
        let edits = [
            edit("s1", 99 * H),
            edit("s1", 98 * H),
            edit("s2", 70 * H),
            edit("s3", 40 * H),
            edit("s4", 100 * H + WHY_GRACE_MS + 1),
        ];
        let r = match_edits_to_commits(&fc, &edits, 0);
        assert_eq!(
            r[0].edits
                .iter()
                .map(|e| e.chunk_id.clone())
                .collect::<Vec<_>>(),
            vec![format!("s1:{}", 99 * H), format!("s2:{}", 70 * H)]
        );
        assert_eq!(
            r[1].edits
                .iter()
                .map(|e| e.session_id.as_str())
                .collect::<Vec<_>>(),
            vec!["s3"]
        );
    }

    #[test]
    fn bounds_oldest_commit() {
        let edits = [edit("days-before", 50 * H - 96 * H)];
        let first = FileCommits {
            commits: vec![commit('a', 50 * H)],
            older_ts: None,
        };
        assert_eq!(match_edits_to_commits(&first, &edits, 0)[0].edits.len(), 1);
        let bounded = FileCommits {
            commits: vec![commit('a', 50 * H)],
            older_ts: Some(10 * H),
        };
        assert!(
            match_edits_to_commits(&bounded, &edits, 0)[0]
                .edits
                .is_empty()
        );
    }

    #[test]
    fn uncommitted_line_uses_edits_since_last_commit() {
        let now = 500 * H;
        let mut c = commit('0', 0);
        c.uncommitted = true;
        let fc = FileCommits {
            commits: vec![c],
            older_ts: Some(now - 10 * H),
        };
        let r = match_edits_to_commits(
            &fc,
            &[edit("s", now - H), edit("before-last-commit", now - 20 * H)],
            now,
        );
        assert_eq!(
            r[0].edits
                .iter()
                .map(|e| e.session_id.as_str())
                .collect::<Vec<_>>(),
            vec!["s"]
        );
    }

    fn git(repo: &Path, args: &[&str]) {
        let st = Command::new("git")
            .args(args)
            .current_dir(repo)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(
            st.status.success(),
            "{}",
            String::from_utf8_lossy(&st.stderr)
        );
    }

    #[test]
    fn real_git_commits_and_blame() {
        let dir = tempfile::tempdir().unwrap();
        let repo = std::fs::canonicalize(dir.path()).unwrap();
        git(&repo, &["init", "-q"]);
        let file = repo.join("a b.ts");
        std::fs::write(&file, "one\ntwo\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-qm", "first"]);
        std::fs::write(&file, "one\nTWO\n").unwrap();
        git(&repo, &["commit", "-qam", "second"]);

        let whole = commits_for_file(&file, None);
        assert_eq!(
            whole
                .commits
                .iter()
                .map(|c| c.subject.as_str())
                .collect::<Vec<_>>(),
            vec!["second", "first"]
        );
        assert_eq!(whole.older_ts, None);
        let first = commits_for_file(&file, Some(1));
        assert_eq!(first.commits.len(), 1);
        assert_eq!(first.commits[0].subject, "first");
        assert_eq!(first.older_ts, None);
        let second = commits_for_file(&file, Some(2));
        assert_eq!(
            second
                .commits
                .iter()
                .map(|c| c.subject.as_str())
                .collect::<Vec<_>>(),
            vec!["second"]
        );
        assert_eq!(second.older_ts, Some(first.commits[0].ts));

        assert_eq!(
            commits_for_file(&std::env::temp_dir().join("sg-history-nope/nope.ts"), None),
            FileCommits {
                commits: vec![],
                older_ts: None
            }
        );
        assert!(commits_for_file(&file, Some(99)).commits.is_empty());
    }
}
