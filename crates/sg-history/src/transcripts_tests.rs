// Port of src/main/history/transcripts.test.ts.
// All fixtures are synthetic. Never paste real transcript content here.

use super::*;
use serde_json::{Value, json};
use std::cell::Cell;

const SID: &str = "11111111-2222-3333-4444-555555555555";
const CWD: &str = "/work/demo";
const T0: i64 = 1_767_225_600_000; // 2026-01-01T00:00:00.000Z

thread_local! {
    static CLOCK: Cell<i64> = const { Cell::new(T0) };
    static UUID: Cell<u64> = const { Cell::new(0) };
}

fn reset() {
    CLOCK.with(|c| c.set(T0));
    UUID.with(|c| c.set(0));
}

fn iso(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .unwrap()
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
}

fn base(extra: Value) -> Value {
    let ts = CLOCK.with(|c| {
        c.set(c.get() + 1000);
        c.get()
    });
    let n = UUID.with(|c| {
        c.set(c.get() + 1);
        c.get()
    });
    let mut v = json!({ "uuid": format!("u-{n}"), "timestamp": iso(ts), "sessionId": SID, "cwd": CWD, "gitBranch": "main" });
    if let Value::Object(e) = extra {
        for (k, val) in e {
            v[k] = val;
        }
    }
    v
}

fn merge(mut v: Value, extra: Value) -> Value {
    if let Value::Object(e) = extra {
        for (k, val) in e {
            v[k] = val;
        }
    }
    v
}

fn user(content: Value) -> Value {
    base(json!({ "type": "user", "message": { "role": "user", "content": content } }))
}
fn user_x(content: Value, extra: Value) -> Value {
    merge(user(content), extra)
}
fn assistant(blocks: Value) -> Value {
    base(
        json!({ "type": "assistant", "message": { "role": "assistant", "model": "claude-test", "content": blocks } }),
    )
}
fn say(text: &str) -> Value {
    assistant(json!([{ "type": "text", "text": text }]))
}
fn think(t: &str) -> Value {
    assistant(json!([{ "type": "thinking", "thinking": t, "signature": "sig" }]))
}
fn tool(name: &str, input: Value) -> Value {
    let n = UUID.with(|c| c.get());
    assistant(
        json!([{ "type": "tool_use", "id": format!("toolu_{n}"), "name": name, "input": input }]),
    )
}
fn tool_result(content: &str) -> Value {
    user(json!([{ "type": "tool_result", "tool_use_id": "toolu_x", "content": content }]))
}
fn jsonl(records: &[Value]) -> String {
    records.iter().map(|r| r.to_string() + "\n").collect()
}
fn parse(raw: &str) -> ParsedTranscript {
    parse_with(raw, ParseOptions::default())
}
fn parse_with(raw: &str, opts: ParseOptions<'_>) -> ParsedTranscript {
    parse_transcript_content(
        raw.as_bytes(),
        &TranscriptSource::from(format!("/p/{SID}.jsonl").as_str()),
        opts,
    )
}
fn ids(chunks: &[TranscriptChunk]) -> Vec<String> {
    chunks.iter().map(|c| c.id.clone()).collect()
}

// ─── chunking ────────────────────────────────────────────────────────────────

#[test]
fn builds_one_chunk_per_real_prompt() {
    reset();
    let p = parse(&jsonl(&[
        user(json!("Please add a dark mode toggle to settings")),
        say("Sure, I will add it."),
        say("Done: the toggle lives in the Appearance tab."),
        user(json!("Now write unit tests for the toggle")),
        say("Tests added."),
    ]));
    assert_eq!(ids(&p.chunks), vec![format!("{SID}:0"), format!("{SID}:1")]);
    assert_eq!(
        p.chunks[0].text,
        "User: Please add a dark mode toggle to settings\n\nAssistant: Sure, I will add it.\nDone: the toggle lives in the Appearance tab."
    );
    assert_eq!(p.chunks[0].kind, ChunkKind::Turn);
    assert_eq!(p.chunks[0].session_id, SID);
    assert_eq!(p.chunks[0].ts, T0 + 1000);
    assert_eq!(p.chunks[0].hash.len(), 40);
    assert!(p.chunks[0].hash.bytes().all(|b| b.is_ascii_hexdigit()));
    assert_eq!(p.meta.id, SID);
    assert_eq!(p.meta.cwds, vec![CWD]);
    assert_eq!(p.meta.git_branch.as_deref(), Some("main"));
    assert_eq!(p.meta.first_ts, T0 + 1000);
    assert_eq!(p.meta.last_ts, T0 + 5000);
}

#[test]
fn hash_matches_the_electron_formula() {
    // sha1(text + "\0" + files.join("\n")), as node's createHash produces it.
    assert_eq!(hash_chunk("abc", &[]), crate::util::sha1_hex(&[b"abc\0"]));
    assert_eq!(
        hash_chunk("x", &["/a".into(), "/b".into()]),
        crate::util::sha1_hex(&[b"x\0/a\n/b"])
    );
}

#[test]
fn marks_thinking_and_skips_empty_thinking() {
    reset();
    let p = parse(&jsonl(&[
        user(json!("Why does the build fail on CI?")),
        think(""),
        think("The lockfile is stale."),
        say("Regenerate the lockfile."),
    ]));
    assert_eq!(
        p.chunks[0].text,
        "User: Why does the build fail on CI?\n\nAssistant: [thinking] The lockfile is stale.\nRegenerate the lockfile."
    );
}

#[test]
fn tool_markers_and_edited_files() {
    reset();
    let p = parse(&jsonl(&[
        user(json!("Fix the failing parser test please")),
        tool(
            "Bash",
            json!({ "command": "npx vitest run src/parser", "description": "Run parser tests" }),
        ),
        tool_result(&"FAIL src/parser.test.ts\n".repeat(200)),
        tool(
            "Bash",
            json!({ "command": format!("echo {}", "x".repeat(300)) }),
        ),
        tool("Read", json!({ "file_path": "/work/demo/src/parser.ts" })),
        tool(
            "Edit",
            json!({ "file_path": "/work/demo/src/parser.ts", "old_string": "a", "new_string": "b" }),
        ),
        tool(
            "Write",
            json!({ "file_path": "notes/plan.md", "content": "x" }),
        ),
        tool(
            "NotebookEdit",
            json!({ "notebook_path": "/work/demo/nb.ipynb", "new_source": "x" }),
        ),
        tool("Grep", json!({ "pattern": "parseLine", "path": "src" })),
        say("Fixed the off-by-one in parseLine."),
    ]));
    let text = &p.chunks[0].text;
    assert!(text.contains("[Bash] Run parser tests"));
    assert!(text.contains("[Read] /work/demo/src/parser.ts"));
    assert!(text.contains("[Edit] /work/demo/src/parser.ts"));
    assert!(text.contains("[Grep] parseLine in src"));
    assert!(!text.contains("FAIL src/parser.test.ts"));
    let long = text
        .split('\n')
        .find(|l| l.starts_with("[Bash] echo"))
        .unwrap();
    assert!(crate::util::len16(long) <= "[Bash] ".len() + 101);
    assert!(long.ends_with('…'));
    assert_eq!(
        p.chunks[0].files,
        vec![
            "/work/demo/src/parser.ts",
            "/work/demo/notes/plan.md",
            "/work/demo/nb.ipynb"
        ]
    );
}

#[test]
fn tool_result_only_user_record_is_not_a_prompt() {
    reset();
    let p = parse(&jsonl(&[
        user(json!("List the files in the repository root")),
        tool("Bash", json!({ "command": "ls" })),
        tool_result("README.md\nsrc"),
        say("There are two entries."),
    ]));
    assert_eq!(p.chunks.len(), 1);
    assert!(p.chunks[0].text.contains("There are two entries."));
}

#[test]
fn ignores_system_generated_user_records() {
    reset();
    let p = parse(&jsonl(&[
        user(json!("Refactor the logging module for clarity")),
        say("Refactored."),
        user(json!(
            "<local-command-stdout>Set model to opus</local-command-stdout>"
        )),
        user(json!(
            "<system-reminder>The date changed.</system-reminder>"
        )),
        user(json!("[Request interrupted by user for tool use]")),
        user(json!(
            "Caveat: The messages below were generated by the user while running local commands."
        )),
        user(json!("<bash-input>git status</bash-input>")),
        user(json!(
            "<bash-stdout>On branch main</bash-stdout><bash-stderr></bash-stderr>"
        )),
        user_x(
            json!("Base directory for this skill: /skills/x"),
            json!({ "isMeta": true }),
        ),
        user_x(
            json!("Your usage limit has reset. Continue the task."),
            json!({ "isMeta": true, "turnOrigin": "auto_continuation" }),
        ),
        say("Continuing with the refactor."),
    ]));
    assert_eq!(p.chunks.len(), 1);
    assert_eq!(
        p.chunks[0].text,
        "User: Refactor the logging module for clarity\n\nAssistant: Refactored.\nContinuing with the refactor."
    );
}

#[test]
fn strips_inline_reminders_but_keeps_prompt() {
    reset();
    let p = parse(&jsonl(&[
        user(json!([
            { "type": "text", "text": "Summarize the open pull requests" },
            { "type": "text", "text": "<system-reminder>internal context</system-reminder>" },
        ])),
        say("There are three."),
    ]));
    assert_eq!(
        p.chunks[0].text,
        "User: Summarize the open pull requests\n\nAssistant: There are three."
    );
}

#[test]
fn keeps_pasted_markup_prompt() {
    reset();
    let p = parse(&jsonl(&[
        user(json!("<div class=\"card\">why is this not centered?</div>")),
        say("Use flexbox."),
    ]));
    assert!(p.chunks[0].text.contains("User: <div class=\"card\">"));
}

#[test]
fn folds_task_notifications() {
    reset();
    let n = "<task-notification>\n<task-id>bg1</task-id>\n<status>completed</status>\n<summary>Background build finished</summary>\n</task-notification>";
    let p = parse(&jsonl(&[
        user(json!("Kick off the release build in the background")),
        say("Started the build."),
        user_x(
            json!(n),
            json!({ "origin": { "kind": "task-notification" }, "turnOrigin": "task_notification" }),
        ),
        say("The build passed."),
        base(
            json!({ "type": "attachment", "attachment": { "type": "queued_command", "commandMode": "task-notification", "prompt": n } }),
        ),
    ]));
    assert_eq!(p.chunks.len(), 1);
    assert!(
        p.chunks[0]
            .text
            .contains("[task-notification] Background build finished")
    );
    assert!(p.chunks[0].text.contains("The build passed."));
    assert!(!p.chunks[0].text.contains("<task-id>"));
}

#[test]
fn slash_command_is_a_prompt_and_dropped_without_answer() {
    reset();
    let p = parse(&jsonl(&[
        user(json!(
            "<command-name>/model</command-name>\n<command-message>model</command-message>\n<command-args>opus</command-args>"
        )),
        user(json!(
            "<local-command-stdout>Set model</local-command-stdout>"
        )),
        user(json!(
            "<command-message>review</command-message>\n<command-name>/review</command-name>\n<command-args>the auth module</command-args>"
        )),
        user_x(json!("Skill body expanded here"), json!({ "isMeta": true })),
        say("Reviewing the auth module now."),
    ]));
    assert_eq!(ids(&p.chunks), vec![format!("{SID}:1")]);
    assert_eq!(
        p.chunks[0].text,
        "User: /review the auth module\n\nAssistant: Reviewing the auth module now."
    );
}

#[test]
fn queued_prompts_peer_messages_and_scheduled_wakeups() {
    reset();
    let p = parse(&jsonl(&[
        user(json!("Migrate the database schema to v2")),
        say("Migrating."),
        base(
            json!({ "type": "attachment", "attachment": { "type": "queued_command", "commandMode": "prompt", "prompt": "Also back up the old tables first", "origin": { "kind": "human" } } }),
        ),
        say("Backing up first."),
        user(json!("Also back up the old tables first")),
        say("Backup complete."),
        user_x(
            json!(
                "Another Claude session sent a message:\n<agent-message from=\"abc\">Staging is free now</agent-message>"
            ),
            json!({ "isMeta": true, "origin": { "kind": "peer" } }),
        ),
        say("Deploying to staging."),
        user_x(
            json!("Check whether the nightly job finished"),
            json!({ "isMeta": true, "turnOrigin": "scheduled" }),
        ),
        say("It finished."),
    ]));
    let firsts: Vec<&str> = p
        .chunks
        .iter()
        .map(|c| c.text.split('\n').next().unwrap())
        .collect();
    assert_eq!(
        firsts,
        vec![
            "User: Migrate the database schema to v2",
            "User: Also back up the old tables first",
            "User: [peer message] Staging is free now",
            "User: [scheduled] Check whether the nightly job finished",
        ]
    );
    assert!(p.chunks[1].text.contains("Backup complete."));
}

#[test]
fn folds_short_prompts() {
    reset();
    let p = parse(&jsonl(&[
        user(json!("Draft a plan for the caching layer")),
        say("Plan: 1. LRU 2. TTL"),
        user(json!("yes go")),
        say("Implemented both."),
    ]));
    assert_eq!(p.chunks.len(), 1);
    assert_eq!(
        p.chunks[0].text,
        "User: Draft a plan for the caching layer\n\nAssistant: Plan: 1. LRU 2. TTL\n\nUser: yes go\n\nAssistant: Implemented both."
    );
    let raw = jsonl(&[
        user(json!("Draft a plan")),
        say("a"),
        user(json!("yes go")),
        say("b"),
    ]);
    assert_eq!(
        parse_with(
            &raw,
            ParseOptions {
                min_prompt_chars: Some(3),
                ..Default::default()
            }
        )
        .chunks
        .len(),
        2
    );
}

#[test]
fn skips_turns_without_assistant_text() {
    reset();
    let p = parse(&jsonl(&[
        user(json!("Run the linter over the whole repository")),
        tool("Bash", json!({ "command": "npm run lint" })),
        tool_result("ok"),
        user(json!("Now explain what the linter config does")),
        say("It extends the recommended rules."),
    ]));
    assert_eq!(p.chunks.iter().map(|c| c.seq).collect::<Vec<_>>(), vec![1]);
}

#[test]
fn compact_summaries_without_ending_the_turn() {
    reset();
    let p = parse(&jsonl(&[
        user(json!("Port the scheduler to the new queue API")),
        say("Starting the port."),
        base(json!({ "type": "system", "subtype": "compact_boundary" })),
        user_x(
            json!(
                "This session is being continued from a previous conversation. Summary: scheduler port in progress."
            ),
            json!({ "isCompactSummary": true, "isVisibleInTranscriptOnly": true }),
        ),
        say("Finished the port after compaction."),
    ]));
    let got: Vec<(String, ChunkKind)> = p.chunks.iter().map(|c| (c.id.clone(), c.kind)).collect();
    assert_eq!(
        got,
        vec![
            (format!("{SID}:0"), ChunkKind::Turn),
            (format!("{SID}:1"), ChunkKind::CompactSummary)
        ]
    );
    assert!(
        p.chunks[0]
            .text
            .contains("Finished the port after compaction.")
    );
    assert!(
        p.chunks[1]
            .text
            .starts_with("Compaction summary: This session is being continued")
    );
}

#[test]
fn skips_api_errors_and_synthetic_messages() {
    reset();
    let p = parse(&jsonl(&[
        user(json!("Generate the changelog for this release")),
        merge(
            assistant(json!([{ "type": "text", "text": "API Error: overloaded" }])),
            json!({ "isApiErrorMessage": true }),
        ),
        base(
            json!({ "type": "assistant", "message": { "role": "assistant", "model": "<synthetic>", "content": [{ "type": "text", "text": "No response requested." }] } }),
        ),
    ]));
    assert!(p.chunks.is_empty());
}

#[test]
fn dedupes_replayed_records() {
    reset();
    let prompt = user(json!("Explain the retry policy in the client"));
    let answer = say("It retries three times with backoff.");
    let p = parse(&jsonl(&[prompt.clone(), answer.clone(), prompt, answer]));
    assert_eq!(p.chunks.len(), 1);
    assert_eq!(p.chunks[0].text.matches("retries three times").count(), 1);
}

#[test]
fn splits_long_turns_carrying_the_prompt() {
    reset();
    let mut recs = vec![user(json!("Write the full design document for sync"))];
    for i in 0..60 {
        recs.push(say(&format!(
            "Paragraph {i}: {}",
            "lorem ipsum ".repeat(80)
        )));
    }
    recs.push(tool("Write", json!({ "file_path": "/work/demo/doc.md" })));
    recs.push(say("end"));
    let p = parse(&jsonl(&recs));
    assert!(p.chunks.len() > 2);
    assert_eq!(p.chunks[0].id, format!("{SID}:0"));
    assert_eq!(p.chunks[1].id, format!("{SID}:0.1"));
    for c in &p.chunks {
        assert!(crate::util::len16(&c.text) <= MAX_CHUNK_CHARS);
        assert_eq!(c.seq, 0);
    }
    assert!(
        p.chunks[1].text.starts_with(
            "User: Write the full design document for sync (continued)\n\nAssistant: "
        )
    );
    assert_eq!(p.chunks.last().unwrap().files, vec!["/work/demo/doc.md"]);
    assert!(p.chunks[0].files.is_empty());
    let all: String = p
        .chunks
        .iter()
        .map(|c| c.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    for i in 0..60 {
        assert_eq!(
            all.matches(&format!("Paragraph {i}:")).count(),
            1,
            "paragraph {i}"
        );
    }
}

#[test]
fn hard_splits_one_oversized_block() {
    reset();
    let raw = jsonl(&[
        user(json!("Paste the whole log file here")),
        say(&"y".repeat(50_000)),
    ]);
    let p = parse_with(
        &raw,
        ParseOptions {
            max_chunk_chars: Some(10_000),
            ..Default::default()
        },
    );
    assert!(p.chunks.len() >= 5);
    for c in &p.chunks {
        assert!(c.text.len() <= 10_000);
    }
}

#[test]
fn ids_and_hashes_stable_as_file_grows() {
    reset();
    let head = jsonl(&[
        user(json!("First task: rename the config loader")),
        say("Renamed."),
        user(json!("Second task: add a migration for it")),
        say("Added."),
    ]);
    let before = parse(&head).chunks;
    let grown = parse(
        &(head.clone()
            + &jsonl(&[
                say("Also updated the docs."),
                user(json!("Third task: release notes please")),
                say("Written."),
            ])),
    )
    .chunks;
    assert_eq!(
        ids(&grown),
        vec![format!("{SID}:0"), format!("{SID}:1"), format!("{SID}:2")]
    );
    assert_eq!(grown[0].hash, before[0].hash);
    assert_ne!(grown[1].hash, before[1].hash);

    let long = |n: usize| {
        UUID.with(|c| c.set(0));
        let mut recs = vec![user(json!("Write the long report on the outage"))];
        for i in 0..n {
            recs.push(say(&format!("Section {i} {}", "z".repeat(3000))));
        }
        jsonl(&recs)
    };
    let short = parse(&long(10)).chunks;
    let longer = parse(&long(20)).chunks;
    assert!(longer.len() > short.len());
    for i in 0..short.len() - 1 {
        assert_eq!(longer[i].id, short[i].id);
        // Timestamps differ between the two builds (the clock moved), so compare text.
        assert_eq!(longer[i].text, short[i].text);
        assert_eq!(longer[i].hash, short[i].hash);
    }
}

#[test]
fn tolerates_truncated_last_line() {
    reset();
    let raw = jsonl(&[
        user(json!("Audit the dependency licenses")),
        say("All MIT."),
    ]) + "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"Next ta";
    let p = parse(&raw);
    assert_eq!(p.chunks.len(), 1);
    assert!(p.chunks[0].text.contains("All MIT."));
}

#[test]
fn collects_cwds_titles_and_relocations() {
    reset();
    let raw = jsonl(&[
        user(json!("Investigate the flaky upload test")),
        base(json!({ "type": "ai-title", "aiTitle": "Flaky upload test" })),
        say("Looking."),
        merge(say("Found it in src."), json!({ "cwd": "/work/demo/src" })),
        base(json!({ "type": "ai-title", "aiTitle": "Fix flaky upload test" })),
        json!({ "type": "relocated", "relocatedCwd": "/work/moved", "sessionId": SID }),
    ]);
    let p = parse(&raw);
    assert_eq!(p.meta.cwds, vec![CWD, "/work/demo/src", "/work/moved"]);
    assert_eq!(p.meta.title.as_deref(), Some("Fix flaky upload test"));
    let with_custom = raw
        + &jsonl(&[json!({ "type": "custom-title", "customTitle": "My name", "sessionId": SID })]);
    assert_eq!(parse(&with_custom).meta.title.as_deref(), Some("My name"));
}

#[test]
fn skips_ignored_types_by_prefix_but_reads_type_first_records() {
    reset();
    let mut first = serde_json::Map::new();
    first.insert("type".into(), json!("user"));
    for (k, v) in base(json!({})).as_object().unwrap() {
        first.insert(k.clone(), v.clone());
    }
    first.insert(
        "message".into(),
        json!({ "role": "user", "content": "Rename the queue worker class" }),
    );
    let raw = [
        json!({ "type": "file-history-snapshot", "messageId": "m", "snapshot": { "cwd": "/elsewhere" } }).to_string(),
        json!({ "type": "api-request-blob", "cwd": "/elsewhere", "blob": "x".repeat(1000) }).to_string(),
        json!({ "type": "mode" }).to_string(),
        Value::Object(first).to_string(),
        say("Renamed it.").to_string(),
        json!({ "type": "ai-title", "aiTitle": "Rename worker", "sessionId": SID }).to_string(),
    ]
    .join("\n");
    assert!(raw.starts_with("{\"type\":\"file-history-snapshot\""));
    let p = parse(&raw);
    assert_eq!(
        p.chunks.iter().map(|c| c.text.as_str()).collect::<Vec<_>>(),
        vec!["User: Rename the queue worker class\n\nAssistant: Renamed it."]
    );
    assert_eq!(p.meta.cwds, vec![CWD]);
    assert_eq!(p.meta.title.as_deref(), Some("Rename worker"));
}

#[test]
fn redacts_chunk_text_and_titles() {
    reset();
    let secret = format!("ghp_{}", "A1b2C3d4".repeat(5));
    let p = parse(&jsonl(&[
        user(json!(format!("Use this token {secret} to push"))),
        base(json!({ "type": "ai-title", "aiTitle": format!("Push with {secret}") })),
        say(&format!("Pushed using {secret}.")),
    ]));
    assert!(!p.chunks[0].text.contains(&secret));
    assert!(p.chunks[0].text.contains("[REDACTED]"));
    assert_eq!(p.meta.title.as_deref(), Some("Push with [REDACTED]"));
}

// ─── redactSecrets ───────────────────────────────────────────────────────────

const R: &str = "[REDACTED]";

#[test]
fn redacts_known_secret_shapes() {
    let cases: Vec<(&str, String, String)> = vec![
        ("anthropic", format!("key sk-ant-api03-{}xyz", "aB3_".repeat(10)), format!("key {R}")),
        ("openai legacy", format!("sk-{}", "Ab1".repeat(16)), R.into()),
        ("openai project", format!("sk-proj-{}", "Ab1-_".repeat(10)), R.into()),
        ("github classic", format!("ghp_{}", "a1B2".repeat(9)), R.into()),
        ("github oauth", format!("gho_{}", "a1B2".repeat(9)), R.into()),
        ("github fine-grained", format!("github_pat_11ABCDEFG_{}", "x1Y2".repeat(12)), R.into()),
        ("aws key id", "aws AKIAIOSFODNN7EXAMPLE end".into(), format!("aws {R} end")),
        ("slack", "xoxb-1234567890-abcdefghij".into(), R.into()),
        ("jwt", "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U".into(), R.into()),
        ("pem", "-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEAabc123\nQUJDREVGR0hJSktMTU5PUFFS\n-----END RSA PRIVATE KEY-----".into(), R.into()),
        ("pem openssh", "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAA\n-----END OPENSSH PRIVATE KEY-----".into(), R.into()),
        ("pem truncated", "cat <<EOF\n-----BEGIN PRIVATE KEY-----\nMIIEvQIBADANBgkqhkiG9w0BAQEF".into(), format!("cat <<EOF\n{R}")),
        ("bearer header", "curl -H \"Authorization: Bearer abc123def456ghi789\"".into(), format!("curl -H \"Authorization: Bearer {R}\"")),
        ("basic header", "Authorization: Basic dXNlcjpwYXNzd29yZA==".into(), format!("Authorization: Basic {R}")),
        ("env password", "DB_PASSWORD=hunter".into(), format!("DB_PASSWORD={R}")),
        ("env token", "export GITHUB_TOKEN=\"abcdef0123456789\"".into(), format!("export GITHUB_TOKEN=\"{R}\"")),
        ("yaml password", "password: s3cr3tPass".into(), format!("password: {R}")),
        ("upper colon", "PASSWORD:hunter2!".into(), format!("PASSWORD:{R}")),
        ("json secret", "{\"client_secret\": \"Zx81kq0PqLm2\"}".into(), format!("{{\"client_secret\": \"{R}\"}}")),
        ("camel api key", "const apiKey = 'abcd1234efgh5678';".into(), format!("const apiKey = '{R}';")),
        ("header api key", "-H 'X-Api-Key: 9f8e7d6c5b4a3210'".into(), format!("-H 'X-Api-Key: {R}'")),
        ("passwd", "passwd=letmein99".into(), format!("passwd={R}")),
        ("query token", "https://api.example.com/v1?token=q8w7e6r5t4y3&x=1".into(), format!("https://api.example.com/v1?token={R}&x=1")),
        ("flag equals", "psql --password=Tr0ub4dor".into(), format!("psql --password={R}")),
        ("flag space", "tool --api-key \"k3y-abcdef-123456\"".into(), format!("tool --api-key \"{R}\"")),
        ("flag space bare", "login --password hunter2".into(), format!("login --password {R}")),
        ("sshpass", "sshpass -p 'S3cret!' ssh root@10.0.0.5".into(), format!("sshpass -p {R} ssh root@10.0.0.5")),
        ("mysql glued", "mysql -u root -p'S3cret!' appdb".into(), format!("mysql -u root -p{R} appdb")),
        ("url creds", "postgres://admin:Sup3rS3cret@db.internal:5432/app".into(), format!("postgres://admin:{R}@db.internal:5432/app")),
        ("stripe", "sk_live_4eC39HqLyjWDarjtT1zdp7dc".into(), R.into()),
        ("env pass", "DB_PASS=hunter22".into(), format!("DB_PASS={R}")),
        ("passphrase", "passphrase = `c0rrect-horse`".into(), format!("passphrase = `{R}`")),
    ];
    for (name, input, expected) in cases {
        assert_eq!(redact_secrets(&input), expected, "redacts {name}");
    }
}

#[test]
fn leaves_code_and_prose_alone() {
    let untouched = [
        (
            "prose about passwords",
            "The password must be at least 12 characters and the token expires hourly.",
        ),
        (
            "ts type annotations",
            "interface Creds { password: string; token?: string; apiKey: string }",
        ),
        (
            "code reading a secret",
            "const token = await getToken(); const secret = config.secret; password = hashedPassword",
        ),
        (
            "env references",
            "TOKEN=$GITHUB_TOKEN; PASSWORD=${DB_PASS}; api_key: \"{{ secrets.KEY }}\"",
        ),
        ("process.env", "const apiKey = process.env.OPENAI_API_KEY;"),
        (
            "token counts",
            "max_tokens=4096, input_token: 1234, tokens: 98765432",
        ),
        (
            "metadata keys",
            "TOKEN_URL=https://auth.example.com/token\nSECRET_NAME=prod-db-credentials\npasswordField: \"pwd-input\"",
        ),
        (
            "placeholders",
            "password: ******** and token=<your-token> and PASSWORD=changeme",
        ),
        (
            "bearer variable",
            "curl -H \"Authorization: Bearer $TOKEN\" https://api.example.com",
        ),
        (
            "sk- words",
            "Use sk-learn style pipelines; see the risk-assessment-2026 doc.",
        ),
        (
            "passport and bypass",
            "PASSPORT=issued; bypass=enabled-for-admins; compass: north",
        ),
        ("mysql prompt form", "mysql -u root -p appdb"),
        ("git ssh url", "git clone git@github.com:owner/repo.git"),
        (
            "url env creds",
            "postgres://${DB_USER}:${DB_PASS}@db:5432/app",
        ),
        ("flag mention", "pass the --token flag to the CLI"),
        ("comparison", "if (password === confirmPassword) return;"),
        ("quoted prose", "placeholder: \"Enter your password here\""),
        ("short values", "pass=3 fail=0 token: abc"),
        ("jwt-like words", "eyJ is how base64 JSON starts"),
        (
            "test summaries",
            "Tests pass: 120/120 and all required checks pass: `lint-and-build`",
        ),
        (
            "config templates",
            "\"apiKey\": \"{env:OPENAI_KEY}\", api_key:{llm_key}",
        ),
        (
            "rust paths",
            "ticket: Secret::new(raw), ApiKeyHasher::hash(key)",
        ),
        (
            "c arrows",
            "token = (s->bearer_token) ? session->bearer_token : NULL;",
        ),
        (
            "ssrf host:port",
            "userinfo trick http://localhost:80@169.254.169.254/",
        ),
        ("variable names in quotes", "password: \"DB_PASSWORD_VAR\""),
        ("literal marker", "API_KEY=[REDACTED] stays as it was"),
    ];
    for (name, input) in untouched {
        assert_eq!(redact_secrets(input), input, "leaves {name} alone");
    }
}

#[test]
fn redacts_long_values_completely() {
    let long = "Ab1".repeat(400);
    assert_eq!(
        redact_secrets(&format!("SESSION_TOKEN={long}")),
        format!("SESSION_TOKEN={R}")
    );
    assert_eq!(
        redact_secrets(&format!("{{\"auth_token\": \"{long}\"}}")),
        format!("{{\"auth_token\": \"{R}\"}}")
    );
}

#[test]
fn reports_each_hit() {
    let mut hits: Vec<String> = Vec::new();
    redact_secrets_with(
        "DB_PASSWORD=hunter and AKIAIOSFODNN7EXAMPLE",
        &mut |rule, _| hits.push(rule.to_string()),
    );
    hits.sort();
    assert_eq!(hits, vec!["assignment", "aws-access-key"]);
}

// ─── transcriptBelongsTo ─────────────────────────────────────────────────────

fn identity(p: &str) -> Option<String> {
    Some(p.to_string())
}

fn belongs(
    cwds: &[&str],
    folder: Option<&str>,
    platform: Platform,
    realpath: &dyn Fn(&str) -> Option<String>,
) -> bool {
    let cwds: Vec<String> = cwds.iter().map(|s| s.to_string()).collect();
    transcript_belongs_to(
        &cwds,
        folder,
        Some(&BelongsToOptions { platform, realpath }),
    )
}

#[test]
fn belongs_folder_and_inside() {
    assert!(belongs(&["/a/b"], Some("/a/b"), Platform::Linux, &identity));
    assert!(belongs(
        &["/x", "/a/b/src/deep"],
        Some("/a/b"),
        Platform::Linux,
        &identity
    ));
}

#[test]
fn belongs_respects_boundary() {
    assert!(!belongs(
        &["/a/bc"],
        Some("/a/b"),
        Platform::Linux,
        &identity
    ));
    assert!(!belongs(&["/a"], Some("/a/b"), Platform::Linux, &identity));
}

#[test]
fn belongs_normalizes() {
    assert!(belongs(
        &["/a/b/"],
        Some("/a/b//"),
        Platform::Linux,
        &identity
    ));
    assert!(belongs(
        &["/a/b/c/../d"],
        Some("/a/b/"),
        Platform::Linux,
        &identity
    ));
    assert!(belongs(
        &["/anything"],
        Some("/"),
        Platform::Linux,
        &identity
    ));
}

#[test]
fn belongs_case_insensitive_on_darwin_and_win32_only() {
    assert!(belongs(
        &["/Users/Me/Proj/src"],
        Some("/users/me/proj"),
        Platform::Darwin,
        &identity
    ));
    assert!(!belongs(
        &["/Users/Me/Proj"],
        Some("/users/me/proj"),
        Platform::Linux,
        &identity
    ));
    assert!(belongs(
        &["C:\\Work\\Repo\\src"],
        Some("c:/work/repo/"),
        Platform::Win32,
        &identity
    ));
    assert!(!belongs(
        &["C:\\Work\\Repo2"],
        Some("C:\\Work\\Repo"),
        Platform::Win32,
        &identity
    ));
}

#[test]
fn belongs_compares_realpaths() {
    let realpath = |p: &str| p.strip_prefix("/link").map(|rest| format!("/real{rest}"));
    assert!(belongs(
        &["/real/proj/src"],
        Some("/link/proj"),
        Platform::Linux,
        &realpath
    ));
    assert!(belongs(
        &["/link/proj"],
        Some("/real/proj"),
        Platform::Linux,
        &realpath
    ));
    assert!(belongs(
        &["/gone/proj"],
        Some("/gone/proj"),
        Platform::Linux,
        &realpath
    ));
}

#[test]
fn belongs_false_for_missing_folder_or_no_cwds() {
    let cwds = vec!["/a/b".to_string()];
    assert!(!transcript_belongs_to(&cwds, None, None));
    assert!(!transcript_belongs_to(&cwds, Some(""), None));
    assert!(!belongs(&[], Some("/a/b"), Platform::Linux, &identity));
}

#[test]
fn belongs_nfc_composes_decomposed_names() {
    // macOS can record a decomposed (NFD) name; the folder arrives composed.
    assert!(belongs(
        &["/work/cafe\u{301}/src"],
        Some("/work/caf\u{e9}"),
        Platform::Darwin,
        &identity
    ));
}

#[test]
fn posix_normalize_matches_node() {
    assert_eq!(posix_normalize("/a//b/./c/../d/"), "/a/b/d/");
    assert_eq!(
        posix_normalize("/work/demo/notes/plan.md"),
        "/work/demo/notes/plan.md"
    );
    assert_eq!(posix_normalize("a/../../b"), "../b");
    assert_eq!(posix_normalize("/.."), "/");
    assert_eq!(posix_normalize(""), ".");
}

// ─── files on disk ───────────────────────────────────────────────────────────

#[test]
fn discovers_main_and_subagent_transcripts() {
    reset();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let project = root.join("-work-demo");
    std::fs::create_dir_all(project.join(SID).join("subagents")).unwrap();
    std::fs::create_dir_all(project.join(SID).join("tool-results")).unwrap();
    std::fs::create_dir_all(project.join("memory")).unwrap();
    std::fs::write(
        project.join(format!("{SID}.jsonl")),
        jsonl(&[user(json!("hello there, main session")), say("hi")]),
    )
    .unwrap();
    std::fs::write(
        project.join(SID).join("subagents/agent-abc123.jsonl"),
        jsonl(&[user(json!("subagent task prompt here")), say("ok")]),
    )
    .unwrap();
    std::fs::write(
        project.join(SID).join("subagents/agent-abc123.meta.json"),
        r#"{"description":"Survey the tests"}"#,
    )
    .unwrap();
    std::fs::write(project.join(SID).join("tool-results/x.txt"), "noise").unwrap();
    std::fs::write(project.join("memory/MEMORY.md"), "noise").unwrap();
    std::fs::write(root.join("stray.jsonl"), "not inside a project").unwrap();

    let found = discover_transcript_files(root);
    let rel = |f: &TranscriptFile| {
        f.file
            .strip_prefix(&format!("{}/", root.display()))
            .unwrap()
            .to_string()
    };
    let got: Vec<(String, String, Option<String>)> = found
        .iter()
        .map(|f| (rel(f), f.id.clone(), f.parent_session_id.clone()))
        .collect();
    assert_eq!(
        got,
        vec![
            (format!("-work-demo/{SID}.jsonl"), SID.to_string(), None),
            (
                format!("-work-demo/{SID}/subagents/agent-abc123.jsonl"),
                "agent-abc123".to_string(),
                Some(SID.to_string())
            ),
        ]
    );
    assert!(found[0].size > 0);
    assert!(found[0].mtime_ms > 0.0);

    let sub = parse_transcript(&TranscriptSource::from(&found[1])).unwrap();
    assert_eq!(sub.meta.id, "agent-abc123");
    assert_eq!(sub.meta.parent_session_id.as_deref(), Some(SID));
    assert_eq!(sub.meta.title.as_deref(), Some("Survey the tests"));
    assert_eq!(sub.chunks[0].id, "agent-abc123:0");
    let by_path = parse_transcript(&TranscriptSource::from(found[1].file.as_str())).unwrap();
    assert_eq!(by_path.meta.parent_session_id.as_deref(), Some(SID));
}

#[test]
fn missing_projects_dir_yields_nothing() {
    let dir = tempfile::tempdir().unwrap();
    assert!(discover_transcript_files(&dir.path().join("nope")).is_empty());
}

// ─── performance ─────────────────────────────────────────────────────────────

#[test]
fn parses_a_5mb_transcript_quickly() {
    reset();
    let mut recs = Vec::new();
    for i in 0..400 {
        recs.push(user(json!(format!(
            "Task number {i}: refactor module {i} with care"
        ))));
        recs.push(think(""));
        recs.push(tool("Bash", json!({ "command": format!("npm test -- module{i}"), "description": format!("Test module {i}") })));
        recs.push(tool_result(&"PASS ".repeat(2000)));
        recs.push(say(&format!(
            "Refactored module {i}. {}",
            "detail ".repeat(150)
        )));
    }
    let raw = jsonl(&recs);
    assert!(raw.len() > 4_000_000);
    let start = std::time::Instant::now();
    let p = parse(&raw);
    let elapsed = start.elapsed();
    assert_eq!(p.chunks.len(), 400);
    // Generous bound: unoptimised test builds are much slower than release.
    assert!(elapsed.as_millis() < 5000, "took {elapsed:?}");
}

#[test]
fn value_cap_matches_the_js_4096_repeat() {
    // Bare: JS matches the first 4096 units and leaves the rest.
    let long = "Ab1".repeat(2000);
    assert_eq!(
        redact_secrets(&format!("SESSION_TOKEN={long}")),
        format!("SESSION_TOKEN={R}{}", &long[4096..])
    );
    // Quoted and over the cap: no match at all.
    let quoted = format!("token = \"{}\"", "Zq9".repeat(1500));
    assert_eq!(redact_secrets(&quoted), quoted);
}
