// Port of src/main/history/store.test.ts.

use super::*;

fn vecc(components: &[(usize, f32)]) -> Vec<f32> {
    let mut v = vec![0f32; HISTORY_EMBED_DIM];
    for &(i, x) in components {
        v[i] = x;
    }
    v
}

fn session(id: &str) -> SessionMeta {
    SessionMeta {
        id: id.into(),
        file: format!("/home/u/.claude/projects/p/{id}.jsonl"),
        cwds: vec!["/repo".into()],
        first_ts: 1000,
        last_ts: 2000,
        ..Default::default()
    }
}

fn chunk(sid: &str, seq: i64, text: &str) -> TranscriptChunk {
    TranscriptChunk {
        id: format!("{sid}:{seq}"),
        session_id: sid.into(),
        seq,
        ts: 1000 + seq,
        kind: ChunkKind::Turn,
        text: text.into(),
        files: vec![],
        hash: format!("h:{text}"),
    }
}

fn with_files(mut c: TranscriptChunk, files: &[&str]) -> TranscriptChunk {
    c.files = files.iter().map(|s| s.to_string()).collect();
    c
}
fn with_ts(mut c: TranscriptChunk, ts: i64) -> TranscriptChunk {
    c.ts = ts;
    c
}

fn mem() -> HistoryStore {
    HistoryStore::open(":memory:").unwrap()
}

fn seed(
    s: &HistoryStore,
    sid: &str,
    chunks: &[TranscriptChunk],
    meta: impl FnOnce(&mut SessionMeta),
) -> Vec<String> {
    let mut m = session(sid);
    meta(&mut m);
    s.upsert_session(&m).unwrap();
    s.replace_session_chunks(sid, chunks).unwrap()
}

fn cv(id: &str, v: Vec<f32>) -> ChunkVector {
    ChunkVector {
        id: id.into(),
        vector: v,
        hash: None,
    }
}

fn ids(hits: &[SearchHit]) -> Vec<String> {
    hits.iter().map(|h| h.chunk_id.clone()).collect()
}

fn lex(s: &HistoryStore, text: &str) -> Vec<String> {
    ids(&s
        .search(
            &SearchQuery {
                text: text.into(),
                ..Default::default()
            },
            None,
        )
        .unwrap())
}

fn dense(s: &HistoryStore, text: &str, v: Vec<f32>) -> Vec<String> {
    ids(&s
        .search(
            &SearchQuery {
                text: text.into(),
                vector: Some(v),
                ..Default::default()
            },
            Some(SearchMode::Dense),
        )
        .unwrap())
}

fn hybrid(s: &HistoryStore, text: &str, v: Vec<f32>) -> Vec<String> {
    ids(&s
        .search(
            &SearchQuery {
                text: text.into(),
                vector: Some(v),
                ..Default::default()
            },
            None,
        )
        .unwrap())
}

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}

// ─── replaceSessionChunks ────────────────────────────────────────────────────

#[test]
fn inserts_updates_deletes_and_keeps() {
    let s = mem();
    let first = seed(
        &s,
        "S",
        &[
            chunk("S", 0, "User: alpha one"),
            with_files(chunk("S", 1, "User: bravo two"), &["/repo/a.ts"]),
            chunk("S", 2, "User: charlie three"),
        ],
        |_| {},
    );
    assert_eq!(first, vec!["S:0", "S:1", "S:2"]);
    assert_eq!(
        s.pending_embeddings(10)
            .unwrap()
            .iter()
            .map(|p| p.id.as_str())
            .collect::<Vec<_>>(),
        vec!["S:0", "S:1", "S:2"]
    );
    assert_eq!(
        s.set_vectors(&[
            cv("S:0", vecc(&[(0, 1.0)])),
            cv("S:1", vecc(&[(1, 1.0)])),
            cv("S:2", vecc(&[(2, 1.0)]))
        ])
        .unwrap(),
        3
    );
    assert!(s.pending_embeddings(10).unwrap().is_empty());

    let second = s
        .replace_session_chunks(
            "S",
            &[
                chunk("S", 0, "User: alpha one"),
                with_files(chunk("S", 1, "User: bravo two, edited"), &["/repo/b.ts"]),
                chunk("S", 3, "User: delta four"),
            ],
        )
        .unwrap();
    assert_eq!(second, vec!["S:1", "S:3"]);
    assert!(s.get_chunk("S:0").unwrap().unwrap().embedded);
    let s1 = s.get_chunk("S:1").unwrap().unwrap();
    assert!(!s1.embedded);
    assert_eq!(s1.chunk.text, "User: bravo two, edited");
    assert_eq!(s1.chunk.files, vec!["/repo/b.ts"]);
    assert!(s.get_chunk("S:2").unwrap().is_none());
    assert!(!s.get_chunk("S:3").unwrap().unwrap().embedded);
    assert_eq!(
        sorted(
            s.pending_embeddings(10)
                .unwrap()
                .into_iter()
                .map(|p| p.id)
                .collect()
        ),
        vec!["S:1", "S:3"]
    );
    assert!(s.sessions_for_file("/repo/a.ts", 10).unwrap().is_empty());
    assert_eq!(
        s.sessions_for_file("/repo/b.ts", 10)
            .unwrap()
            .iter()
            .map(|e| e.session_id.as_str())
            .collect::<Vec<_>>(),
        vec!["S"]
    );
    let st = s.stats().unwrap();
    assert_eq!((st.sessions, st.chunks, st.embedded_chunks), (1, 3, 1));
}

#[test]
fn identical_rerun_is_noop() {
    let s = mem();
    let chunks = [chunk("S", 0, "one"), chunk("S", 1, "two")];
    seed(&s, "S", &chunks, |_| {});
    s.set_vectors(&[cv("S:0", vecc(&[(0, 1.0)]))]).unwrap();
    assert!(s.replace_session_chunks("S", &chunks).unwrap().is_empty());
    assert!(s.get_chunk("S:0").unwrap().unwrap().embedded);
}

#[test]
fn bad_chunk_rolls_back() {
    let s = mem();
    seed(&s, "S", &[chunk("S", 0, "kept text")], |_| {});
    let err = s
        .replace_session_chunks(
            "S",
            &[
                chunk("S", 1, "would be inserted"),
                chunk("OTHER", 2, "wrong session"),
            ],
        )
        .unwrap_err();
    assert!(err.to_string().contains("belongs to OTHER"));
    assert_eq!(
        s.get_session_chunks("S", None, None)
            .unwrap()
            .iter()
            .map(|c| c.chunk.id.as_str())
            .collect::<Vec<_>>(),
        vec!["S:0"]
    );
    assert!(lex(&s, "inserted").is_empty());
}

#[test]
fn drops_vector_for_changed_text() {
    let s = mem();
    seed(&s, "S", &[chunk("S", 0, "old text")], |_| {});
    let pending = s.pending_embeddings(1).unwrap().remove(0);
    s.replace_session_chunks("S", &[chunk("S", 0, "new text")])
        .unwrap();
    assert_eq!(
        s.set_vectors(&[ChunkVector {
            id: pending.id,
            hash: Some(pending.hash),
            vector: vecc(&[(0, 1.0)])
        }])
        .unwrap(),
        0
    );
    assert!(!s.get_chunk("S:0").unwrap().unwrap().embedded);
    assert!(
        s.set_vectors(&[cv("S:0", vec![0.0; 3])])
            .unwrap_err()
            .to_string()
            .contains("256-d")
    );
}

// ─── lexical / FTS ───────────────────────────────────────────────────────────

#[test]
fn fts_follows_writes() {
    let s = mem();
    seed(
        &s,
        "S",
        &[chunk("S", 0, "User: configure the zebrafish pipeline")],
        |_| {},
    );
    assert_eq!(lex(&s, "zebrafish"), vec!["S:0"]);
    s.replace_session_chunks(
        "S",
        &[chunk("S", 0, "User: configure the platypus pipeline")],
    )
    .unwrap();
    assert!(lex(&s, "zebrafish").is_empty());
    assert_eq!(lex(&s, "platypus"), vec!["S:0"]);
    s.replace_session_chunks("S", &[]).unwrap();
    assert!(lex(&s, "platypus").is_empty());
}

#[test]
fn substrings_case_insensitive_cyrillic() {
    let s = mem();
    seed(
        &s,
        "S",
        &[
            chunk(
                "S",
                0,
                "Assistant: Оновив конфіг на СЕРВЕРІ, FrobnicateWidget перейменовано",
            ),
            chunk("S", 1, "Assistant: nothing relevant here"),
        ],
        |_| {},
    );
    assert_eq!(lex(&s, "сервер"), vec!["S:0"]);
    assert_eq!(lex(&s, "frobnicatew"), vec!["S:0"]);
}

#[test]
fn user_text_never_becomes_fts_syntax() {
    let s = mem();
    seed(
        &s,
        "S",
        &[
            chunk("S", 0, "call NEAR(alpha beta) then foo* and -bar (x OR y)"),
            chunk("S", 1, "she said \"quoted words\" here"),
            chunk("S", 2, "привіт «світ» з лапками"),
        ],
        |_| {},
    );
    for q in [
        "NEAR(alpha beta)",
        "NEAR",
        "foo*",
        "*",
        "-bar",
        "- -",
        "(x OR y)",
        "AND OR NOT",
        "\"unterminated",
        "\"\"",
        "\"quoted words\"",
        "col:value",
        "^start",
        "a\"b\"c",
        "{alpha}",
        "привіт \"світ\"",
        "\u{0}nul\u{0}",
        "",
        "   ",
    ] {
        assert!(
            s.search(
                &SearchQuery {
                    text: q.into(),
                    ..Default::default()
                },
                None
            )
            .is_ok(),
            "{q}"
        );
    }
    assert_eq!(lex(&s, "NEAR(alpha"), vec!["S:0"]);
    assert_eq!(lex(&s, "-bar"), vec!["S:0"]);
    assert_eq!(lex(&s, "\"quoted words\""), vec!["S:1"]);
    assert_eq!(lex(&s, "привіт \"світ\""), vec!["S:2"]);

    assert_eq!(
        build_match_expression(&parse_query_terms("a\"b NEAR(x y)")).as_deref(),
        Some("\"near\"")
    );
    assert_eq!(
        build_match_expression(&parse_query_terms("\"say \"\"hi\"\" now\"")).as_deref(),
        Some("\"say\" OR \"now\"")
    );
    assert_eq!(
        build_match_expression(&parse_query_terms("\"it's\" here")).as_deref(),
        Some("\"it's\" OR \"here\"")
    );
}

#[test]
fn quoted_phrase_is_one_term() {
    let terms = parse_query_terms("find \"build failed\" in logs");
    assert!(terms.contains(&QueryTerm {
        text: "build failed".into(),
        identifier: true
    }));
    assert_eq!(
        build_match_expression(&terms).as_deref(),
        Some("\"find\" OR \"build failed\" OR \"logs\"")
    );
}

#[test]
fn short_terms() {
    let s = mem();
    seed(
        &s,
        "S",
        &[
            chunk("S", 0, "the UI is broken"),
            chunk("S", 1, "nothing here"),
        ],
        |_| {},
    );
    assert!(build_match_expression(&parse_query_terms("UI")).is_none());
    assert_eq!(lex(&s, "UI"), vec!["S:0"]);
    s.set_vectors(&[cv("S:0", vecc(&[(0, 1.0)])), cv("S:1", vecc(&[(1, 1.0)]))])
        .unwrap();
    assert_eq!(hybrid(&s, "UI", vecc(&[(1, 1.0)]))[0], "S:1");
    assert!(lex(&s, "i_").is_empty());
}

// ─── dense ───────────────────────────────────────────────────────────────────

#[test]
fn dense_ranks_by_cosine_and_cache_follows_writes() {
    let s = mem();
    seed(
        &s,
        "S",
        &[chunk("S", 0, "a"), chunk("S", 1, "b"), chunk("S", 2, "c")],
        |_| {},
    );
    s.set_vectors(&[
        cv("S:0", vecc(&[(0, 1.0)])),
        cv("S:1", vecc(&[(0, 1.0), (1, 1.0)])),
        cv("S:2", vecc(&[(1, 1.0)])),
    ])
    .unwrap();
    let q = vecc(&[(0, 0.2), (1, 1.0)]);
    let hits = s
        .search(
            &SearchQuery {
                text: "zzz".into(),
                vector: Some(q.clone()),
                ..Default::default()
            },
            Some(SearchMode::Dense),
        )
        .unwrap();
    assert_eq!(ids(&hits), vec!["S:2", "S:1", "S:0"]);
    assert!(hits[0].score > hits[1].score);

    s.replace_session_chunks(
        "S",
        &[
            chunk("S", 0, "a"),
            chunk("S", 1, "b"),
            chunk("S", 2, "c changed"),
        ],
    )
    .unwrap();
    assert_eq!(dense(&s, "zzz", q.clone()), vec!["S:1", "S:0"]);
    s.set_vectors(&[cv("S:2", vecc(&[(1, 1.0)]))]).unwrap();
    assert_eq!(dense(&s, "zzz", q.clone())[0], "S:2");
    s.replace_session_chunks("S", &[chunk("S", 0, "a"), chunk("S", 1, "b")])
        .unwrap();
    assert_eq!(dense(&s, "zzz", q), vec!["S:1", "S:0"]);
}

#[test]
fn dense_needs_vector() {
    let err = mem()
        .search(
            &SearchQuery {
                text: "x".into(),
                ..Default::default()
            },
            Some(SearchMode::Dense),
        )
        .unwrap_err();
    assert!(err.to_string().contains("needs a query vector"));
}

// ─── fusion ──────────────────────────────────────────────────────────────────

fn fusion_store() -> HistoryStore {
    let s = mem();
    seed(
        &s,
        "S",
        &[
            chunk("S", 0, "User: the parser crashes when the config has tabs"),
            chunk("S", 1, "User: parser rewrite discussion, config loading"),
            chunk("S", 2, "User: why is config parsing slow"),
            chunk(
                "S",
                3,
                "Assistant: fixed parseConfigTabs in src/config/loader.ts",
            ),
        ],
        |_| {},
    );
    s.set_vectors(&[
        cv("S:0", vecc(&[(0, 1.0), (1, 0.1)])),
        cv("S:1", vecc(&[(0, 1.0), (1, 0.3)])),
        cv("S:2", vecc(&[(0, 1.0), (1, 0.5)])),
        cv("S:3", vecc(&[(2, 1.0), (0, 0.2)])),
    ])
    .unwrap();
    s
}

#[test]
fn identifier_query_surfaces_exact_match() {
    let s = fusion_store();
    let q = vecc(&[(0, 1.0)]);
    assert_eq!(dense(&s, "parseConfigTabs", q.clone())[0], "S:0");
    assert_eq!(
        hybrid(&s, "where did we touch parseConfigTabs", q.clone())[0],
        "S:3"
    );
    assert_eq!(hybrid(&s, "changes to src/config/loader.ts", q)[0], "S:3");
}

#[test]
fn paraphrase_query_keeps_dense_order() {
    let s = fusion_store();
    let q = vecc(&[(0, 1.0)]);
    let text = "config parser problems";
    assert!(!parse_query_terms(text).iter().any(|t| t.identifier));
    assert_eq!(hybrid(&s, text, q.clone()), dense(&s, text, q));
}

#[test]
fn classifies_identifiers() {
    for t in [
        "FrobnicateWidget",
        "parse_config",
        "ABC-DEF-TEST",
        "502",
        "v1.2",
        "src/a.ts",
        "git.example.com",
        "HTTPServer",
        "sg-history",
    ] {
        assert!(is_identifier_like(t), "{t}");
    }
    for t in [
        "config",
        "Kubernetes",
        "DNS",
        "будь-який",
        "сервер",
        "Конфіг",
    ] {
        assert!(!is_identifier_like(t), "{t}");
    }
}

// ─── snippets ────────────────────────────────────────────────────────────────

#[test]
fn snippet_centres_on_match() {
    let filler = "lorem ipsum dolor sit amet ".repeat(40);
    let text = format!("User: intro line\n\n{filler}the needle_token is here\n{filler}");
    let sn = make_snippet(&text, &parse_query_terms("needle_token"));
    assert!(sn.contains("needle_token"));
    assert!(sn.starts_with('…'));
    assert!(sn.chars().count() <= 310);
    assert!(make_snippet(&text, &[]).starts_with("User: intro line"));
    assert_eq!(make_snippet("short", &[]), "short");
}

#[test]
fn hits_carry_snippet_and_title() {
    let s = mem();
    seed(&s, "S", &[chunk("S", 0, "User: hello world zebra")], |m| {
        m.title = Some("Fix the zebra".into())
    });
    let hit = s
        .search(
            &SearchQuery {
                text: "zebra".into(),
                ..Default::default()
            },
            None,
        )
        .unwrap()
        .remove(0);
    assert_eq!(hit.chunk_id, "S:0");
    assert_eq!(hit.session_id, "S");
    assert_eq!(hit.kind, ChunkKind::Turn);
    assert_eq!(hit.session_title.as_deref(), Some("Fix the zebra"));
    assert_eq!(hit.snippet, "User: hello world zebra");
}

// ─── sessions, files, since, delete, stats ───────────────────────────────────

#[test]
fn sessions_for_file_ordering_and_matching() {
    let s = mem();
    seed(
        &s,
        "old",
        &[with_files(
            with_ts(chunk("old", 0, "edit a"), 100),
            &["/repo/src/a.ts"],
        )],
        |m| {
            m.title = Some("Old".into());
            m.last_ts = 100;
        },
    );
    seed(
        &s,
        "new",
        &[
            with_files(
                with_ts(chunk("new", 0, "edit a again"), 500),
                &["/repo/src/a.ts", "/repo/src/b.ts"],
            ),
            with_files(
                with_ts(chunk("new", 1, "and again"), 600),
                &["/repo/src/a.ts"],
            ),
            with_ts(chunk("new", 2, "unrelated"), 700),
        ],
        |m| {
            m.title = Some("New".into());
            m.git_branch = Some("main".into());
            m.last_ts = 700;
        },
    );
    let sids = |v: Vec<FileHistoryEntry>| v.into_iter().map(|e| e.session_id).collect::<Vec<_>>();
    let res = s.sessions_for_file("/repo/src/a.ts", 10).unwrap();
    assert_eq!(sids(res.clone()), vec!["new", "old"]);
    assert_eq!(res[0].session_title.as_deref(), Some("New"));
    assert_eq!(res[0].git_branch.as_deref(), Some("main"));
    assert_eq!(res[0].last_ts, 600);
    assert_eq!(
        res[0]
            .chunks
            .iter()
            .map(|c| c.chunk_id.as_str())
            .collect::<Vec<_>>(),
        vec!["new:1", "new:0"]
    );
    assert_eq!(
        sids(s.sessions_for_file("src/a.ts", 10).unwrap()),
        vec!["new", "old"]
    );
    assert_eq!(
        sids(s.sessions_for_file("a.ts", 10).unwrap()),
        vec!["new", "old"]
    );
    assert_eq!(
        sids(s.sessions_for_file("/repo/src/a.ts", 1).unwrap()),
        vec!["new"]
    );
    assert!(
        s.sessions_for_file("/elsewhere/a.ts", 10)
            .unwrap()
            .is_empty()
    );
    assert!(s.sessions_for_file("%.ts", 10).unwrap().is_empty());
}

#[test]
fn since_filters_search_and_list() {
    let s = mem();
    seed(&s, "A", &[with_ts(chunk("A", 0, "kiwi early"), 100)], |m| {
        m.last_ts = 100
    });
    seed(&s, "B", &[with_ts(chunk("B", 0, "kiwi late"), 900)], |m| {
        m.last_ts = 900
    });
    s.set_vectors(&[
        cv("A:0", vecc(&[(0, 1.0)])),
        cv("B:0", vecc(&[(0, 0.5), (1, 1.0)])),
    ])
    .unwrap();
    assert_eq!(sorted(lex(&s, "kiwi")), vec!["A:0", "B:0"]);
    let q = |v: Option<Vec<f32>>| SearchQuery {
        text: "kiwi".into(),
        vector: v,
        since: Some(500),
        ..Default::default()
    };
    assert_eq!(ids(&s.search(&q(None), None).unwrap()), vec!["B:0"]);
    assert_eq!(
        ids(&s.search(&q(Some(vecc(&[(0, 1.0)]))), None).unwrap()),
        vec!["B:0"]
    );
    assert_eq!(
        s.list_sessions(10, None)
            .unwrap()
            .iter()
            .map(|x| x.meta.id.as_str())
            .collect::<Vec<_>>(),
        vec!["B", "A"]
    );
    assert_eq!(
        s.list_sessions(10, Some(500))
            .unwrap()
            .iter()
            .map(|x| x.meta.id.as_str())
            .collect::<Vec<_>>(),
        vec!["B"]
    );
    let first = s.list_sessions(10, None).unwrap().remove(0);
    assert_eq!(
        (
            first.meta.id.as_str(),
            first.chunk_count,
            first.meta.cwds.clone()
        ),
        ("B", 1, vec!["/repo".to_string()])
    );
}

#[test]
fn ties_go_to_newer_chunk() {
    let s = mem();
    let mut later = with_ts(chunk("S", 1, "same"), 200);
    later.hash = "other".into();
    seed(
        &s,
        "S",
        &[with_ts(chunk("S", 0, "same"), 100), later],
        |_| {},
    );
    s.set_vectors(&[cv("S:0", vecc(&[(0, 1.0)])), cv("S:1", vecc(&[(0, 1.0)]))])
        .unwrap();
    assert_eq!(hybrid(&s, "x", vecc(&[(0, 1.0)])), vec!["S:1", "S:0"]);
}

#[test]
fn session_chunk_window() {
    let s = mem();
    let chunks: Vec<TranscriptChunk> = (0..10)
        .map(|i| chunk("S", i, &format!("turn {i}")))
        .collect();
    seed(&s, "S", &chunks, |_| {});
    let seqs = |v: Vec<StoredChunk>| v.into_iter().map(|c| c.chunk.seq).collect::<Vec<_>>();
    assert_eq!(s.get_session_chunks("S", None, None).unwrap().len(), 10);
    assert_eq!(
        seqs(s.get_session_chunks("S", Some("S:5"), Some(1)).unwrap()),
        vec![4, 5, 6]
    );
    assert_eq!(
        seqs(s.get_session_chunks("S", Some("S:0"), None).unwrap()),
        vec![0, 1, 2]
    );
    assert!(
        s.get_session_chunks("S", Some("nope"), None)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn delete_session_removes_everything() {
    let s = mem();
    seed(
        &s,
        "A",
        &[with_files(chunk("A", 0, "mango keep"), &["/repo/x.ts"])],
        |_| {},
    );
    seed(
        &s,
        "B",
        &[with_files(chunk("B", 0, "mango drop"), &["/repo/x.ts"])],
        |_| {},
    );
    s.set_file_state(
        "/t/A.jsonl",
        &FileState {
            size: 1,
            mtime_ms: 1.0,
            session_id: Some("A".into()),
        },
    )
    .unwrap();
    s.set_file_state(
        "/t/B.jsonl",
        &FileState {
            size: 2,
            mtime_ms: 2.0,
            session_id: Some("B".into()),
        },
    )
    .unwrap();
    s.set_vectors(&[cv("A:0", vecc(&[(0, 1.0)])), cv("B:0", vecc(&[(0, 1.0)]))])
        .unwrap();
    assert_eq!(dense(&s, "x", vecc(&[(0, 1.0)])).len(), 2);
    s.delete_session("B").unwrap();
    assert_eq!(lex(&s, "mango"), vec!["A:0"]);
    assert_eq!(dense(&s, "x", vecc(&[(0, 1.0)])), vec!["A:0"]);
    assert_eq!(
        s.sessions_for_file("/repo/x.ts", 10)
            .unwrap()
            .iter()
            .map(|e| e.session_id.as_str())
            .collect::<Vec<_>>(),
        vec!["A"]
    );
    assert!(s.get_file_state("/t/B.jsonl").unwrap().is_none());
    assert_eq!(
        s.get_file_state("/t/A.jsonl").unwrap(),
        Some(FileState {
            size: 1,
            mtime_ms: 1.0,
            session_id: Some("A".into())
        })
    );
    let st = s.stats().unwrap();
    assert_eq!((st.sessions, st.chunks, st.embedded_chunks), (1, 1, 1));
    assert!(st.db_bytes > 0);
}

#[test]
fn file_state_round_trips() {
    let s = mem();
    assert!(s.get_file_state("/t/x.jsonl").unwrap().is_none());
    s.set_file_state(
        "/t/x.jsonl",
        &FileState {
            size: 10,
            mtime_ms: 123.7,
            session_id: None,
        },
    )
    .unwrap();
    assert_eq!(
        s.get_file_state("/t/x.jsonl").unwrap(),
        Some(FileState {
            size: 10,
            mtime_ms: 124.0,
            session_id: None
        })
    );
    s.set_file_state(
        "/t/x.jsonl",
        &FileState {
            size: 20,
            mtime_ms: 200.0,
            session_id: Some("S".into()),
        },
    )
    .unwrap();
    assert_eq!(
        s.get_file_state("/t/x.jsonl").unwrap(),
        Some(FileState {
            size: 20,
            mtime_ms: 200.0,
            session_id: Some("S".into())
        })
    );
}

// ─── file-backed DB ──────────────────────────────────────────────────────────

#[test]
fn persists_across_reopen_with_wal_and_0600() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("ws.sqlite");
    let db = db.to_str().unwrap();
    let a = HistoryStore::open(db).unwrap();
    seed(
        &a,
        "S",
        &[with_files(
            chunk("S", 0, "persistent walrus"),
            &["/repo/w.ts"],
        )],
        |m| m.title = Some("Walrus".into()),
    );
    a.set_vectors(&[cv("S:0", vecc(&[(3, 1.0)]))]).unwrap();
    a.set_file_state(
        "/t/S.jsonl",
        &FileState {
            size: 5,
            mtime_ms: 6.0,
            session_id: Some("S".into()),
        },
    )
    .unwrap();
    a.close();
    a.close();
    assert!(a.stats().unwrap_err().to_string().contains("closed"));
    assert_eq!(
        std::fs::metadata(db).unwrap().permissions().mode() & 0o777,
        0o600
    );

    let b = HistoryStore::open(db).unwrap();
    let mode: String = rusqlite::Connection::open(db)
        .unwrap()
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
    assert_eq!(
        b.get_file_state("/t/S.jsonl")
            .unwrap()
            .unwrap()
            .session_id
            .as_deref(),
        Some("S")
    );
    let hit = b
        .search(
            &SearchQuery {
                text: "walrus".into(),
                ..Default::default()
            },
            None,
        )
        .unwrap()
        .remove(0);
    assert_eq!(
        (hit.chunk_id.as_str(), hit.session_title.as_deref()),
        ("S:0", Some("Walrus"))
    );
    assert_eq!(hybrid(&b, "zzz", vecc(&[(3, 1.0)])), vec!["S:0"]);
    assert_eq!(b.sessions_for_file("/repo/w.ts", 10).unwrap().len(), 1);
    let st = b.stats().unwrap();
    assert_eq!((st.sessions, st.chunks, st.embedded_chunks), (1, 1, 1));
    assert!(st.db_bytes > 0);
}

#[test]
fn refuses_a_newer_schema_without_writing() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("ws.sqlite");
    let db = db.to_str().unwrap();
    drop(HistoryStore::open(db).unwrap());
    {
        let c = rusqlite::Connection::open(db).unwrap();
        c.execute(
            "UPDATE meta SET value = '3' WHERE key = 'schema_version'",
            [],
        )
        .unwrap();
        c.execute("CREATE TABLE future_only (x)", []).unwrap();
    }
    let err = HistoryStore::open(db).err().unwrap();
    assert!(err.to_string().contains("newer than supported"));
    let v: String = rusqlite::Connection::open(db)
        .unwrap()
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(v, "3");
}

#[test]
fn vector_blob_is_little_endian_f32() {
    let s = mem();
    seed(&s, "S", &[chunk("S", 0, "v")], |_| {});
    s.set_vectors(&[cv("S:0", vecc(&[(0, 3.0), (1, 4.0)]))])
        .unwrap();
    let inner = s.lock();
    let blob: Vec<u8> = inner
        .conn()
        .unwrap()
        .query_row("SELECT vec FROM chunks", [], |r| r.get(0))
        .unwrap();
    assert_eq!(blob.len(), HISTORY_EMBED_DIM * 4);
    assert_eq!(&blob[0..4], &0.6f32.to_le_bytes());
    assert_eq!(&blob[4..8], &0.8f32.to_le_bytes());
}

// ─── decision notes ──────────────────────────────────────────────────────────

fn note(sid: &str, group: i64, i: usize, text: &str) -> NoteInput {
    NoteInput {
        chunk: TranscriptChunk {
            id: format!("{sid}:n{group}.{i}"),
            session_id: sid.into(),
            seq: 1,
            ts: 1500,
            kind: ChunkKind::Decision,
            text: text.into(),
            files: vec![],
            hash: format!("n:{text}"),
        },
        note_kind: DecisionNoteKind::Decision,
        sources: vec![format!("{sid}:1")],
    }
}

fn run(input_hash: &str) -> DistillRunRecord {
    DistillRunRecord {
        input_hash: input_hash.into(),
        ok: true,
        cost_usd: 0.001,
        input_tokens: 1000.0,
        output_tokens: 100.0,
        at: 5000,
        ..Default::default()
    }
}

#[test]
fn notes_survive_replace_session_chunks() {
    let s = mem();
    seed(
        &s,
        "a",
        &[chunk("a", 0, "first turn"), chunk("a", 1, "second turn")],
        |_| {},
    );
    s.replace_group_notes("a", 0, &[note("a", 0, 0, "Decision: keep notes")], None)
        .unwrap();
    assert_eq!(
        s.replace_session_chunks("a", &[chunk("a", 0, "first turn edited")])
            .unwrap(),
        vec!["a:0"]
    );
    assert!(s.get_chunk("a:1").unwrap().is_none());
    let n = s.get_chunk("a:n0.0").unwrap().unwrap();
    assert_eq!(n.chunk.kind, ChunkKind::Decision);
    assert_eq!(n.chunk.text, "Decision: keep notes");
    assert_eq!(
        n.note,
        Some(NoteRef {
            kind: DecisionNoteKind::Decision,
            sources: vec!["a:1".into()]
        })
    );
    let mut bad = chunk("a", 2, "x");
    bad.kind = ChunkKind::Decision;
    assert!(
        s.replace_session_chunks("a", &[bad])
            .unwrap_err()
            .to_string()
            .contains("replace_group_notes")
    );
}

#[test]
fn replace_group_notes_one_group_atomically() {
    let s = mem();
    seed(&s, "a", &[chunk("a", 0, "turn")], |_| {});
    s.replace_group_notes(
        "a",
        0,
        &[note("a", 0, 0, "g0 old"), note("a", 0, 1, "g0 old two")],
        Some(&run("h0")),
    )
    .unwrap();
    s.replace_group_notes("a", 1, &[note("a", 1, 0, "g1 note")], Some(&run("h1")))
        .unwrap();
    s.replace_group_notes(
        "a",
        0,
        &[note("a", 0, 0, "g0 new")],
        Some(&DistillRunRecord {
            cost_usd: 0.002,
            ..run("h0b")
        }),
    )
    .unwrap();
    let texts = |g| {
        s.get_group_notes("a", g)
            .unwrap()
            .into_iter()
            .map(|n| n.chunk.text)
            .collect::<Vec<_>>()
    };
    assert_eq!(texts(0), vec!["g0 new"]);
    assert_eq!(texts(1), vec!["g1 note"]);
    assert!(s.get_chunk("a:n0.1").unwrap().is_none());
    assert_eq!(s.get_chunk("a:0").unwrap().unwrap().chunk.text, "turn");
    let st = s.group_states("a").unwrap()[&0].clone();
    assert_eq!(
        (st.input_hash.as_str(), st.ok, st.notes, st.attempts),
        ("h0b", true, 1, 0)
    );
    assert!((st.cost_usd - 0.003).abs() < 1e-9);
    let (notes, spent) = s.distill_totals().unwrap();
    assert_eq!(notes, 2);
    assert!((spent - 0.004).abs() < 1e-6);
    s.replace_group_notes(
        "a",
        1,
        &[],
        Some(&DistillRunRecord {
            cost_usd: 0.0,
            ..run("h1b")
        }),
    )
    .unwrap();
    assert!(s.get_group_notes("a", 1).unwrap().is_empty());
    let st1 = s.group_states("a").unwrap()[&1].clone();
    assert_eq!(
        (st1.input_hash.as_str(), st1.ok, st1.notes),
        ("h1b", true, 0)
    );
}

#[test]
fn failed_run_keeps_notes_and_counts_attempts() {
    let s = mem();
    seed(&s, "a", &[chunk("a", 0, "turn")], |_| {});
    s.replace_group_notes("a", 0, &[note("a", 0, 0, "kept")], Some(&run("h0")))
        .unwrap();
    let fail = |counts: bool, err: &str| DistillRunRecord {
        ok: false,
        counts_as_attempt: counts,
        error: Some(err.into()),
        ..run("h1")
    };
    s.record_group_run("a", 0, &fail(true, "bad json")).unwrap();
    s.record_group_run("a", 0, &fail(false, "limit")).unwrap();
    s.record_group_run("a", 0, &fail(true, "bad json")).unwrap();
    let st = s.group_states("a").unwrap()[&0].clone();
    assert_eq!(
        (
            st.input_hash.as_str(),
            st.ok,
            st.attempts,
            st.notes,
            st.error.as_deref()
        ),
        ("h1", false, 2, 1, Some("bad json"))
    );
    assert_eq!(s.get_group_notes("a", 0).unwrap().len(), 1);
    s.record_group_run(
        "a",
        0,
        &DistillRunRecord {
            ok: false,
            counts_as_attempt: true,
            ..run("h2")
        },
    )
    .unwrap();
    assert_eq!(s.group_states("a").unwrap()[&0].attempts, 1);
}

#[test]
fn notes_are_searched_and_shown_but_not_turns() {
    let s = mem();
    seed(
        &s,
        "a",
        &[
            chunk("a", 0, "we talked about sockets"),
            chunk("a", 1, "and then about retries"),
            chunk("a", 2, "done"),
        ],
        |_| {},
    );
    let mut n = note("a", 0, 0, "Rejected: unbounded reconnect backoff");
    n.note_kind = DecisionNoteKind::Rejected;
    n.sources = vec!["a:0".into(), "a:1".into()];
    s.replace_group_notes("a", 0, &[n], None).unwrap();
    assert!(
        s.pending_embeddings(10)
            .unwrap()
            .iter()
            .any(|p| p.id == "a:n0.0")
    );
    let hits = s
        .search(
            &SearchQuery {
                text: "reconnect backoff".into(),
                ..Default::default()
            },
            None,
        )
        .unwrap();
    assert_eq!(hits[0].chunk_id, "a:n0.0");
    assert_eq!(hits[0].kind, ChunkKind::Decision);
    assert_eq!(
        hits[0].note,
        Some(NoteRef {
            kind: DecisionNoteKind::Rejected,
            sources: vec!["a:0".into(), "a:1".into()]
        })
    );
    assert!(
        s.search(
            &SearchQuery {
                text: "sockets".into(),
                ..Default::default()
            },
            None
        )
        .unwrap()[0]
            .note
            .is_none()
    );
    let window = |anchor: Option<&str>| {
        s.get_session_chunks("a", anchor, Some(5))
            .unwrap()
            .into_iter()
            .map(|c| c.chunk.id)
            .collect::<Vec<_>>()
    };
    assert_eq!(window(Some("a:1")), vec!["a:0", "a:1", "a:2"]);
    assert_eq!(window(Some("a:n0.0")), vec!["a:n0.0"]);
    assert_eq!(window(None), vec!["a:0", "a:1", "a:2"]);
    assert_eq!(s.list_sessions(10, None).unwrap()[0].chunk_count, 3);
    assert!(s.sessions_for_file("/x.ts", 10).unwrap().is_empty());
}

#[test]
fn dense_sees_embedded_note() {
    let s = mem();
    seed(&s, "a", &[chunk("a", 0, "turn")], |_| {});
    s.replace_group_notes("a", 0, &[note("a", 0, 0, "Decision: x")], None)
        .unwrap();
    s.set_vectors(&[
        cv("a:0", vecc(&[(0, 1.0)])),
        cv("a:n0.0", vecc(&[(1, 1.0)])),
    ])
    .unwrap();
    let q = |limit| SearchQuery {
        text: "q".into(),
        vector: Some(vecc(&[(1, 1.0)])),
        limit: Some(limit),
        ..Default::default()
    };
    assert_eq!(
        s.search(&q(1), Some(SearchMode::Dense)).unwrap()[0].chunk_id,
        "a:n0.0"
    );
    s.replace_group_notes("a", 0, &[], None).unwrap();
    assert_eq!(
        ids(&s.search(&q(2), Some(SearchMode::Dense)).unwrap()),
        vec!["a:0"]
    );
}

#[test]
fn delete_notes_and_prune_groups() {
    let s = mem();
    seed(&s, "a", &[chunk("a", 0, "turn")], |_| {});
    s.replace_group_notes("a", 0, &[note("a", 0, 0, "one")], Some(&run("h0")))
        .unwrap();
    s.replace_group_notes("a", 1, &[note("a", 1, 0, "two")], Some(&run("h1")))
        .unwrap();
    s.prune_groups("a", 1).unwrap();
    assert!(s.get_group_notes("a", 1).unwrap().is_empty());
    assert_eq!(
        s.group_states("a").unwrap().into_keys().collect::<Vec<_>>(),
        vec![0]
    );
    assert_eq!(s.delete_notes().unwrap(), 1);
    assert!(s.group_states("a").unwrap().is_empty());
    let (notes, spent) = s.distill_totals().unwrap();
    assert_eq!(notes, 0);
    assert!((spent - 0.002).abs() < 1e-6);
    assert!(s.get_chunk("a:0").unwrap().is_some());
}

#[test]
fn distill_candidates_main_sessions_only() {
    let s = mem();
    let mut summary = chunk("main", 2, "summary");
    summary.kind = ChunkKind::CompactSummary;
    seed(
        &s,
        "main",
        &[chunk("main", 1, "bb"), chunk("main", 0, "a"), summary],
        |m| {
            m.title = Some("T".into());
            m.last_ts = 9;
        },
    );
    seed(&s, "agent-1", &[chunk("agent-1", 0, "sub")], |m| {
        m.parent_session_id = Some("main".into())
    });
    s.replace_group_notes("main", 0, &[note("main", 0, 0, "n")], None)
        .unwrap();
    assert_eq!(
        s.distill_candidates().unwrap(),
        vec![DistillCandidate {
            session_id: "main".into(),
            title: Some("T".into()),
            last_ts: 9,
            chunks: vec![
                CandidateChunk {
                    id: "main:0".into(),
                    seq: 0,
                    hash: "h:a".into(),
                    chars: 1,
                    ts: 1000
                },
                CandidateChunk {
                    id: "main:1".into(),
                    seq: 1,
                    hash: "h:bb".into(),
                    chars: 2,
                    ts: 1001
                },
            ],
        }]
    );
}
