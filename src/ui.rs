//! The local page on 127.0.0.1: uploads wait in memory until the page asks to merge them, and
//! the merged file waits there for its download. Nothing is written to disk.

use std::collections::BTreeMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, Shutdown, TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use doc_merge::error::{Error, mib};
use doc_merge::package::{Family, MAX_FILE, Upload};
use doc_merge::paths::MAX_FILES;
use doc_merge::{Options, Report, StyleMode, TIME_LIMIT, guarded, inspect, merge_uploads};
use serde::{Deserialize, Serialize};
use serde_json::json;

const PAGE: &str = include_str!("ui.html");
/// The most a request may send in its line and headers.
const HEAD_LIMIT: u64 = 16 * 1024;
/// The most all uploads together may hold.
const MAX_HELD: u64 = 1 << 30;
/// The most `POST /api/merge` may send: 500 ids are a few kilobytes.
const MAX_REQUEST: u64 = 1 << 20;
/// Sent per write, so a write's timeout covers a piece and not a whole file.
const CHUNK: usize = 1 << 20;
/// The page reaches nothing but its own server.
const CSP: &str = "default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; img-src data:; connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";
const JSON: &str = "application/json";

/// An upload, listed from before its bytes arrive so that they count against the limits.
struct File {
    name: String,
    size: u64,
    bytes: Option<Upload>,
}

/// The last merged file.
struct Merged {
    name: String,
    family: Family,
    bytes: Arc<Vec<u8>>,
}

struct State {
    files: BTreeMap<u64, File>,
    next_id: u64,
    result: Option<Merged>,
}

static STATE: Mutex<State> = Mutex::new(State {
    files: BTreeMap::new(),
    next_id: 1,
    result: None,
});

/// Requests are answered side by side, but files are read and merged one at a time: each can
/// take hundreds of megabytes.
static JOB: Mutex<()> = Mutex::new(());

fn state() -> MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

fn one_at_a_time() -> MutexGuard<'static, ()> {
    JOB.lock().unwrap_or_else(|e| e.into_inner())
}

struct Reply {
    status: u16,
    kind: &'static str,
    /// Further header lines, each ending in CRLF.
    headers: String,
    body: Arc<Vec<u8>>,
}

impl Reply {
    fn new(status: u16, kind: &'static str, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            kind,
            headers: String::new(),
            body: Arc::new(body.into()),
        }
    }

    fn json(value: &impl Serialize) -> Self {
        let body = serde_json::to_vec(value).expect("results are plain data");
        Self::new(200, JSON, body)
    }

    /// `e` as the page's error object.
    fn refused(status: u16, e: &Error) -> Self {
        Self::new(status, JSON, e.to_json())
    }

    /// An error of the request itself, which no merge code names.
    fn request(status: u16, code: &str, message: &str) -> Self {
        let e = json!({"error": {"code": code, "message": message, "hint": null, "input": null}});
        Self::new(status, JSON, e.to_string())
    }

    fn bad(message: &str) -> Self {
        Self::request(400, "bad_request", message)
    }

    fn not_found() -> Self {
        Self::request(404, "not_found", "There's nothing here.")
    }
}

/// Serves on 127.0.0.1:`port` (0 picks a free port) until the process is stopped, and opens
/// the page in the default browser when `open` is set. `stop` says how to stop it.
pub fn serve(port: u16, open: bool, stop: &str) -> Result<(), Error> {
    let failed = |e: &io::Error| Error::internal("the page can't start").because(e);
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).map_err(|e| {
        let taken = matches!(
            e.kind(),
            io::ErrorKind::AddrInUse | io::ErrorKind::PermissionDenied
        );
        if taken && port != 0 {
            Error::port_in_use(port)
        } else {
            failed(&e)
        }
    })?;
    let port = listener.local_addr().map_err(|e| failed(&e))?.port();
    let url = format!("http://127.0.0.1:{port}");
    // With stdout closed nobody learns the address, so end quietly as other commands do.
    match writeln!(io::stdout(), "Open {url}{stop}") {
        Err(e) if e.kind() == io::ErrorKind::BrokenPipe => return Ok(()),
        Err(e) => return Err(failed(&e)),
        Ok(()) => {}
    }
    if open {
        browse(url);
    }
    for stream in listener.incoming().flatten() {
        // A client that hangs up or stalls loses only its own connection.
        let _ = thread::Builder::new().spawn(move || handle(&stream, port));
    }
    Ok(())
}

/// Asks the OS to open `url`; when it can't, the printed address is enough.
fn browse(url: String) {
    let mut command = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.args(["/C", "start", ""]);
        c
    } else if cfg!(target_os = "macos") {
        Command::new("open")
    } else {
        Command::new("xdg-open")
    };
    command
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // Some openers wait for the browser to close, so they run beside the server.
    thread::spawn(move || command.status());
}

/// One request, one response, then the connection closes.
fn handle(stream: &TcpStream, port: u16) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let reply = respond(&mut BufReader::new(stream.take(HEAD_LIMIT)), port)?;
    let reason = match reply.status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        413 => "Content Too Large",
        _ => "Unprocessable Content",
    };
    let content = match reply.status {
        204 => String::new(),
        _ => format!(
            "Content-Type: {}\r\nContent-Length: {}\r\n",
            reply.kind,
            reply.body.len()
        ),
    };
    let mut out = stream;
    write!(
        out,
        "HTTP/1.1 {} {reason}\r\n{content}{}Cache-Control: no-store\r\n\
         X-Content-Type-Options: nosniff\r\nContent-Security-Policy: {CSP}\r\n\
         Connection: close\r\n\r\n",
        reply.status, reply.headers,
    )?;
    for piece in reply.body.chunks(CHUNK) {
        out.write_all(piece)?;
    }
    stream.shutdown(Shutdown::Write)?;
    // Read what the client still sends, so closing doesn't reset the connection before it has
    // read the answer: a refused upload may be sending its whole body.
    io::copy(&mut stream.take(MAX_FILE), &mut io::sink()).map(drop)
}

fn respond(reader: &mut BufReader<io::Take<&TcpStream>>, port: u16) -> io::Result<Reply> {
    let (mut request, mut line) = (Vec::new(), Vec::new());
    reader.read_until(b'\n', &mut request)?;
    let (mut host, mut origin, mut length, mut name) = (None, None, None, None);
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            return Ok(Reply::bad("The request ended early."));
        }
        if line.iter().all(|b| b"\r\n".contains(b)) {
            break;
        }
        let Some((key, value)) = header(&line) else {
            return Ok(Reply::bad("Can't read a request header."));
        };
        let field = match key.to_ascii_lowercase().as_str() {
            "host" => &mut host,
            "origin" => &mut origin,
            "content-length" => &mut length,
            "x-file-name" => &mut name,
            // Bodies come only with a Content-Length; chunked ones aren't read.
            "transfer-encoding" => return Ok(Reply::bad("Send the body with a Content-Length.")),
            _ => continue,
        };
        // A second value could disagree with the first one checked (RFC 9112 §3.2, §6.3).
        if field.replace(value.to_owned()).is_some() {
            return Ok(Reply::bad("A request header is repeated."));
        }
    }
    // RFC 9112 §3: the request line is ASCII.
    let request = std::str::from_utf8(&request).ok().filter(|r| r.is_ascii());
    let mut parts = request
        .unwrap_or_default()
        .trim_end_matches(['\r', '\n'])
        .split(' ');
    let (Some(method), Some(target), Some("HTTP/1.1" | "HTTP/1.0"), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Ok(Reply::bad("Can't read the request line."));
    };
    let local = |scheme: &str, value: &str| {
        ["127.0.0.1", "localhost"]
            .iter()
            .any(|h| value.eq_ignore_ascii_case(&format!("{scheme}{h}:{port}")))
    };
    if !host.is_some_and(|h| local("", &h)) || origin.is_some_and(|o| !local("http://", &o)) {
        let message = "Only this computer's page can use doc-merge ui.";
        return Ok(Reply::request(403, "forbidden", message));
    }
    let path = target.split_once('?').map_or(target, |(path, _)| path);
    // Digits only: Rust would also read a leading `+`. A number too long for u64 is too large.
    let length = length
        .filter(|l| !l.is_empty() && l.bytes().all(|b| b.is_ascii_digit()))
        .map(|l| l.parse().unwrap_or(u64::MAX));
    Ok(match (method, path) {
        ("GET", "/") => Reply::new(200, "text/html; charset=utf-8", PAGE),
        ("GET", "/api/result") => result(),
        ("DELETE", _) if path.starts_with("/api/files/") => forget(&path["/api/files/".len()..]),
        ("POST", "/api/files" | "/api/merge") => {
            let Some(length) = length else {
                return Ok(Reply::bad("Can't read the request length."));
            };
            if path == "/api/merge" {
                merge(reader, length)
            } else {
                match name.as_deref().and_then(file_name) {
                    Some(name) => upload(reader, length, name),
                    None => Reply::bad("Can't read the file name."),
                }
            }
        }
        _ => Reply::not_found(),
    })
}

/// A header line's name and value. Malformed: a name that isn't a token, bytes that aren't
/// UTF-8, or a NUL or CR inside the value (RFC 9110 §5.5).
fn header(line: &[u8]) -> Option<(&str, &str)> {
    let line = std::str::from_utf8(line).ok()?;
    let line = line.strip_suffix('\n')?;
    let line = line.strip_suffix('\r').unwrap_or(line);
    let (name, value) = line.split_once(':')?;
    let token = !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b));
    (token && !value.contains(['\0', '\r'])).then_some((name, value.trim()))
}

/// The `length` bytes after the head.
fn body(reader: &mut BufReader<io::Take<&TcpStream>>, length: u64) -> Result<Vec<u8>, Reply> {
    let buffered = reader.buffer().len() as u64;
    reader.get_mut().set_limit(length.saturating_sub(buffered));
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(length as usize).map_err(|_| {
        let e = Error::limit_exceeded("The upload", &mib(length), "the memory free");
        Reply::refused(413, &e)
    })?;
    let read = reader.take(length).read_to_end(&mut bytes);
    if read.is_err() || (bytes.len() as u64) < length {
        return Err(Reply::bad("The request ended early."));
    }
    Ok(bytes)
}

/// Takes one file: read and listed, or refused with why.
fn upload(reader: &mut BufReader<io::Take<&TcpStream>>, length: u64, name: String) -> Reply {
    let id = match reserve(length, &name) {
        Ok(id) => id,
        Err(e) => return Reply::refused(413, &e),
    };
    let read = body(reader, length).and_then(|bytes| {
        let bytes = Arc::new(bytes);
        let _job = one_at_a_time();
        let deadline = Instant::now() + TIME_LIMIT;
        guarded(|| inspect(&name, bytes.clone(), Some(deadline)))
            .map(|summary| (bytes, summary))
            .map_err(|e| Reply::refused(422, &e))
    });
    let mut state = state();
    match read {
        Ok((bytes, summary)) => {
            if let Some(file) = state.files.get_mut(&id) {
                file.bytes = Some(bytes);
            }
            let mut listed =
                json!({"id": id.to_string(), "name": name, "kind": summary.kind, "bytes": length});
            listed[summary.unit] = summary.count.into();
            Reply::json(&listed)
        }
        Err(reply) => {
            state.files.remove(&id);
            reply
        }
    }
}

/// Lists a file about to arrive, within the page's limits.
fn reserve(length: u64, name: &str) -> Result<u64, Error> {
    if length > MAX_FILE {
        let what = format!("'{name}'");
        return Err(Error::limit_exceeded(&what, &mib(length), "512 MiB"));
    }
    let mut state = state();
    if state.files.len() >= MAX_FILES {
        let what = "The number of files";
        return Err(Error::limit_exceeded(what, "more than 500", "500"));
    }
    let held: u64 = state.files.values().map(|f| f.size).sum();
    if held.saturating_add(length) > MAX_HELD {
        let what = "The files together";
        return Err(Error::limit_exceeded(
            what,
            &mib(held + length),
            "1,024 MiB",
        ));
    }
    let id = state.next_id;
    state.next_id += 1;
    state.files.insert(
        id,
        File {
            name: name.to_owned(),
            size: length,
            bytes: None,
        },
    );
    Ok(id)
}

/// A merge already running keeps its copy of the bytes.
fn forget(id: &str) -> Reply {
    if let Ok(id) = id.parse() {
        state().files.remove(&id);
    }
    Reply::new(204, JSON, [])
}

/// What the page sends to `/api/merge`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MergeRequest {
    files: Vec<String>,
    #[serde(default)]
    styles: StyleMode,
    name: Option<String>,
}

fn merge(reader: &mut BufReader<io::Take<&TcpStream>>, length: u64) -> Reply {
    if length > MAX_REQUEST {
        return Reply::request(413, "limit_exceeded", "The request is too large.");
    }
    let bytes = match body(reader, length) {
        Ok(bytes) => bytes,
        Err(reply) => return reply,
    };
    let request: MergeRequest = match serde_json::from_slice(&bytes) {
        Ok(request) => request,
        Err(e) => return Reply::bad(&format!("Can't read the request: {e}.")),
    };
    let _job = one_at_a_time();
    merged(request, Instant::now() + TIME_LIMIT)
}

/// Merges the listed files, holding the result for its download.
fn merged(request: MergeRequest, deadline: Instant) -> Reply {
    let files = match listed(&request.files) {
        Ok(files) => files,
        Err(e) => return Reply::refused(422, &e),
    };
    state().result = None;
    let name = request.name.as_deref().map(last_component);
    let options = Options {
        styles: request.styles,
    };
    answer(guarded(|| {
        merge_uploads(&files, name, &options, Some(deadline))
    }))
}

/// The report, with the file held for `GET /api/result`, or the error object.
fn answer(merged: Result<(Report, Family, Vec<u8>), Error>) -> Reply {
    match merged {
        Ok((report, family, bytes)) => {
            state().result = Some(Merged {
                name: report.output.path.to_string_lossy().into_owned(),
                family,
                bytes: Arc::new(bytes),
            });
            Reply::json(&report)
        }
        Err(e) => Reply::refused(422, &e),
    }
}

/// The uploads with these ids, in order.
fn listed(ids: &[String]) -> Result<Vec<(String, Upload)>, Error> {
    match ids.len() {
        0 | 1 => return Err(Error::need_two_files(ids.len())),
        n if n > MAX_FILES => {
            let what = "The number of files";
            return Err(Error::limit_exceeded(what, &n.to_string(), "500"));
        }
        _ => {}
    }
    let state = state();
    ids.iter()
        .map(|id| {
            let file = id.parse().ok().and_then(|id| state.files.get(&id));
            file.and_then(|f| Some((f.name.clone(), f.bytes.clone()?)))
                .ok_or_else(|| Error::file_not_found(id))
        })
        .collect()
}

/// The held result, or 404.
fn result() -> Reply {
    let state = state();
    let Some(merged) = &state.result else {
        return Reply::not_found();
    };
    let kind = match merged.family {
        Family::Word => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        Family::PowerPoint => {
            "application/vnd.openxmlformats-officedocument.presentationml.presentation"
        }
        Family::Excel => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
    };
    Reply {
        status: 200,
        kind,
        headers: format!(
            "Content-Disposition: attachment; filename*=UTF-8''{}\r\n",
            escape(&merged.name)
        ),
        body: merged.bytes.clone(),
    }
}

/// A name's last component, so that a name is never a path.
fn last_component(name: &str) -> &str {
    name.rsplit(['/', '\\']).next().unwrap_or_default()
}

/// The shown name of an `X-File-Name`: percent-decoded UTF-8, its last path component only.
fn file_name(header: &str) -> Option<String> {
    let name = unescape(header)?;
    let name = last_component(&name);
    Path::new(name).file_stem()?;
    Some(name.to_owned())
}

/// Percent-decodes `s` as UTF-8.
fn unescape(s: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(s.len());
    let mut rest = s.as_bytes();
    while let Some((&b, tail)) = rest.split_first() {
        rest = tail;
        if b == b'%' {
            let hex = tail
                .get(..2)
                .filter(|h| h.iter().all(u8::is_ascii_hexdigit))?;
            bytes.push(u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?);
            rest = &tail[2..];
        } else {
            bytes.push(b);
        }
    }
    String::from_utf8(bytes).ok()
}

/// Percent-encodes `s` as `encodeURIComponent` does, which also suits a header's `filename*`.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out += &format!("%{b:02X}");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;

    fn code(reply: &Reply) -> String {
        let e: Value = serde_json::from_slice(&reply.body).unwrap();
        e["error"]["code"].as_str().unwrap().to_string()
    }

    fn held(name: &str) -> u64 {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/a.docx");
        let bytes = std::fs::read(path).unwrap();
        let id = reserve(bytes.len() as u64, name).unwrap();
        state().files.get_mut(&id).unwrap().bytes = Some(Arc::new(bytes));
        id
    }

    fn request(ids: &[u64]) -> MergeRequest {
        MergeRequest {
            files: ids.iter().map(ToString::to_string).collect(),
            styles: StyleMode::Keep,
            name: None,
        }
    }

    #[test]
    fn a_merge_past_its_deadline_answers_timeout() {
        let ids = [held("a.docx"), held("b.docx")];
        let reply = merged(request(&ids), Instant::now());
        assert_eq!((reply.status, code(&reply).as_str()), (422, "timeout"));
    }

    #[test]
    fn a_panic_answers_internal_and_the_next_merge_still_works() {
        let reply = answer(guarded(|| panic!("boom")));
        assert_eq!((reply.status, code(&reply).as_str()), (422, "internal"));
        let ids = [held("a.docx"), held("b.docx")];
        let reply = merged(request(&ids), Instant::now() + TIME_LIMIT);
        assert_eq!(reply.status, 200);
    }

    #[test]
    fn file_names_are_percent_decoded_and_lose_their_folders() {
        let name = |s| file_name(s);
        assert_eq!(
            name("Gr%C3%BC%C3%9Fe%20a.pptx").as_deref(),
            Some("Grüße a.pptx")
        );
        assert_eq!(name("..%2F..%2Fa.pptx").as_deref(), Some("a.pptx"));
        assert_eq!(name("C%3A%5Cx%5Ca.pptx").as_deref(), Some("a.pptx"));
        for bad in ["", "%", "%4", "%zz", "%+1", "%C3", "x%2F"] {
            assert_eq!(name(bad), None, "{bad}");
        }
    }

    #[test]
    fn escaping_round_trips_and_leaves_no_header_breaks() {
        let s = "Grüße \"a\"\r\n%.docx";
        let e = escape(s);
        assert!(e.is_ascii() && !e.contains(['"', '\r', '\n', ' ']), "{e}");
        assert_eq!(unescape(&e).as_deref(), Some(s));
    }

    #[test]
    fn headers_are_tokens_and_utf_8_values() {
        assert_eq!(header(b"Host: a:1\r\n"), Some(("Host", "a:1")));
        assert_eq!(header(b"X:\n"), Some(("X", "")));
        for bad in [
            &b"Host : a\r\n"[..],
            b" folded: a\r\n",
            b"no colon\r\n",
            b": a\r\n",
            b"X: \xFF\r\n",
            b"X: a\rb\r\n",
            b"X: a\0\r\n",
            b"X: a",
        ] {
            assert_eq!(header(bad), None, "{bad:?}");
        }
    }
}
