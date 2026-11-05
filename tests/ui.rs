//! The local page: the HTML works offline, and `doc-merge ui` takes uploads, merges them and
//! hands the result back over HTTP on 127.0.0.1, as the CLI does with files.

mod common;

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use common::*;
use serde_json::{Value, json};

const BIN: &str = env!("CARGO_BIN_EXE_doc-merge");
const PAGE: &str = include_str!("../src/ui.html");
const WORD: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
const POWERPOINT: &str =
    "application/vnd.openxmlformats-officedocument.presentationml.presentation";
const EXCEL: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";

#[test]
fn page_loads_nothing_from_another_host() {
    let footer = [
        "https://nowmyagentcan.com",
        "https://github.com/nowmyagentcan/doc-merge",
        "https://x.com/chayp05",
    ];
    for pat in ["src=", "href=", "url(", "@import", "fetch(", "action="] {
        for (i, _) in PAGE.match_indices(pat) {
            let rest = PAGE[i + pat.len()..].trim_start_matches(['"', '\'', ' ']);
            let target = &rest[..rest.find(['"', '\'', ' ', ')', '>']).unwrap_or(rest.len())];
            assert!(
                target.starts_with("data:") || !target.contains("://") || footer.contains(&target),
                "{pat} loads {target}"
            );
        }
    }
}

#[test]
fn page_calls_the_real_routes() {
    for route in ["\"/api/files\"", "\"/api/merge\"", "\"/api/result\""] {
        assert!(PAGE.contains(route), "{route}");
    }
    for stub in ["samples", "SLIDES", "pause("] {
        assert!(!PAGE.contains(stub), "{stub}");
    }
}

fn fixture(name: &str) -> Vec<u8> {
    fs::read(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name),
    )
    .unwrap()
}

/// `doc-merge ui` on a free port, stopped when dropped.
struct Server {
    child: Child,
    port: u16,
}

/// A response: status, headers and body.
type Answer = (u16, String, Vec<u8>);

impl Server {
    fn start() -> Server {
        Self::start_in(&std::env::temp_dir())
    }

    fn start_in(dir: &Path) -> Server {
        let mut child = Command::new(BIN)
            .args(["ui", "--no-open"])
            .current_dir(dir)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let port = line
            .strip_prefix("Open http://127.0.0.1:")
            .and_then(|rest| rest.strip_suffix("  (Ctrl+C to stop)\n"))
            .unwrap_or_else(|| panic!("{line:?}"))
            .parse()
            .unwrap();
        Server { child, port }
    }

    /// Sends `head` (`{port}` filled in) and `body`, closes the sending side, and reads the
    /// answer.
    fn raw(&self, head: &[u8], body: &[u8]) -> Answer {
        let mut stream = TcpStream::connect(("127.0.0.1", self.port)).unwrap();
        let (mut filled, mut rest) = (vec![], head);
        while let Some(i) = rest.windows(6).position(|w| w == b"{port}") {
            filled.extend_from_slice(&rest[..i]);
            filled.extend_from_slice(self.port.to_string().as_bytes());
            rest = &rest[i + 6..];
        }
        stream.write_all(&[&filled, rest].concat()).unwrap();
        stream.write_all(b"\r\n\r\n").unwrap();
        stream.write_all(body).unwrap();
        stream.shutdown(Shutdown::Write).unwrap();
        answer(&mut stream)
    }

    fn get(&self, path: &str) -> Answer {
        self.raw(
            format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{{port}}").as_bytes(),
            b"",
        )
    }

    /// The page's upload of one file.
    fn upload(&self, name: &str, bytes: &[u8]) -> (u16, Value) {
        let head = format!(
            "POST /api/files HTTP/1.1\r\nHost: 127.0.0.1:{{port}}\r\nOrigin: http://127.0.0.1:{{port}}\r\nX-File-Name: {name}\r\nContent-Length: {}",
            bytes.len()
        );
        let (status, _, body) = self.raw(head.as_bytes(), bytes);
        (status, serde_json::from_slice(&body).unwrap())
    }

    /// Uploads a committed fixture and gives its id.
    fn add(&self, name: &str) -> String {
        let (status, listed) = self.upload(name, &fixture(name));
        assert_eq!(status, 200, "{listed}");
        listed["id"].as_str().unwrap().to_string()
    }

    fn merge(&self, request: &Value) -> (u16, Value) {
        let text = request.to_string();
        let head = format!(
            "POST /api/merge HTTP/1.1\r\nHost: localhost:{{port}}\r\nContent-Type: application/json\r\nContent-Length: {}",
            text.len()
        );
        let (status, _, body) = self.raw(head.as_bytes(), text.as_bytes());
        (status, serde_json::from_slice(&body).unwrap())
    }

    fn delete(&self, id: &str) -> u16 {
        let head = format!("DELETE /api/files/{id} HTTP/1.1\r\nHost: 127.0.0.1:{{port}}");
        self.raw(head.as_bytes(), b"").0
    }

    fn still_answers(&self) {
        let (status, _, body) = self.get("/");
        assert_eq!((status, body.len()), (200, PAGE.len()));
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn answer(stream: &mut TcpStream) -> Answer {
    let mut answer = vec![];
    stream.read_to_end(&mut answer).unwrap();
    let end = answer.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let headers = String::from_utf8(answer[..end].to_vec()).unwrap();
    let status = headers.split(' ').nth(1).unwrap().parse().unwrap();
    (status, headers, answer[end + 4..].to_vec())
}

/// The value of a response header.
fn header<'a>(headers: &'a str, name: &str) -> Option<&'a str> {
    headers.lines().find_map(|l| {
        let (k, v) = l.split_once(": ")?;
        k.eq_ignore_ascii_case(name).then_some(v)
    })
}

fn code(answer: &[u8]) -> String {
    let e: Value = serde_json::from_slice(answer).unwrap();
    e["error"]["code"].as_str().unwrap().to_string()
}

#[test]
fn serves_the_page_with_its_guards_on() {
    let s = Server::start();
    let (status, headers, body) = s.get("/");
    assert_eq!(status, 200);
    assert_eq!(body, PAGE.as_bytes());
    assert_eq!(
        header(&headers, "Content-Type"),
        Some("text/html; charset=utf-8")
    );
    assert_eq!(header(&headers, "Cache-Control"), Some("no-store"));
    assert_eq!(header(&headers, "X-Content-Type-Options"), Some("nosniff"));
    let csp = header(&headers, "Content-Security-Policy").unwrap();
    assert!(csp.starts_with("default-src 'none';"), "{csp}");
    assert!(csp.contains("connect-src 'self'"), "{csp}");
    for host in ["localhost:{port}", "LOCALHOST:{port}"] {
        let head = format!("GET /?x=1 HTTP/1.1\r\nHost: {host}");
        assert_eq!(s.raw(head.as_bytes(), b"").0, 200, "{host}");
    }
}

#[test]
fn every_guard_answers_as_stated() {
    let s = Server::start();
    let post = "POST /api/merge HTTP/1.1\r\nHost: 127.0.0.1:{port}";
    let head = |rest: &str| format!("{post}\r\nContent-Length: 1{rest}").into_bytes();
    let get = "GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}";
    let mut cases: Vec<(Vec<u8>, &[u8], u16, &str)> = vec![
        (b"GET / HTTP/1.1\r\nHost: evil.test".to_vec(), b"", 403, "forbidden"),
        (b"GET / HTTP/1.1\r\nHost: 127.0.0.1".to_vec(), b"", 403, "forbidden"),
        (b"GET / HTTP/1.1\r\nHost: 127.0.0.1:1".to_vec(), b"", 403, "forbidden"),
        (b"GET / HTTP/1.0".to_vec(), b"", 403, "forbidden"),
        (head("\r\nOrigin: https://evil.test"), b"x", 403, "forbidden"),
        (head("\r\nOrigin: null"), b"x", 403, "forbidden"),
        (
            b"GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nHost: evil.test".to_vec(),
            b"",
            400,
            "bad_request",
        ),
        (
            head("\r\nOrigin: http://127.0.0.1:{port}\r\nOrigin: http://evil.test"),
            b"x",
            400,
            "bad_request",
        ),
        (head("\r\nContent-Length: 1"), b"x", 400, "bad_request"),
        (head("\r\nTransfer-Encoding: chunked"), b"x", 400, "bad_request"),
        (head("\r\ntransfer-encoding: identity"), b"x", 400, "bad_request"),
        (
            b"GET /\xFF HTTP/1.1\r\nHost: 127.0.0.1:{port}".to_vec(),
            b"",
            400,
            "bad_request",
        ),
        (
            b"GET  / HTTP/1.1\r\nHost: 127.0.0.1:{port}".to_vec(),
            b"",
            400,
            "bad_request",
        ),
        (
            b"GET / HTTP/2\r\nHost: 127.0.0.1:{port}".to_vec(),
            b"",
            400,
            "bad_request",
        ),
        (b"nonsense\r\nHost: 127.0.0.1:{port}".to_vec(), b"", 400, "bad_request"),
        (
            [get.as_bytes(), b"\r\nX-Other: \xFF"].concat(),
            b"",
            400,
            "bad_request",
        ),
        (format!("{get}\r\nno colon").into_bytes(), b"", 400, "bad_request"),
        (format!("{post}\r\nContent-Length: +1").into_bytes(), b"x", 400, "bad_request"),
        (format!("{post}\r\nContent-Length: x").into_bytes(), b"x", 400, "bad_request"),
        (post.as_bytes().to_vec(), b"", 400, "bad_request"),
        (format!("{post}\r\nContent-Length: 10").into_bytes(), b"short", 400, "bad_request"),
        (
            "POST /api/files HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: 1".into(),
            b"x",
            400,
            "bad_request",
        ),
        (
            "POST /api/files HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nX-File-Name: %zz\r\nContent-Length: 1".into(),
            b"x",
            400,
            "bad_request",
        ),
        (
            "POST /api/files HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nX-File-Name: a.docx\r\nContent-Length: 10".into(),
            b"short",
            400,
            "bad_request",
        ),
        (
            "POST /api/files HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nX-File-Name: a.docx\r\nContent-Length: 536870913".into(),
            b"",
            413,
            "limit_exceeded",
        ),
        (
            "POST /api/files HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nX-File-Name: a.docx\r\nContent-Length: 99999999999999999999999".into(),
            b"",
            413,
            "limit_exceeded",
        ),
        (
            format!("{post}\r\nContent-Length: 1048577").into_bytes(),
            b"",
            413,
            "limit_exceeded",
        ),
        (b"GET /nothing HTTP/1.1\r\nHost: 127.0.0.1:{port}".to_vec(), b"", 404, "not_found"),
        (b"PUT / HTTP/1.1\r\nHost: 127.0.0.1:{port}".to_vec(), b"", 404, "not_found"),
        (b"GET /api/files HTTP/1.1\r\nHost: 127.0.0.1:{port}".to_vec(), b"", 404, "not_found"),
        (b"GET /api/result HTTP/1.1\r\nHost: 127.0.0.1:{port}".to_vec(), b"", 404, "not_found"),
    ];
    for (head, body, status, want) in cases.drain(..) {
        let (got, _, answer) = s.raw(&head, body);
        let shown = String::from_utf8_lossy(&head).into_owned();
        assert_eq!((got, code(&answer).as_str()), (status, want), "{shown}");
    }
    s.still_answers();
}

#[test]
fn a_file_over_the_limit_is_refused_by_name_before_its_body() {
    let s = Server::start();
    let mut stream = TcpStream::connect(("127.0.0.1", s.port)).unwrap();
    write!(
        stream,
        "POST /api/files HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nX-File-Name: big.pptx\r\nContent-Length: 600000000\r\n\r\n",
        s.port
    )
    .unwrap();
    stream.shutdown(Shutdown::Write).unwrap();
    let (status, _, body) = answer(&mut stream);
    assert_eq!(status, 413);
    let e: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        e["error"]["message"],
        "'big.pptx' is over the limit: 573 MiB (at most 512 MiB)."
    );
    assert_eq!(
        e["error"]["hint"],
        "Merge fewer or smaller files at a time."
    );
}

#[test]
fn uploads_held_together_stop_at_the_limit() {
    let s = Server::start();
    // Two uploads under way count in full before their bodies arrive.
    let open = |size: u64| {
        let mut stream = TcpStream::connect(("127.0.0.1", s.port)).unwrap();
        write!(
            stream,
            "POST /api/files HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nX-File-Name: a.docx\r\nContent-Length: {size}\r\n\r\n",
            s.port
        )
        .unwrap();
        stream
    };
    // Each try gives the two longer to be listed, so the third can't arrive before them.
    for attempt in 1.. {
        let _held = [open(500 << 20), open(500 << 20)];
        thread::sleep(Duration::from_millis(100 * attempt));
        let mut third = open(100 << 20);
        third
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut head = [0; 12];
        if third.read_exact(&mut head).is_ok() {
            assert_eq!(&head, b"HTTP/1.1 413");
            break;
        }
        assert!(attempt < 10, "never refused");
    }
    // Their places are free again once they hang up.
    let start = Instant::now();
    while s.upload("a.docx", &fixture("a.docx")).0 == 413 {
        assert!(start.elapsed() < Duration::from_secs(20), "never freed");
    }
}

#[test]
fn at_most_500_files_are_held() {
    let s = Server::start();
    let tiny = build(&minimal("docx", &[]));
    for n in 0..500 {
        assert_eq!(s.upload("t.docx", &tiny).0, 200, "{n}");
    }
    let (status, e) = s.upload("t.docx", &tiny);
    assert_eq!(status, 413);
    assert_eq!(e["error"]["code"], "limit_exceeded");
    assert_eq!(s.delete("500"), 204);
    assert_eq!(s.upload("t.docx", &tiny).0, 200);
}

#[test]
fn uploads_are_listed_with_their_kind_and_size() {
    let s = Server::start();
    for (name, unit, count) in [
        ("a.docx", "sections", 2),
        ("c.docx", "sections", 1),
        ("a.pptx", "slides", 4),
        ("a.xlsx", "sheets", 2),
    ] {
        let bytes = fixture(name);
        let (status, listed) = s.upload(name, &bytes);
        assert_eq!(status, 200, "{listed}");
        let keys: Vec<_> = listed.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["id", "name", "kind", "bytes", unit]);
        assert_eq!(listed["name"], name);
        assert_eq!(listed["kind"], name.rsplit('.').next().unwrap());
        assert_eq!(listed["bytes"], bytes.len());
        assert_eq!(listed[unit], count, "{name}");
    }
    let (_, listed) = s.upload("Gr%C3%BC%C3%9Fe%20a.docx", &fixture("a.docx"));
    assert_eq!(listed["name"], "Grüße a.docx");
}

#[test]
fn each_family_merges_as_the_command_line_does() {
    let s = Server::start();
    let dir = temp_dir("ui-cli");
    for (files, kind, name) in [
        (["a.docx", "b.docx"], WORD, "merged.docx"),
        (["a.pptx", "b.pptx"], POWERPOINT, "merged.pptx"),
        (["a.xlsx", "b.xlsx"], EXCEL, "merged.xlsx"),
    ] {
        let ids: Vec<String> = files.iter().map(|f| s.add(f)).collect();
        let (status, report) = s.merge(&json!({"files": ids, "styles": "keep", "name": name}));
        assert_eq!(status, 200, "{report}");
        assert_eq!(report["output"]["path"], name);
        assert_eq!(report["inputs"][0]["path"], files[0]);
        assert_eq!(report["inputs"][1]["path"], files[1]);
        let (status, headers, body) = s.get("/api/result");
        assert_eq!(status, 200);
        assert_eq!(header(&headers, "Content-Type"), Some(kind));
        assert_eq!(
            header(&headers, "Content-Disposition").unwrap(),
            format!("attachment; filename*=UTF-8''{name}")
        );
        assert_eq!(report["output"]["bytes"], body.len());

        let paths: Vec<_> = files.iter().map(|f| write(&dir, f, &fixture(f))).collect();
        let out = dir.join(name);
        let done = Command::new(BIN)
            .args(&paths)
            .arg("-o")
            .arg(&out)
            .args(["--overwrite", "--json"])
            .output()
            .unwrap();
        assert_eq!(done.status.code(), Some(0), "{done:?}");
        let cli: Value = serde_json::from_slice(&done.stdout).unwrap();
        assert_eq!(report["renames"], cli["renames"]);
        assert_eq!(report["warnings"], cli["warnings"]);
        assert_eq!(body, fs::read(&out).unwrap(), "{name}");
    }
}

#[test]
fn the_order_of_the_ids_is_the_merge_order() {
    let s = Server::start();
    let (a, b) = (s.add("a.xlsx"), s.add("b.xlsx"));
    let sheets = |report: &Value| report["inputs"][0]["sheets"][0]["name"].clone();
    let (_, first) = s.merge(&json!({"files": [&a, &b]}));
    let (_, turned) = s.merge(&json!({"files": [&b, &a]}));
    assert_eq!(sheets(&first), "Data");
    assert_eq!(sheets(&turned), "Summary");
    assert_eq!(turned["inputs"][0]["path"], "b.xlsx");
    let bytes = s.get("/api/result").2;
    let dir = temp_dir("ui-order");
    let merged = write(&dir, "m.xlsx", &bytes);
    let book = text(&entries(&merged), "xl/workbook.xml");
    assert!(book.find("\"Summary\"").unwrap() < book.find("\"Data\"").unwrap());
}

#[test]
fn word_styles_and_output_names_are_honoured() {
    let s = Server::start();
    let ids = [s.add("a.docx"), s.add("b.docx")];
    let (_, first) = s.merge(&json!({"files": ids, "styles": "first"}));
    let styles = |r: &Value| {
        let renames = r["renames"].as_array().unwrap().iter();
        renames.filter(|r| r["kind"] == "style").count()
    };
    assert_eq!(styles(&first), 0);
    assert_eq!(first["output"]["path"], "merged.docx");
    let (_, keep) = s.merge(&json!({"files": ids}));
    assert!(styles(&keep) > 0);

    let (status, named) = s.merge(&json!({"files": ids, "name": "../../Grüße b.docx"}));
    assert_eq!(
        (status, &named["output"]["path"]),
        (200, &json!("Grüße b.docx"))
    );
    let (_, headers, _) = s.get("/api/result");
    assert_eq!(
        header(&headers, "Content-Disposition"),
        Some("attachment; filename*=UTF-8''Gr%C3%BC%C3%9Fe%20b.docx")
    );

    let (status, e) = s.merge(&json!({"files": ids, "name": "b.pptx"}));
    assert_eq!(
        (status, &e["error"]["code"]),
        (422, &json!("bad_extension"))
    );
}

#[test]
fn a_refused_merge_says_why_and_leaves_no_result() {
    let s = Server::start();
    let (a, word, pivot, old) = (
        s.add("a.xlsx"),
        s.add("a.docx"),
        s.add("p.xlsx"),
        s.add("c.xlsx"),
    );
    assert_eq!(s.merge(&json!({"files": [&a, &a]})).0, 200);
    assert_eq!(s.get("/api/result").0, 200);
    for (files, status, want, input) in [
        (json!([&a, &word]), 422, "mixed_formats", json!(null)),
        (json!([&a, &pivot]), 422, "unsupported_variant", json!(2)),
        (json!([&a, &old]), 422, "date_system_differs", json!(2)),
    ] {
        let (got, e) = s.merge(&json!({"files": files}));
        assert_eq!((got, &e["error"]["code"]), (status, &json!(want)), "{e}");
        assert_eq!(e["error"]["input"], input);
        assert!(e["error"]["hint"].is_string(), "{e}");
        assert_eq!(s.get("/api/result").0, 404);
    }
    for (files, want) in [
        (json!([]), "need_two_files"),
        (json!([&a]), "need_two_files"),
        (json!([&a, "999"]), "file_not_found"),
        (json!([&a, "x"]), "file_not_found"),
        (json!(vec![&a; 501]), "limit_exceeded"),
    ] {
        let (got, e) = s.merge(&json!({"files": files}));
        assert_eq!((got, &e["error"]["code"]), (422, &json!(want)), "{e}");
    }
    for bad in [
        json!({"files": [&a, &a], "extra": 1}),
        json!({"files": [&a, &a], "styles": "last"}),
        json!({"files": "x"}),
        json!({}),
    ] {
        assert_eq!(s.merge(&bad).0, 400, "{bad}");
    }
}

#[test]
fn a_deleted_file_is_gone_and_deleting_it_again_is_fine() {
    let s = Server::start();
    let (a, b) = (s.add("a.docx"), s.add("b.docx"));
    assert_eq!(s.delete(&b), 204);
    assert_eq!(s.delete(&b), 204);
    assert_eq!(s.delete("nonsense"), 204);
    let (status, e) = s.merge(&json!({"files": [&a, &b]}));
    assert_eq!(
        (status, &e["error"]["code"]),
        (422, &json!("file_not_found"))
    );
    let head = format!("DELETE /api/files/{a} HTTP/1.1\r\nHost: 127.0.0.1:{{port}}");
    let (status, headers, body) = s.raw(head.as_bytes(), b"");
    assert_eq!((status, body.len()), (204, 0));
    assert_eq!(header(&headers, "Content-Length"), None);
}

#[test]
fn merges_side_by_side_all_finish_and_nothing_reaches_the_disk() {
    let dir = temp_dir("ui-disk");
    let s = Server::start_in(&dir);
    let ids = [s.add("a.pptx"), s.add("b.pptx"), s.add("c.pptx")];
    thread::scope(|scope| {
        for _ in 0..3 {
            scope.spawn(|| assert_eq!(s.merge(&json!({"files": ids})).0, 200));
        }
    });
    assert_eq!(s.get("/api/result").0, 200);
    assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
}

#[test]
fn a_bad_file_leaves_the_page_answering() {
    let s = Server::start();
    let good = s.add("a.docx");
    for (name, bytes, want) in hostile() {
        let (status, answer) = s.upload(name, &bytes);
        if status == 200 {
            let id = answer["id"].as_str().unwrap();
            let (status, e) = s.merge(&json!({"files": [&good, id]}));
            assert_eq!((status, &e["error"]["code"]), (422, &json!(want)), "{name}");
        } else {
            assert_eq!(
                (status, &answer["error"]["code"]),
                (422, &json!(want)),
                "{name}"
            );
        }
        s.still_answers();
    }
    let next = s.add("b.docx");
    assert_eq!(s.merge(&json!({"files": [&good, &next]})).0, 200);
}

#[test]
fn a_client_that_hangs_up_loses_only_its_own_connection() {
    let s = Server::start();
    for partial in [&b"GET"[..], b"POST /api/files HTTP/1.1\r\nHost: 127.0.0.1"] {
        let mut stream = TcpStream::connect(("127.0.0.1", s.port)).unwrap();
        stream.write_all(partial).unwrap();
    }
    let ids = [s.add("a.docx"), s.add("b.docx")];
    assert_eq!(s.merge(&json!({"files": ids})).0, 200);
    let mut stream = TcpStream::connect(("127.0.0.1", s.port)).unwrap();
    write!(
        stream,
        "GET /api/result HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n",
        s.port
    )
    .unwrap();
    drop(stream);
    s.still_answers();
}

#[test]
fn a_port_in_use_is_refused_with_its_two_lines() {
    let taken = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = taken.local_addr().unwrap().port().to_string();
    let out = Command::new(BIN)
        .args(["ui", "--no-open", "--port", &port])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3));
    assert_eq!(
        String::from_utf8(out.stderr).unwrap(),
        format!("✗ Port {port} is in use.\n  Use --port 0 for a free port.\n")
    );
    assert!(out.stdout.is_empty());
}

#[test]
fn a_chosen_port_is_the_port_served() {
    let free = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = free.local_addr().unwrap().port();
    drop(free);
    let mut child = Command::new(BIN)
        .args(["ui", "--no-open", "--port", &port.to_string()])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(
        line,
        format!("Open http://127.0.0.1:{port}  (Ctrl+C to stop)\n")
    );
    let s = Server { child, port };
    s.still_answers();
}

#[test]
fn exits_quietly_when_stdout_is_closed() {
    // The read end closes before the spawn: dropped after it, a test spawning beside this one
    // could inherit a copy and keep the pipe open.
    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);
    let out = Command::new(BIN)
        .args(["ui", "--no-open"])
        .stdout(writer)
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert_eq!(
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).as_ref()
        ),
        (Some(0), "")
    );
}

#[test]
fn has_a_help_and_refuses_a_bad_port() {
    let run = |args: &[&str]| Command::new(BIN).args(args).output().unwrap();
    let out = run(&["ui", "--help"]);
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        text.starts_with("Open the local page in your browser.\n"),
        "{text}"
    );
    for bad in ["70000", "x", "-1"] {
        let out = run(&["ui", "--no-open", "--port", bad]);
        assert_eq!(out.status.code(), Some(2), "{bad}");
    }
}
