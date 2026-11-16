// Port of src/main/history/runtime.test.ts (+ an #[ignore]d live test in
// place of runtime.live.test.ts). Nothing here downloads the real model or
// binary: downloads are exercised against a loopback server.

use super::*;
use std::io::{BufRead, BufReader};
use std::net::TcpStream;
use std::sync::atomic::{AtomicUsize, Ordering};

fn sha(b: &[u8]) -> String {
    hex::encode(Sha256::digest(b))
}

// ─── manifest + platform selection ───────────────────────────────────────────

#[test]
fn maps_platforms_to_pinned_assets() {
    assert!(
        select_llama_asset("darwin", "arm64")
            .unwrap()
            .url
            .contains("llama-b11960-bin-macos-arm64.tar.gz")
    );
    assert!(
        select_llama_asset("darwin", "x64")
            .unwrap()
            .url
            .contains("bin-macos-x64.tar.gz")
    );
    assert!(
        select_llama_asset("linux", "x64")
            .unwrap()
            .url
            .contains("bin-ubuntu-x64.tar.gz")
    );
    assert!(
        select_llama_asset("win32", "x64")
            .unwrap()
            .url
            .contains("bin-win-cpu-x64.zip")
    );
    assert!(LLAMA_ASSETS.iter().all(|a| a.url.contains(LLAMA_CPP_TAG)));
}

#[test]
fn unsupported_combinations_are_none() {
    assert!(select_llama_asset("win32", "arm64").is_none());
    assert!(select_llama_asset("linux", "arm64").is_none());
    assert!(select_llama_asset("freebsd", "x64").is_none());
    assert!(select_llama_asset("constructor", "").is_none());
}

#[test]
fn pins_every_asset() {
    let all: Vec<(&str, &str, u64)> = LLAMA_ASSETS
        .iter()
        .map(|a| (a.url, a.sha256, a.size))
        .chain([(MODEL_ASSET.url, MODEL_ASSET.sha256, MODEL_ASSET.size)])
        .collect();
    for (url, sha256, size) in all {
        assert!(url.starts_with("https://"));
        assert!(
            sha256.len() == 64
                && sha256
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        );
        assert!(size > 1_000_000);
    }
    assert!(
        MODEL_ASSET
            .url
            .contains(&format!("/resolve/{}/", MODEL_ASSET.revision))
    );
    assert!(
        MODEL_ASSET.revision.len() == 40
            && MODEL_ASSET.revision.bytes().all(|b| b.is_ascii_hexdigit())
    );
    assert!(MODEL_ASSET.url.ends_with(MODEL_ASSET.file));
}

#[test]
fn keeps_server_libs_and_drops_tools() {
    let re = |a: &LlamaAsset, n: &str| Regex::new(a.lib_pattern).unwrap().is_match(n);
    let mac = select_llama_asset("darwin", "arm64").unwrap();
    assert!(re(mac, "libllama.0.dylib"));
    assert!(!re(mac, "llama-cli"));
    let linux = select_llama_asset("linux", "x64").unwrap();
    assert!(re(linux, "libllama.so.0.8.0"));
    assert!(re(linux, "libggml-cpu-haswell.so"));
    assert!(!re(linux, "llama-connect"));
    let win = select_llama_asset("win32", "x64").unwrap();
    assert!(re(win, "libomp.dll"));
    assert!(!re(win, "llama-connect.exe"));
    for a in &LLAMA_ASSETS {
        for lib in a.required_libs {
            assert!(is_kept_file(a, lib), "{lib}");
        }
        assert!(is_kept_file(a, basename(a.server_path)));
    }
    assert!(!is_kept_file(mac, "libllama-cli-impl.dylib"));
    assert!(is_kept_file(mac, "libllama-server-impl.dylib"));
    assert!(!is_kept_file(linux, "libllama-bench-impl.so"));
    assert!(!is_kept_file(win, "llama-cli-impl.dll"));
    assert!(is_kept_file(win, "llama-server.exe"));
    assert!(!is_kept_file(win, "llama.exe"));
}

#[test]
fn extracts_with_system_tool() {
    assert_eq!(
        extract_command(Archive::TarGz, Path::new("/a.tgz"), Path::new("/d")),
        (
            "tar".to_string(),
            vec![
                "-xzf".to_string(),
                "/a.tgz".into(),
                "-C".into(),
                "/d".into()
            ]
        )
    );
    assert_eq!(
        extract_command(Archive::Zip, Path::new("/a.zip"), Path::new("/d")).0,
        "unzip"
    );
}

#[test]
fn builds_server_command_line() {
    let args = server_args("/m.gguf", 4321, 3);
    let flag = |f: &str| args[args.iter().position(|a| a == f).unwrap() + 1].clone();
    assert_eq!(flag("-m"), "/m.gguf");
    assert!(args.iter().any(|a| a == "--embedding"));
    assert_eq!(flag("--pooling"), "mean");
    assert_eq!(flag("-c"), "8192");
    assert_eq!(flag("-ub"), "8192");
    assert_eq!(flag("--host"), "127.0.0.1");
    assert_eq!(flag("--port"), "4321");
    assert_eq!(flag("-np"), "1");
    assert_eq!(flag("-t"), "3");
    assert!(args.iter().any(|a| a == "--offline"));
}

// ─── input/output shaping ────────────────────────────────────────────────────

#[test]
fn prefixes() {
    assert_eq!(
        with_prefix("hello", EmbedKind::Document),
        format!("{DOCUMENT_PREFIX}hello")
    );
    assert_eq!(
        with_prefix("hello", EmbedKind::Query),
        format!("{QUERY_PREFIX}hello")
    );
    assert_eq!(DOCUMENT_PREFIX, "title: none | text: ");
    assert_eq!(QUERY_PREFIX, "task: search result | query: ");
}

#[test]
fn clamps_without_splitting_surrogates() {
    assert_eq!(clamp_input("short", MAX_INPUT_CHARS), "short");
    assert_eq!(
        clamp_input(&"x".repeat(MAX_INPUT_CHARS + 50), MAX_INPUT_CHARS).len(),
        MAX_INPUT_CHARS
    );
    let s = "ab😀cd";
    assert_eq!(clamp_input(s, 3), "ab");
    assert_eq!(clamp_input(s, 4), "ab😀");
}

#[test]
fn truncates_and_renormalises() {
    let v: Vec<f64> = (0..768).map(|i| ((i + 1) as f64).sin()).collect();
    let out = truncate_normalize(&v, HISTORY_EMBED_DIM).unwrap();
    assert_eq!(out.len(), HISTORY_EMBED_DIM);
    let norm: f64 = out
        .iter()
        .map(|&x| f64::from(x) * f64::from(x))
        .sum::<f64>()
        .sqrt();
    assert!((norm - 1.0).abs() < 1e-5);
    assert!((f64::from(out[1]) / f64::from(out[0]) - v[1] / v[0]).abs() < 1e-4);
}

#[test]
fn zero_vector_and_short_vector() {
    assert_eq!(truncate_normalize(&[0.0; 300], 4).unwrap(), vec![0.0; 4]);
    assert!(
        truncate_normalize(&[1.0, 2.0, 3.0], HISTORY_EMBED_DIM)
            .unwrap_err()
            .to_string()
            .contains("3 dims")
    );
}

#[test]
fn batches_by_count_and_chars() {
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    assert!(make_batches(&[], 32, 48_000).is_empty());
    assert_eq!(
        make_batches(&s(&["a", "b", "c", "d", "e"]), 2, 48_000)
            .iter()
            .map(Vec::len)
            .collect::<Vec<_>>(),
        vec![2, 2, 1]
    );
    let big = "x".repeat(30);
    assert_eq!(
        make_batches(&[big.clone(), big.clone(), big], 10, 50)
            .iter()
            .map(Vec::len)
            .collect::<Vec<_>>(),
        vec![1, 1, 1]
    );
    assert_eq!(
        make_batches(&["x".repeat(100), "y".into()], 10, 50),
        vec![vec!["x".repeat(100)], vec!["y".to_string()]]
    );
}

// ─── IdleTimer (real time, short periods) ────────────────────────────────────

fn counter() -> (Arc<AtomicUsize>, Arc<dyn Fn() + Send + Sync>) {
    let n = Arc::new(AtomicUsize::new(0));
    let m = n.clone();
    (
        n,
        Arc::new(move || {
            m.fetch_add(1, Ordering::SeqCst);
        }),
    )
}

#[test]
fn idle_fires_once_after_period() {
    let (n, f) = counter();
    let t = IdleTimer::new(Duration::from_millis(150), f);
    t.busy();
    t.release();
    assert!(t.armed());
    std::thread::sleep(Duration::from_millis(60));
    assert_eq!(n.load(Ordering::SeqCst), 0);
    std::thread::sleep(Duration::from_millis(250));
    assert_eq!(n.load(Ordering::SeqCst), 1);
    assert!(!t.armed());
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(n.load(Ordering::SeqCst), 1);
}

#[test]
fn idle_never_fires_in_flight_and_restarts_on_work() {
    let (n, f) = counter();
    let t = IdleTimer::new(Duration::from_millis(150), f);
    t.busy();
    t.busy();
    t.release();
    assert!(!t.armed());
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(n.load(Ordering::SeqCst), 0);
    t.release();
    std::thread::sleep(Duration::from_millis(80));
    t.busy();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(n.load(Ordering::SeqCst), 0);
    t.release();
    std::thread::sleep(Duration::from_millis(350));
    assert_eq!(n.load(Ordering::SeqCst), 1);
}

#[test]
fn idle_cancel_disarms() {
    let (n, f) = counter();
    let t = IdleTimer::new(Duration::from_millis(100), f);
    t.busy();
    t.release();
    t.cancel();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(n.load(Ordering::SeqCst), 0);
}

// ─── downloadVerified (loopback server) ──────────────────────────────────────

fn payload() -> Vec<u8> {
    b"splitgrid embedding runtime test payload\n".repeat(1000)
}

fn serve(mut stream: TcpStream) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return;
    }
    let path = line.split(' ').nth(1).unwrap_or("/").to_string();
    let mut range_from: Option<usize> = None;
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h).is_err() || h == "\r\n" || h.is_empty() {
            break;
        }
        if let Some(v) = h.to_ascii_lowercase().strip_prefix("range: bytes=") {
            range_from = v.trim().trim_end_matches('-').parse().ok();
        }
    }
    let p = payload();
    let head = |status: &str, extra: &str, len: usize| {
        format!("HTTP/1.1 {status}\r\n{extra}Content-Length: {len}\r\nConnection: close\r\n\r\n")
    };
    match path.as_str() {
        "/file" => {
            let _ = stream.write_all(head("200 OK", "", p.len()).as_bytes());
            let _ = stream.write_all(&p);
        }
        // Drops the first connection halfway; answers a Range with the rest.
        "/flaky" => match range_from {
            None => {
                let _ = stream.write_all(head("200 OK", "", p.len()).as_bytes());
                let _ = stream.write_all(&p[..p.len() / 2]);
                let _ = stream.flush();
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
            Some(from) => {
                let range = format!("Content-Range: bytes {from}-{}/{}\r\n", p.len() - 1, p.len());
                let _ = stream.write_all(head("206 Partial Content", &range, p.len() - from).as_bytes());
                let _ = stream.write_all(&p[from..]);
            }
        },
        "/redirect" => {
            let _ = stream.write_all(head("302 Found", "Location: /file\r\n", 0).as_bytes());
        }
        "/loop" => {
            let _ = stream.write_all(head("302 Found", "Location: /loop\r\n", 0).as_bytes());
        }
        "/offsite" => {
            let _ = stream.write_all(
                head("302 Found", "Location: http://example.com/file\r\n", 0).as_bytes(),
            );
        }
        "/slow" => {
            let _ = stream.write_all(head("200 OK", "", p.len()).as_bytes());
            let _ = stream.write_all(&p[..100]);
            let _ = stream.flush();
            std::thread::sleep(Duration::from_secs(5));
        }
        _ => {
            let _ = stream.write_all(head("404 Not Found", "", 0).as_bytes());
        }
    }
}

fn server() -> String {
    static BASE: LazyLock<String> = LazyLock::new(|| {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://127.0.0.1:{}", l.local_addr().unwrap().port());
        std::thread::spawn(move || {
            for s in l.incoming().flatten() {
                std::thread::spawn(move || serve(s));
            }
        });
        base
    });
    BASE.clone()
}

#[test]
fn downloads_through_redirect_and_lands_verified_file() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("ok/file.bin");
    let mut bytes = 0u64;
    download_verified(
        &format!("{}/redirect", server()),
        &dest,
        &sha(&payload()),
        &CancelToken::new(),
        &mut |n| bytes += n,
    )
    .unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), payload());
    assert_eq!(bytes, payload().len() as u64);
    assert!(!dest.with_file_name("file.bin.part").exists());
    assert_eq!(sha256_file(&dest).unwrap(), sha(&payload()));
}

#[test]
fn a_dropped_download_resumes_where_it_stopped() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("flaky/file.bin");
    let mut bytes = 0u64;
    download_verified(&format!("{}/flaky", server()), &dest, &sha(&payload()), &CancelToken::new(), &mut |n| bytes += n).unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), payload());
    assert_eq!(bytes, payload().len() as u64);
    assert!(!dest.with_file_name("file.bin.part").exists());
}

#[test]
fn checksum_mismatch_deletes_and_explains() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("bad/file.bin");
    let fake = "f".repeat(64);
    let err = download_verified(
        &format!("{}/file", server()),
        &dest,
        &fake,
        &CancelToken::new(),
        &mut |_| {},
    )
    .unwrap_err();
    assert!(err.to_string().contains("Checksum mismatch"));
    assert!(err.to_string().contains(&format!("expected sha256 {fake}")));
    assert!(!dest.exists());
    assert!(!dest.with_file_name("file.bin.part").exists());
}

#[test]
fn failed_verification_keeps_previous_file() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("keep.bin");
    std::fs::write(&dest, "previous").unwrap();
    assert!(
        download_verified(
            &format!("{}/file", server()),
            &dest,
            &"0".repeat(64),
            &CancelToken::new(),
            &mut |_| {}
        )
        .is_err()
    );
    assert_eq!(std::fs::read_to_string(&dest).unwrap(), "previous");
}

#[test]
fn http_errors_loops_and_non_https() {
    let dir = tempfile::tempdir().unwrap();
    let s = sha(&payload());
    let dl = |path: &str, name: &str| {
        download_verified(
            path,
            &dir.path().join(name),
            &s,
            &CancelToken::new(),
            &mut |_| {},
        )
        .unwrap_err()
        .to_string()
    };
    assert!(dl(&format!("{}/missing", server()), "x").contains("HTTP 404"));
    assert!(dl(&format!("{}/loop", server()), "y").contains("Too many redirects"));
    assert!(dl(&format!("{}/offsite", server()), "z").contains("non-https"));
    assert!(dl("http://example.com/f", "w").contains("non-https"));
}

#[test]
fn cancel_aborts_and_cleans_up() {
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("abort/file.bin");
    let cancel = CancelToken::new();
    let c2 = cancel.clone();
    let start = Instant::now();
    let err = download_verified(
        &format!("{}/slow", server()),
        &dest,
        &sha(&payload()),
        &cancel,
        &mut |_| c2.cancel(),
    )
    .unwrap_err();
    assert!(crate::is_cancelled_error(&err), "{err}");
    assert!(start.elapsed() < Duration::from_secs(4));
    assert!(!dest.exists());
    assert!(!dest.with_file_name("file.bin.part").exists());
}

// ─── EmbeddingRuntime: no side effects until asked ───────────────────────────

fn opts(root: PathBuf, platform: (&'static str, &'static str)) -> EmbeddingRuntimeOptions {
    EmbeddingRuntimeOptions {
        root_dir: root,
        idle: None,
        threads: None,
        platform: Some(platform),
        log: None,
    }
}

#[test]
fn construction_touches_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("never-created");
    let rt = EmbeddingRuntime::new(opts(root.clone(), ("darwin", "arm64")));
    assert!(!root.exists());
    assert!(!rt.is_installed());
    assert!(!root.exists());
    assert!(rt.is_supported());
    assert_eq!(
        rt.download_bytes(),
        select_llama_asset("darwin", "arm64").unwrap().size + MODEL_ASSET.size
    );
    assert_eq!(rt.model_path(), root.join("models").join(MODEL_ASSET.file));
    assert!(rt.server_pid().is_none());
}

#[test]
fn unsupported_platform_refuses_install() {
    let dir = tempfile::tempdir().unwrap();
    let rt = EmbeddingRuntime::new(opts(dir.path().join("rt"), ("linux", "arm64")));
    assert!(!rt.is_supported());
    assert!(!rt.is_installed());
    let err = rt.install(&CancelToken::new(), &mut |_| {}).unwrap_err();
    assert!(err.to_string().contains("isn't available"));
    assert!(!dir.path().join("rt").exists());
}

#[test]
fn embed_empty_is_noop_and_uninstalled_embed_fails_without_spawning() {
    let dir = tempfile::tempdir().unwrap();
    let rt = EmbeddingRuntime::new(opts(dir.path().join("absent"), ("darwin", "arm64")));
    assert!(rt.embed(&[], EmbedKind::Query).unwrap().is_empty());
    assert!(
        rt.embed(&["x".into()], EmbedKind::Query)
            .unwrap_err()
            .to_string()
            .contains("not installed")
    );
    assert!(rt.server_pid().is_none());
    rt.stop();
    assert!(!dir.path().join("absent").exists());
}

#[test]
fn installed_record_matches_the_electron_layout() {
    let rec = InstalledRecord {
        version: 1,
        platform: "darwin-arm64".into(),
        llama: Some(InstalledPin {
            tag: Some(LLAMA_CPP_TAG.into()),
            revision: None,
            sha256: "a".into(),
        }),
        model: None,
    };
    assert_eq!(
        serde_json::to_string_pretty(&rec).unwrap(),
        "{\n  \"version\": 1,\n  \"platform\": \"darwin-arm64\",\n  \"llama\": {\n    \"tag\": \"b11960\",\n    \"sha256\": \"a\"\n  }\n}"
    );
}

/// Real end-to-end check: downloads the pinned release + model (~335 MB),
/// spawns llama-server and embeds. Run explicitly with
/// `SG_RUNTIME_LIVE_DIR=/some/dir cargo test -p sg-history -- --ignored live`.
#[test]
#[ignore]
fn live_install_and_embed() {
    let root = std::env::var_os("SG_RUNTIME_LIVE_DIR")
        .map(PathBuf::from)
        .expect("set SG_RUNTIME_LIVE_DIR");
    let rt = EmbeddingRuntime::new(EmbeddingRuntimeOptions {
        root_dir: root,
        idle: Some(Duration::from_secs(3)),
        threads: None,
        platform: None,
        log: Some(Arc::new(|m: &str| eprintln!("{m}"))),
    });
    rt.install(&CancelToken::new(), &mut |p| eprintln!("{p:?}"))
        .unwrap();
    assert!(rt.is_installed());
    let docs = rt
        .embed(
            &[
                "the settings file is written atomically via rename".into(),
                "a recipe for pancakes".into(),
            ],
            EmbedKind::Document,
        )
        .unwrap();
    let q = rt
        .embed(
            &["why do we write settings atomically".into()],
            EmbedKind::Query,
        )
        .unwrap();
    let dot = |a: &[f32], b: &[f32]| {
        a.iter()
            .zip(b)
            .map(|(x, y)| f64::from(*x) * f64::from(*y))
            .sum::<f64>()
    };
    assert!(dot(&q[0], &docs[0]) > dot(&q[0], &docs[1]));
    std::thread::sleep(Duration::from_secs(5));
    assert!(rt.server_pid().is_none(), "idle shutdown");
}
