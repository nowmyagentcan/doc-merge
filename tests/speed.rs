//! The speed and memory targets, timed on a release build:
//! `cargo test --release --test speed -- --ignored --nocapture`. A row is the median of 10 runs,
//! one process per run, and the highest peak memory among them; the page's row is a session
//! with a server already running. The large inputs grow out of the committed fixtures.

mod common;

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use common::*;
use serde_json::json;

const BIN: &str = env!("CARGO_BIN_EXE_doc-merge");
const MB: u64 = 1_000_000;
const LAYOUT_REL: &str =
    "http://schemas.openxmlformats.org/officeDocument/2006/relationships/slideLayout";
const SLIDE_REL: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide";
const SLIDE_TYPE: &str = "application/vnd.openxmlformats-officedocument.presentationml.slide+xml";
const SENTENCE: &str = "The quick brown fox jumps over the lazy dog, and the dog takes it well, as dogs do when a page of a report needs a line of text.";

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// Bytes that don't compress, the same for the same seed.
fn noise(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut bytes = Vec::with_capacity(len + 8);
    while bytes.len() < len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        bytes.extend(x.to_le_bytes());
    }
    bytes.truncate(len);
    bytes
}

/// `a.docx` with `paragraphs` more styled list paragraphs and pictures of its own.
fn word(paragraphs: usize, seed: u64) -> Vec<u8> {
    let mut parts = entries(&fixture("a.docx"));
    let mut doc = text(&parts, "word/document.xml");
    let added: String = (0..paragraphs)
        .map(|n| {
            format!(
                r#"<w:p><w:pPr><w:pStyle w:val="ClauseText"/><w:numPr><w:ilvl w:val="0"/><w:numId w:val="1"/></w:numPr></w:pPr><w:r><w:rPr><w:b/></w:rPr><w:t xml:space="preserve">{SENTENCE}</w:t></w:r><w:r><w:t>{n}</w:t></w:r></w:p>"#
            )
        })
        .collect();
    doc.insert_str(doc.rfind("<w:sectPr").unwrap(), &added);
    set(&mut parts, "word/document.xml", doc.as_bytes());
    for (i, (name, data)) in parts.iter_mut().enumerate() {
        if name.starts_with("word/media/") {
            *data = noise(100_000, seed * 100 + i as u64);
        }
    }
    build(&parts)
}

/// `a.xlsx` with `rows` more rows of 10 cells on its first sheet: a shared string, 8 numbers
/// and a formula with 3 sheet references.
fn workbook(rows: usize, seed: u64) -> Vec<u8> {
    let mut parts = entries(&fixture("a.xlsx"));
    let strings = text(&parts, "xl/sharedStrings.xml");
    let count = strings.matches("<si>").count();
    let added: String = (0..rows)
        .map(|k| format!("<si><t>Item {k} of workbook {}</t></si>", seed % 3))
        .collect();
    let strings = strings.replace("</sst>", &(added + "</sst>"));
    set(&mut parts, "xl/sharedStrings.xml", strings.as_bytes());
    let mut added = String::new();
    for k in 0..rows {
        let r = 1000 + k;
        added += &format!(r#"<row r="{r}"><c r="A{r}" t="s"><v>{}</v></c>"#, count + k);
        for (n, column) in "BCDEFGHI".chars().enumerate() {
            added += &format!(r#"<c r="{column}{r}"><v>{}.5</v></c>"#, r * (n + 1));
        }
        added += &format!(
            r#"<c r="J{r}"><f>Summary!B2*2+SUM(Data!B{r}:I{r})+Summary!$B$3</f><v>1</v></c></row>"#
        );
    }
    let sheet = text(&parts, "xl/worksheets/sheet1.xml");
    let sheet = sheet.replace("</sheetData>", &(added + "</sheetData>"));
    set(&mut parts, "xl/worksheets/sheet1.xml", sheet.as_bytes());
    build(&parts)
}

/// `a.pptx` grown to `slides` slides, the new ones all showing `picture`.
fn deck(slides: usize, picture: &[u8]) -> Vec<u8> {
    let mut parts = entries(&fixture("a.pptx"));
    let mut main = text(&parts, "ppt/presentation.xml");
    let mut main_rels = text(&parts, "ppt/_rels/presentation.xml.rels");
    let mut types = text(&parts, "[Content_Types].xml");
    if !types.contains("Extension=\"png\"") {
        let png = r#"<Default Extension="png" ContentType="image/png"/>"#;
        types = types.replace("<Override", &format!("{png}<Override"));
    }
    let slide = format!(
        r#"<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"><p:cSld><p:spTree><p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/><p:pic><p:nvPicPr><p:cNvPr id="2" name="Picture"/><p:cNvPicPr/><p:nvPr/></p:nvPicPr><p:blipFill><a:blip r:embed="rId2"/><a:stretch><a:fillRect/></a:stretch></p:blipFill><p:spPr><a:xfrm><a:off x="1000" y="2000"/><a:ext cx="3000000" cy="2000000"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></p:spPr></p:pic><p:sp><p:nvSpPr><p:cNvPr id="3" name="Text"/><p:cNvSpPr/><p:nvPr/></p:nvSpPr><p:spPr/><p:txBody><a:bodyPr/><a:p><a:r><a:rPr lang="en-US" sz="2400"/><a:t>{SENTENCE}</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#
    );
    let slide_rels = rels(&[
        ("rId1", LAYOUT_REL, "../slideLayouts/slideLayout2.xml"),
        ("rId2", IMAGE_REL, "../media/photo.png"),
    ]);
    let first_new = parts
        .iter()
        .filter(|p| p.0.starts_with("ppt/slides/slide"))
        .count()
        + 1;
    for n in first_new..=slides {
        let (id, rid) = (1000 + n, format!("rId{}", 1000 + n));
        main = main.replace(
            "</p:sldIdLst>",
            &format!(r#"<p:sldId id="{id}" r:id="{rid}"/></p:sldIdLst>"#),
        );
        main_rels = main_rels.replace(
            "</Relationships>",
            &format!(
                r#"<Relationship Id="{rid}" Type="{SLIDE_REL}" Target="slides/slide{n}.xml"/></Relationships>"#
            ),
        );
        types = types.replace(
            "</Types>",
            &format!(
                r#"<Override PartName="/ppt/slides/slide{n}.xml" ContentType="{SLIDE_TYPE}"/></Types>"#
            ),
        );
        parts.push((
            format!("ppt/slides/slide{n}.xml"),
            slide.clone().into_bytes(),
        ));
        parts.push((
            format!("ppt/slides/_rels/slide{n}.xml.rels"),
            slide_rels.clone().into_bytes(),
        ));
    }
    set(&mut parts, "ppt/presentation.xml", main.as_bytes());
    set(
        &mut parts,
        "ppt/_rels/presentation.xml.rels",
        main_rels.as_bytes(),
    );
    set(&mut parts, "[Content_Types].xml", types.as_bytes());
    parts.push(("ppt/media/photo.png".into(), picture.to_vec()));
    build(&parts)
}

/// A running or finished child's memory counters.
#[cfg(windows)]
fn counters(child: &Child) -> windows_sys::Win32::System::ProcessStatus::PROCESS_MEMORY_COUNTERS {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    // SAFETY: the structure is plain numbers, for which zero is valid.
    let mut counters: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
    let size = size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
    // SAFETY: the handle is the child's, still open, and `counters` is as large as `size` says.
    let done = unsafe { K32GetProcessMemoryInfo(child.as_raw_handle(), &mut counters, size) };
    assert_ne!(done, 0);
    counters
}

/// The peak resident memory of a child that is ending, and its exit code.
#[cfg(windows)]
fn wait(child: &mut Child) -> (Option<u64>, i32) {
    let code = child.wait().unwrap().code().unwrap();
    (Some(counters(child).PeakWorkingSetSize as u64), code)
}

#[cfg(target_os = "linux")]
fn wait(child: &mut Child) -> (Option<u64>, i32) {
    let (mut status, mut usage) = (0, unsafe { std::mem::zeroed::<libc::rusage>() });
    // SAFETY: the pid is our unreaped child; both out-pointers are valid.
    let pid = unsafe { libc::wait4(child.id() as i32, &mut status, 0, &mut usage) };
    assert!(pid > 0 && libc::WIFEXITED(status));
    (
        Some(usage.ru_maxrss as u64 * 1024),
        libc::WEXITSTATUS(status),
    )
}

#[cfg(not(any(windows, target_os = "linux")))]
fn wait(child: &mut Child) -> (Option<u64>, i32) {
    (None, child.wait().unwrap().code().unwrap())
}

/// The resident memory of a running process, where this OS reports it.
fn resident(child: &Child) -> Option<u64> {
    #[cfg(windows)]
    return Some(counters(child).WorkingSetSize as u64);
    #[cfg(not(windows))]
    {
        let status = fs::read_to_string(format!("/proc/{}/status", child.id())).ok()?;
        let line = status.lines().find(|l| l.starts_with("VmRSS:"))?;
        Some(line.split_whitespace().nth(1)?.parse::<u64>().ok()? * 1024)
    }
}

/// The median wall time of 10 runs of the binary with `args`, each ending with `code`, and the
/// largest peak memory.
fn timed(args: &[String], code: i32) -> (Duration, Option<u64>) {
    let mut runs: Vec<_> = (0..10)
        .map(|_| {
            let start = Instant::now();
            let mut child = Command::new(BIN)
                .args(args)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let (rss, got) = wait(&mut child);
            assert_eq!(got, code, "{args:?}");
            (start.elapsed(), rss)
        })
        .collect();
    let rss = runs.iter().filter_map(|r| r.1).max();
    runs.sort();
    (runs[5].0, rss)
}

fn median(mut run: impl FnMut()) -> Duration {
    let mut times: Vec<_> = (0..10)
        .map(|_| {
            let start = Instant::now();
            run();
            start.elapsed()
        })
        .collect();
    times.sort();
    times[5]
}

struct Row {
    case: String,
    time: Duration,
    target: Duration,
    rss: Option<u64>,
    rss_target: Option<u64>,
}

fn row(case: &str, (time, rss): (Duration, Option<u64>), ms: u64, rss_mb: Option<u64>) -> Row {
    Row {
        case: case.into(),
        time,
        target: Duration::from_millis(ms),
        rss,
        rss_target: rss_mb.map(|m| m * MB),
    }
}

/// `paths` merged into `out`, as arguments.
fn merging(paths: &[PathBuf], out: &Path) -> Vec<String> {
    let mut args: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
    args.extend(["-o".into(), out.display().to_string(), "--overwrite".into()]);
    args
}

/// One request to the page on `port`; the answer's status and body.
fn request(port: &str, head: &str, body: &[u8]) -> (u16, Vec<u8>) {
    let mut stream = TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
    write!(
        stream,
        "{head}\r\nHost: 127.0.0.1:{port}\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .unwrap();
    stream.write_all(body).unwrap();
    let mut answer = vec![];
    stream.read_to_end(&mut answer).unwrap();
    let status = String::from_utf8_lossy(&answer[9..12]).parse().unwrap();
    let end = answer.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    (status, answer[end + 4..].to_vec())
}

#[test]
#[ignore = "times a release build: cargo test --release --test speed -- --ignored --nocapture"]
fn meets_the_speed_and_memory_targets() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("speed");
    // A child process writes the inputs: on Linux a child's peak memory starts at its parent's,
    // so the measuring process must never hold the large ones.
    let writing = std::env::var_os("DOC_MERGE_SPEED_WRITE").is_some();
    if !writing {
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let status = Command::new(std::env::current_exe().unwrap())
            .args(["--ignored", "--exact", "meets_the_speed_and_memory_targets"])
            .env("DOC_MERGE_SPEED_WRITE", "1")
            .stdout(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        // A virus scanner unpacks new Office files for over a minute, and everything beside
        // it runs several times slower meanwhile.
        std::thread::sleep(Duration::from_secs(90));
    }
    let input = |name: String, data: &dyn Fn() -> Vec<u8>| {
        let path = dir.join(name);
        if writing {
            fs::write(&path, data()).unwrap();
        }
        path
    };
    let decks: Vec<PathBuf> = (0..20)
        .map(|i| {
            // Pairs of decks share one picture.
            input(format!("deck{i}.pptx"), &|| {
                deck(30, &noise(5 << 20, i / 2 + 7))
            })
        })
        .collect();
    let workbooks: Vec<PathBuf> = (0..10)
        .map(|i| input(format!("book{i}.xlsx"), &|| workbook(10_000, i)))
        .collect();
    let reports: Vec<PathBuf> = (0..50)
        .map(|i| input(format!("report{i}.docx"), &|| word(300, i)))
        .collect();
    let long = input("long.docx".into(), &|| word(620_000, 1));
    let zeros = input("zeros.docx".into(), &|| {
        let mut parts = minimal("docx", &[]);
        set(&mut parts, "word/document.xml", &vec![0; 1 << 30]);
        build(&parts)
    });
    let refused: Vec<PathBuf> = hostile()
        .into_iter()
        .map(|(name, bytes, _)| input(format!("x-{name}"), &|| bytes.clone()))
        .collect();
    // Copies beside the other inputs: in WSL the repo is on a slow shared drive.
    let local = |name: &&str| dir.join(format!("local-{name}"));
    if writing {
        for name in [
            "a.docx", "b.docx", "a.pptx", "b.pptx", "c.pptx", "a.xlsx", "b.xlsx",
        ] {
            fs::copy(fixture(name), local(&name)).unwrap();
        }
        return;
    }

    let fixtures = |names: &[&str]| -> Vec<PathBuf> { names.iter().map(&local).collect() };
    let (word_out, deck_out, book_out) = (
        dir.join("out.docx"),
        dir.join("out.pptx"),
        dir.join("out.xlsx"),
    );
    let linux = !cfg!(windows);
    let mut rows = vec![
        row(
            "20 decks x 30 slides",
            timed(&merging(&decks, &deck_out), 0),
            1000,
            Some(80),
        ),
        row(
            "10 workbooks x 100,000 cells",
            timed(&merging(&workbooks, &book_out), 0),
            if linux { 5000 } else { 3000 },
            Some(100),
        ),
        row(
            "50 x a.docx",
            timed(&merging(&vec![local(&"a.docx"); 50], &word_out), 0),
            1000,
            Some(60),
        ),
        row(
            "50 Word documents x 20 pages",
            timed(&merging(&reports, &word_out), 0),
            1000,
            Some(60),
        ),
        row(
            "200 MB document.xml + small",
            timed(&merging(&[long.clone(), local(&"b.docx")], &word_out), 0),
            if linux { 8000 } else { 5000 },
            Some(450),
        ),
        row(
            "--version",
            timed(&["--version".into()], 0),
            if linux { 10 } else { 40 },
            None,
        ),
        row(
            "two Word fixtures",
            timed(&merging(&fixtures(&["a.docx", "b.docx"]), &word_out), 0),
            if linux { 50 } else { 100 },
            Some(30),
        ),
        row(
            "three PowerPoint fixtures",
            timed(
                &merging(&fixtures(&["a.pptx", "b.pptx", "c.pptx"]), &deck_out),
                0,
            ),
            120,
            Some(40),
        ),
        row(
            "two Excel fixtures",
            timed(&merging(&fixtures(&["a.xlsx", "b.xlsx"]), &book_out), 0),
            100,
            Some(30),
        ),
    ];
    let mut worst = (Duration::ZERO, None);
    for bad in refused.iter().chain([&zeros]) {
        let (time, rss) = timed(&merging(&[local(&"a.docx"), bad.clone()], &word_out), 1);
        worst = (worst.0.max(time), worst.1.max(rss));
    }
    rows.push(row("hostile inputs, the slowest", worst, 200, Some(70)));

    // The page: two uploads, a merge and the download on a running server, and its idle memory.
    let mut ui = Command::new(BIN)
        .args(["ui", "--no-open"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(ui.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let port = &line["Open http://127.0.0.1:".len()..line.find("  (").unwrap()];
    std::thread::sleep(Duration::from_millis(300));
    let idle = resident(&ui);
    let (a, b) = (
        fs::read(fixture("a.docx")).unwrap(),
        fs::read(fixture("b.docx")).unwrap(),
    );
    let session = || {
        let add = |name: &str, bytes: &[u8]| {
            let head = format!("POST /api/files HTTP/1.1\r\nX-File-Name: {name}");
            let (status, body) = request(port, &head, bytes);
            assert_eq!(status, 200);
            let listed: serde_json::Value = serde_json::from_slice(&body).unwrap();
            listed["id"].as_str().unwrap().to_string()
        };
        let ids = [add("a.docx", &a), add("b.docx", &b)];
        let merge = json!({"files": ids}).to_string();
        let done = request(port, "POST /api/merge HTTP/1.1", merge.as_bytes());
        assert_eq!(done.0, 200);
        let (status, body) = request(port, "GET /api/result HTTP/1.1", b"");
        assert_eq!(status, 200);
        assert!(body.starts_with(b"PK"));
        for id in ids {
            request(port, &format!("DELETE /api/files/{id} HTTP/1.1"), b"");
        }
    };
    rows.push(row(
        "ui: upload 2, merge, result",
        (median(session), idle),
        300,
        Some(20),
    ));
    ui.kill().unwrap();
    ui.wait().unwrap();

    // MCP: a whole session merging the two Word fixtures, and the server's idle memory.
    let call = json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {
        "name": "merge_documents",
        "arguments": {"files": fixtures(&["a.docx", "b.docx"]), "output": word_out, "overwrite": true}}});
    let messages = format!(
        "{}\n{}\n{}\n{call}\n",
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"speed","version":"1"}}}"#,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
    );
    let mcp = || {
        let mut child = Command::new(BIN)
            .arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(messages.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(String::from_utf8_lossy(&out.stdout).contains("structuredContent"));
    };
    let session = median(mcp);
    let mut idle = Command::new(BIN)
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(300));
    rows.push(row(
        "mcp: session with a merge",
        (session, resident(&idle)),
        300,
        Some(20),
    ));
    drop(idle.stdin.take());
    idle.wait().unwrap();

    let mut missed = vec![];
    for r in &rows {
        let memory = r
            .rss
            .map_or("-".into(), |b| format!("{:.1} MB", b as f64 / 1e6));
        eprintln!(
            "{:<32} {:>8.1} ms (<= {:>5} ms)  {memory:>9} (<= {})",
            r.case,
            r.time.as_secs_f64() * 1e3,
            r.target.as_millis(),
            r.rss_target
                .map_or("-".into(), |b| format!("{} MB", b / MB))
        );
        if r.time > r.target || r.rss.zip(r.rss_target).is_some_and(|(b, t)| b > t) {
            missed.push(&r.case);
        }
    }
    let size = fs::metadata(BIN).unwrap().len();
    eprintln!("{:<32} {:>8.2} MB (<= 6 MB)", "binary", size as f64 / 1e6);
    let _ = fs::remove_dir_all(&dir);
    assert!(size <= 6 << 20, "the binary is {size} bytes");
    assert!(missed.is_empty(), "missed: {missed:?}");
}
