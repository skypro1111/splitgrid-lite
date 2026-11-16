//! End to end through the public API with the REAL runtime (download, verify,
//! llama-server, embeddings): enable → index synthetic transcripts → search.
//! `SG_RUNTIME_LIVE_DIR=<dir> cargo test -p sg-history --test live_service -- --ignored`
//! (reuses a runtime already installed there).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;
use sg_history::{HistoryConfig, HistoryService, IndexedWorkspace, ProgressState};

fn line(kind: &str, ts: &str, cwd: &str, message: serde_json::Value, uuid: &str) -> String {
    json!({ "type": kind, "timestamp": ts, "cwd": cwd, "sessionId": "s-live", "uuid": uuid, "message": message }).to_string()
}

#[test]
#[ignore]
fn enable_index_and_search() {
    let root = PathBuf::from(std::env::var_os("SG_RUNTIME_LIVE_DIR").expect("set SG_RUNTIME_LIVE_DIR"));
    let work = tempfile::tempdir().unwrap();
    let folder = work.path().join("proj");
    std::fs::create_dir_all(&folder).unwrap();
    let projects = work.path().join("projects");
    let cwd = folder.to_string_lossy().into_owned();
    // The estimate (stat only) finds a workspace's files by Claude's folder slug.
    let slug = projects.join(sg_history::claude_project_slug(&cwd));
    std::fs::create_dir_all(&slug).unwrap();
    let turns = [
        ("We picked SQLite over Postgres for the cache because it ships in the binary and needs no server.", "Agreed: SQLite, embedded, no daemon."),
        ("The login page flickers on Safari when the font loads late.", "Added font-display: optional to stop the layout shift."),
        ("Retry the webhook three times with exponential backoff.", "Implemented backoff of 1s, 4s, 16s in webhook.rs."),
    ];
    let mut lines = Vec::new();
    for (i, (u, a)) in turns.iter().enumerate() {
        let ts = format!("2026-11-0{}T10:00:00.000Z", i + 1);
        lines.push(line("user", &ts, &cwd, json!({ "role": "user", "content": u }), &format!("u{i}")));
        lines.push(line("assistant", &ts, &cwd, json!({ "role": "assistant", "model": "claude-test", "content": [{ "type": "text", "text": a }] }), &format!("a{i}")));
    }
    std::fs::write(slug.join("s-live.jsonl"), lines.join("\n") + "\n").unwrap();

    let ws = IndexedWorkspace { id: "ws-live".into(), working_directory: Some(folder.clone()) };
    let svc = HistoryService::new(HistoryConfig {
        root_dir: root,
        projects_dir: projects,
        workspaces: Arc::new(move || vec![ws.clone()]),
        claude_binary: Arc::new(|| None),
        log: Arc::new(|m: &str| eprintln!("{m}")),
    });
    let est = svc.estimate().unwrap();
    assert_eq!(est.transcript_files, 1);
    svc.enable().unwrap();
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let s = svc.status();
        let embedded = s.workspaces.iter().map(|w| w.embedded_chunks).sum::<u64>();
        if s.progress.state == ProgressState::Idle && embedded >= 3 {
            break;
        }
        assert!(Instant::now() < deadline, "not indexed: {s:?}");
        std::thread::sleep(Duration::from_millis(500));
    }
    // Semantic: no shared words with the turn it should find.
    let hits = svc.search("ws-live", "which embedded database did we choose and why", Some(3), None).unwrap();
    eprintln!("{hits:#?}");
    assert!(hits[0].snippet.contains("SQLite"), "{hits:?}");
    let hits = svc.search("ws-live", "safari flicker", Some(3), None).unwrap();
    assert!(hits[0].snippet.contains("Safari"));
    assert!(svc.search("other-ws", "SQLite", None, None).unwrap().is_empty());
    assert_eq!(svc.list_sessions("ws-live", None).unwrap().len(), 1);
    svc.dispose();
}
