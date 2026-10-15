//! Embedded web dashboard: a minimal, read-only HTTP/1.1 server on loopback.
//!
//! The dashboard is one more *frontend* of the daemon, so `/api/*` is not a new
//! interface: each route builds the same [`Request`] a socket client would send
//! and runs it through [`ipc::dispatch`], returning the very same `{ok,data}`
//! JSON. Everything else is the Svelte build in `web/dist`, compiled into the
//! binary with `include_bytes!`, so there are no files to install.
//!
//! Like the rest of the daemon this is plain `std`: one thread blocked in
//! `accept` (0 % CPU when nobody is looking) and one short-lived thread per
//! request. Every response closes the connection, which keeps the parser
//! trivial: there is no pipelining, no keep-alive state and no request bodies.
//!
//! ## Why it is safe to run without a login
//!
//! * It listens on `127.0.0.1` only, and the address is not configurable.
//! * Only `GET` is served, so nothing can be changed through it.
//! * The `Host` header must name the loopback address and our port. That stops
//!   *DNS rebinding*, where a web page on `evil.example` re-points its own name
//!   at 127.0.0.1 and then reads the API from the victim's browser.
//! * No CORS headers are sent and responses are `Cross-Origin-Resource-Policy:
//!   same-origin`, so other sites cannot read them either.
//!
//! One thing this does *not* do is keep other users of the same machine out:
//! any local account can open the port. The Unix socket is `0600`; this is not.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::str::FromStr;
use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use healthd_proto::{Request, Response, Severity};

use crate::ipc;
use crate::poll::Hub;

/// Maximum simultaneous connections.
///
/// WHY 16: a browser keeps at most 6 connections per origin, so two open tabs
/// plus a `curl` loop fit comfortably, while a runaway client still cannot
/// make the daemon spawn threads without bound.
const MAX_CONNECTIONS: usize = 16;

/// Stack size of the accept thread and of every request thread.
///
/// WHY 256 KiB: a request is parsed from a few KB of text and answered from a
/// prebuilt buffer, so the 2 MiB default would be address space we never touch.
const THREAD_STACK: usize = 256 * 1024;

/// Give up on a request that is not fully received, or a reply that is not
/// fully accepted, after this long.
///
/// WHY 5 s: a browser on loopback sends its request in one packet and reads the
/// reply immediately, so 5 s only trips on a stalled or hostile client, whose
/// connection slot and thread must then be freed.
const IO_TIMEOUT: Duration = Duration::from_secs(5);

/// Largest request head (request line plus headers) we will read.
///
/// WHY 16 KiB: browsers send ~1 KB, but cookies are shared by every service on
/// `localhost` regardless of port, so a developer's browser can legitimately
/// carry several KB. 16 KiB leaves room for that and still bounds memory.
const MAX_HEAD: usize = 16 * 1024;

/// Pause after a failed `accept`, so e.g. fd exhaustion cannot spin the CPU.
///
/// WHY 200 ms: the same as the socket server: short enough that a transient
/// error clears unnoticed, long enough to cap the loop at 5 wake-ups a second.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(200);

/// Headers added to every response.
///
/// The CSP allows only our own files plus `data:` images, and forbids framing,
/// base-tag tricks and form posts. The page has no inline script or style, so
/// nothing needs `unsafe-inline`.
const SECURITY_HEADERS: &str = "X-Content-Type-Options: nosniff\r\n\
    Referrer-Policy: no-referrer\r\n\
    Cross-Origin-Resource-Policy: same-origin\r\n\
    Content-Security-Policy: default-src 'none'; script-src 'self'; style-src 'self'; \
    img-src 'self' data:; connect-src 'self'; base-uri 'none'; form-action 'none'; \
    frame-ancestors 'none'\r\n";

/// One embedded file of the dashboard build.
struct Asset {
    /// URL path it is served at.
    path: &'static str,
    /// `Content-Type` header value.
    mime: &'static str,
    /// File contents, compiled into the binary.
    body: &'static [u8],
}

/// Everything the dashboard needs, built by `make web` into `web/dist`.
///
/// The build output is committed, so a plain `cargo build` never needs Node.
/// Output names are fixed (no content hashes) so this table can be static; a
/// test checks that every file `index.html` references is listed here.
const ASSETS: [Asset; 4] = [
    Asset {
        path: "/",
        mime: "text/html; charset=utf-8",
        body: include_bytes!("../web/dist/index.html"),
    },
    Asset {
        path: "/assets/app.js",
        mime: "text/javascript; charset=utf-8",
        body: include_bytes!("../web/dist/assets/app.js"),
    },
    Asset {
        path: "/assets/app.css",
        mime: "text/css; charset=utf-8",
        body: include_bytes!("../web/dist/assets/app.css"),
    },
    Asset {
        path: "/favicon.svg",
        mime: "image/svg+xml",
        body: include_bytes!("../web/dist/favicon.svg"),
    },
];

/// Look an embedded file up by URL path (`/index.html` is an alias of `/`).
fn asset(path: &str) -> Option<&'static Asset> {
    let path = if path == "/index.html" { "/" } else { path };
    ASSETS.iter().find(|a| a.path == path)
}

/// Bind `127.0.0.1:port` and serve the dashboard from a background thread.
///
/// Returns the bound address (useful with port 0 in tests). Binding happens
/// *here*, on the caller's thread, so a port that is already taken is reported
/// to the caller instead of being lost inside a thread.
pub fn start(hub: &Arc<Hub>, port: u16) -> io::Result<SocketAddr> {
    // Loopback only: see the module docs. This is deliberately not a setting.
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))?;
    let addr = listener.local_addr()?;
    let hub = Arc::clone(hub);
    thread::Builder::new()
        .name("web".to_owned())
        .stack_size(THREAD_STACK)
        .spawn(move || accept_loop(&hub, &listener, addr.port()))?;
    Ok(addr)
}

/// Accept connections forever, one short-lived thread each.
fn accept_loop(hub: &Arc<Hub>, listener: &TcpListener, port: u16) {
    let active = Arc::new(AtomicUsize::new(0));
    // Blocking accept: zero CPU while nobody has the dashboard open.
    for conn in listener.incoming() {
        let mut stream = match conn {
            Ok(s) => s,
            Err(e) => {
                eprintln!("healthd: web accept failed: {e}");
                thread::sleep(ACCEPT_BACKOFF);
                continue;
            }
        };
        let Some(slot) = ipc::try_acquire(&active, MAX_CONNECTIONS) else {
            let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
            let _ = Reply::text(503, "too many connections\n").write_to(&mut stream);
            continue;
        };
        let hub = Arc::clone(hub);
        let spawned = thread::Builder::new()
            .name("web-conn".to_owned())
            .stack_size(THREAD_STACK)
            .spawn(move || {
                let _slot = slot;
                handle(&hub, stream, port);
            });
        if let Err(e) = spawned {
            // The closure (and its slot guard) is dropped, freeing the slot.
            eprintln!("healthd: cannot spawn web connection thread: {e}");
        }
    }
}

/// Why reading a request head failed.
enum HeadError {
    /// The head exceeded [`MAX_HEAD`].
    TooLarge,
    /// The client vanished, timed out or sent garbage; nothing to answer.
    Closed,
}

/// Read up to the blank line that ends the request head.
fn read_head(stream: &mut TcpStream) -> Result<String, HeadError> {
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut chunk = [0_u8; 1024];
    loop {
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            buf.truncate(end);
            return String::from_utf8(buf).map_err(|_| HeadError::Closed);
        }
        if buf.len() > MAX_HEAD {
            return Err(HeadError::TooLarge);
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return Err(HeadError::Closed),
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
}

/// Serve one connection: read the head, route it, write the reply.
fn handle(hub: &Hub, mut stream: TcpStream, port: u16) {
    if stream.set_read_timeout(Some(IO_TIMEOUT)).is_err()
        || stream.set_write_timeout(Some(IO_TIMEOUT)).is_err()
    {
        return;
    }
    let reply = match read_head(&mut stream) {
        Ok(head) => route(hub, &head, port),
        Err(HeadError::TooLarge) => Reply::text(431, "request header too large\n"),
        Err(HeadError::Closed) => return,
    };
    // A write error means the client left; there is nobody to tell.
    let _ = reply.write_to(&mut stream);
}

/// The interesting parts of a request head.
struct Head<'a> {
    method: &'a str,
    target: &'a str,
    host: Option<&'a str>,
}

/// Parse the request line and find the `Host` header.
fn parse_head(head: &str) -> Option<Head<'_>> {
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let (method, target, version) = (first.next()?, first.next()?, first.next()?);
    if first.next().is_some() || !version.starts_with("HTTP/1.") {
        return None;
    }
    let host = lines.find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("host").then(|| value.trim())
    });
    Some(Head {
        method,
        target,
        host,
    })
}

/// Is `host` (a `Host` header value) our own loopback address?
///
/// This is the DNS-rebinding defence: a rebound request carries the attacker's
/// name in `Host`, so it is refused no matter which IP it reached.
fn host_allowed(host: Option<&str>, port: u16) -> bool {
    let Some(host) = host else {
        return false;
    };
    let host = host.to_ascii_lowercase();
    [
        format!("127.0.0.1:{port}"),
        format!("localhost:{port}"),
        format!("[::1]:{port}"),
    ]
    .contains(&host)
}

/// A complete response, ready to be written.
struct Reply {
    status: u16,
    mime: &'static str,
    cache: &'static str,
    body: Cow<'static, [u8]>,
    /// Extra header lines, each ending in `\r\n`.
    extra: &'static str,
}

impl Reply {
    /// A short plain-text reply (errors).
    fn text(status: u16, body: &'static str) -> Self {
        Self {
            status,
            mime: "text/plain; charset=utf-8",
            cache: "no-store",
            body: Cow::Borrowed(body.as_bytes()),
            extra: "",
        }
    }

    /// An API reply: the daemon's `{ok,data}` / `{ok:false,error}` envelope.
    fn json(status: u16, resp: &Response) -> Self {
        let body = serde_json::to_vec(resp)
            .unwrap_or_else(|_| br#"{"ok":false,"error":"serialisation failed"}"#.to_vec());
        Self {
            status,
            mime: "application/json",
            // Live data: a cached reply would show stale health as current.
            cache: "no-store",
            body: Cow::Owned(body),
            extra: "",
        }
    }

    /// An embedded file.
    fn asset(a: &'static Asset) -> Self {
        Self {
            status: 200,
            mime: a.mime,
            // `no-cache` = always revalidate. File names carry no hash, so
            // after an upgrade the browser must not keep serving the old build.
            cache: "no-cache",
            body: Cow::Borrowed(a.body),
            extra: "",
        }
    }

    /// Reason phrase for the status line.
    fn reason(&self) -> &'static str {
        match self.status {
            200 => "OK",
            400 => "Bad Request",
            403 => "Forbidden",
            404 => "Not Found",
            405 => "Method Not Allowed",
            431 => "Request Header Fields Too Large",
            503 => "Service Unavailable",
            _ => "Internal Server Error",
        }
    }

    /// Serialise the status line, headers and body onto `out`.
    fn write_to(&self, out: &mut impl Write) -> io::Result<()> {
        let head = format!(
            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\n\
             Cache-Control: {}\r\n{}Connection: close\r\n{SECURITY_HEADERS}\r\n",
            self.status,
            self.reason(),
            self.mime,
            self.body.len(),
            self.cache,
            self.extra,
        );
        out.write_all(head.as_bytes())?;
        out.write_all(&self.body)?;
        out.flush()
    }
}

/// Turn a request head into a reply.
///
/// Pure apart from reading the hub, so the tests can call it without sockets.
fn route(hub: &Hub, head: &str, port: u16) -> Reply {
    let Some(h) = parse_head(head) else {
        return Reply::text(400, "malformed request\n");
    };
    if !host_allowed(h.host, port) {
        return Reply::text(403, "forbidden host\n");
    }
    if h.method != "GET" {
        return Reply {
            extra: "Allow: GET\r\n",
            ..Reply::text(405, "only GET is supported\n")
        };
    }
    let (path, query) = h.target.split_once('?').unwrap_or((h.target, ""));
    if let Some(name) = path.strip_prefix("/api/") {
        return api_reply(hub, name, query);
    }
    match asset(path) {
        Some(a) => Reply::asset(a),
        None => Reply::text(404, "not found\n"),
    }
}

/// Why an API request could not be built.
enum ApiError {
    /// No such endpoint.
    NotFound,
    /// A parameter is missing, malformed or unknown.
    Bad(String),
}

impl From<String> for ApiError {
    /// Parameter errors are plain messages.
    fn from(msg: String) -> Self {
        Self::Bad(msg)
    }
}

/// Run an `/api/<name>` request through the shared dispatcher.
fn api_reply(hub: &Hub, name: &str, query: &str) -> Reply {
    let req = match build_request(name, query) {
        Ok(r) => r,
        Err(ApiError::NotFound) => {
            return Reply::json(404, &Response::failure("unknown endpoint"));
        }
        Err(ApiError::Bad(msg)) => return Reply::json(400, &Response::failure(msg)),
    };
    let resp = ipc::dispatch(hub, req);
    Reply::json(if resp.ok { 200 } else { 500 }, &resp)
}

/// Map `/api/<name>?<query>` to the socket protocol's [`Request`].
///
/// The parameters are the request's own field names (`source`, `metric`,
/// `since`, `limit`, `min_severity`). Unknown parameters are rejected rather
/// than ignored, so a typo like `?sence=` fails loudly instead of silently
/// returning the default window.
fn build_request(name: &str, query: &str) -> Result<Request, ApiError> {
    let mut q = Query::parse(query)?;
    let req = match name {
        "status" => Request::Status,
        "history" => Request::History {
            source: q.required("source")?,
            metric: q.required("metric")?,
            since: q.number("since")?,
            limit: q.number("limit")?,
        },
        "events" => Request::Events {
            since: q.number("since")?,
            limit: q.number("limit")?,
            min_severity: q
                .take("min_severity")
                .map(|s| Severity::parse(&s).ok_or("min_severity must be info, warn or crit"))
                .transpose()
                .map_err(str::to_owned)?,
        },
        _ => return Err(ApiError::NotFound),
    };
    q.finish()?;
    Ok(req)
}

/// A decoded query string whose parameters are consumed one by one.
struct Query(BTreeMap<String, String>);

impl Query {
    /// Split `a=1&b=2` and percent-decode both halves; the last duplicate wins.
    fn parse(query: &str) -> Result<Self, String> {
        let mut map = BTreeMap::new();
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            map.insert(percent_decode(k)?, percent_decode(v)?);
        }
        Ok(Self(map))
    }

    /// Remove and return a parameter.
    fn take(&mut self, key: &str) -> Option<String> {
        self.0.remove(key)
    }

    /// Remove a parameter that must be present and non-empty.
    fn required(&mut self, key: &str) -> Result<String, String> {
        self.take(key)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| format!("missing parameter '{key}'"))
    }

    /// Remove and parse an optional numeric parameter.
    fn number<T: FromStr>(&mut self, key: &str) -> Result<Option<T>, String> {
        self.take(key)
            .map(|v| {
                v.parse()
                    .map_err(|_| format!("parameter '{key}' must be an integer"))
            })
            .transpose()
    }

    /// Fail if any parameter was not consumed.
    fn finish(self) -> Result<(), String> {
        match self.0.keys().next() {
            Some(k) => Err(format!("unknown parameter '{k}'")),
            None => Ok(()),
        }
    }
}

/// Decode `%XX` escapes and `+` (space) in a query component.
fn percent_decode(s: &str) -> Result<String, String> {
    /// Value of one hex digit.
    fn hex(b: u8) -> Option<u8> {
        char::from(b)
            .to_digit(16)
            .and_then(|d| u8::try_from(d).ok())
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hi = bytes.get(i + 1).copied().and_then(hex);
                let lo = bytes.get(i + 2).copied().and_then(hex);
                let (Some(hi), Some(lo)) = (hi, lo) else {
                    return Err("bad %-escape in query".to_owned());
                };
                out.push(hi * 16 + lo);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).map_err(|_| "query is not valid UTF-8".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::db::Db;

    /// Port used when routing without a socket.
    const PORT: u16 = 8808;

    /// Hub on an in-memory database, notifications off, with one sample/event.
    fn test_hub() -> Arc<Hub> {
        let mut cfg = Config::builtin().unwrap();
        cfg.notify.desktop = false;
        let hub = Hub::new(Db::open_in_memory(), &cfg);
        hub.record("cpu", &[("temp_c", 61.0)], true);
        hub.log_event(Severity::Warn, "cpu", "hello");
        hub
    }

    /// A GET head with a valid Host.
    fn get(target: &str) -> String {
        format!("GET {target} HTTP/1.1\r\nHost: 127.0.0.1:{PORT}\r\nAccept: */*\r\n\r\n")
    }

    /// The reply body as text.
    fn body(r: &Reply) -> String {
        String::from_utf8_lossy(&r.body).into_owned()
    }

    /// The page and its files are served with the right types and no caching.
    #[test]
    fn serves_embedded_files() {
        let hub = test_hub();
        for (path, mime) in [
            ("/", "text/html"),
            ("/index.html", "text/html"),
            ("/assets/app.js", "text/javascript"),
            ("/assets/app.css", "text/css"),
            ("/favicon.svg", "image/svg+xml"),
            ("/?utm=1", "text/html"),
        ] {
            let r = route(&hub, &get(path), PORT);
            assert_eq!(r.status, 200, "{path}");
            assert!(r.mime.starts_with(mime), "{path}: {}", r.mime);
            assert!(!r.body.is_empty(), "{path} is empty");
            assert_eq!(r.cache, "no-cache");
        }
        assert!(body(&route(&hub, &get("/"), PORT)).contains("<div id=\"app\">"));
        assert_eq!(route(&hub, &get("/assets/nope.js"), PORT).status, 404);
        assert_eq!(route(&hub, &get("/../Cargo.toml"), PORT).status, 404);
    }

    /// Every file the built `index.html` points at is actually embedded, so a
    /// rebuild that renames outputs cannot ship a page with dead links.
    #[test]
    fn index_references_only_embedded_files() {
        let html = String::from_utf8_lossy(asset("/").unwrap().body).into_owned();
        let mut refs = 0;
        for attr in ["src=\"", "href=\""] {
            for part in html.split(attr).skip(1) {
                let url = part.split('"').next().unwrap();
                if url.starts_with('/') {
                    refs += 1;
                    assert!(asset(url).is_some(), "index.html references {url}");
                }
            }
        }
        assert!(
            refs >= 3,
            "expected script, stylesheet and icon, got {refs}"
        );
    }

    /// `/api/*` returns the socket API's envelope.
    #[test]
    fn api_matches_the_socket_protocol() {
        let hub = test_hub();
        let r = route(&hub, &get("/api/status"), PORT);
        assert_eq!(
            (r.status, r.mime, r.cache),
            (200, "application/json", "no-store")
        );
        let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["data"]["metrics"][0]["metric"], "temp_c");
        assert!(v["data"]["rules"].as_array().is_some_and(|a| !a.is_empty()));

        let r = route(
            &hub,
            &get("/api/history?source=cpu&metric=temp_c&since=0&limit=5"),
            PORT,
        );
        let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(v["data"]["points"].as_array().unwrap().len(), 1);

        let r = route(&hub, &get("/api/events?since=0&min_severity=warn"), PORT);
        let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(v["data"]["events"][0]["msg"], "hello");
    }

    /// Bad input is a 400 with the daemon's error shape, never a panic.
    #[test]
    fn api_rejects_bad_requests() {
        let hub = test_hub();
        for (target, needle) in [
            ("/api/history", "missing parameter 'source'"),
            ("/api/history?source=cpu", "missing parameter 'metric'"),
            (
                "/api/history?source=cpu&metric=x&since=yesterday",
                "'since'",
            ),
            ("/api/history?source=cpu&metric=x&limit=-1", "'limit'"),
            (
                "/api/history?source=cpu&metric=x&sence=1",
                "unknown parameter 'sence'",
            ),
            ("/api/status?x=1", "unknown parameter 'x'"),
            ("/api/events?min_severity=loud", "min_severity"),
            ("/api/events?since=%zz", "%-escape"),
        ] {
            let r = route(&hub, &get(target), PORT);
            assert_eq!(r.status, 400, "{target}");
            let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
            assert_eq!(v["ok"], false);
            assert!(
                v["error"].as_str().unwrap().contains(needle),
                "{target}: {v}"
            );
        }
        assert_eq!(route(&hub, &get("/api/nope"), PORT).status, 404);
    }

    /// Only our own loopback name and port may be used (DNS-rebinding defence).
    #[test]
    fn host_header_is_enforced() {
        let hub = test_hub();
        let with = |host: &str| format!("GET /api/status HTTP/1.1\r\nHost: {host}\r\n\r\n");
        for ok in [
            "127.0.0.1:8808",
            "localhost:8808",
            "LOCALHOST:8808",
            "[::1]:8808",
        ] {
            assert_eq!(route(&hub, &with(ok), PORT).status, 200, "{ok}");
        }
        for bad in [
            "evil.example",
            "evil.example:8808",
            "127.0.0.1",
            "127.0.0.1:9999",
            "127.0.0.1:8808.evil.example",
            "localhost.evil.example:8808",
            "",
        ] {
            assert_eq!(route(&hub, &with(bad), PORT).status, 403, "{bad:?}");
        }
        let no_host = "GET /api/status HTTP/1.1\r\n\r\n";
        assert_eq!(route(&hub, no_host, PORT).status, 403);
    }

    /// Everything except GET is refused, and garbage is a 400.
    #[test]
    fn methods_and_malformed_heads() {
        let hub = test_hub();
        for method in ["POST", "PUT", "DELETE", "OPTIONS", "HEAD"] {
            let head = format!("{method} / HTTP/1.1\r\nHost: 127.0.0.1:{PORT}\r\n\r\n");
            let r = route(&hub, &head, PORT);
            assert_eq!(r.status, 405, "{method}");
            assert_eq!(r.extra, "Allow: GET\r\n");
        }
        for junk in ["", "GET", "GET /", "GET / HTTP/2.0", "GET / HTTP/1.1 extra"] {
            assert_eq!(route(&hub, junk, PORT).status, 400, "{junk:?}");
        }
    }

    /// Percent-decoding handles escapes, `+`, and rejects broken input.
    #[test]
    fn percent_decoding() {
        assert_eq!(percent_decode("nvme0").unwrap(), "nvme0");
        assert_eq!(percent_decode("a%20b+c%2Fd").unwrap(), "a b c/d");
        assert_eq!(percent_decode("%C3%A9").unwrap(), "é");
        assert!(percent_decode("%").is_err());
        assert!(percent_decode("%4").is_err());
        assert!(percent_decode("%+1").is_err());
        assert!(percent_decode("%ff").is_err()); // lone 0xFF is not UTF-8
    }

    /// Full round trip over a real TCP socket, including the security headers
    /// and the oversized-head limit.
    #[test]
    fn real_socket_round_trip() {
        let hub = test_hub();
        let addr = start(&hub, 0).unwrap();
        assert!(addr.ip().is_loopback());

        let ask = |raw: &str| -> String {
            let mut s = TcpStream::connect(addr).unwrap();
            s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            s.write_all(raw.as_bytes()).unwrap();
            let mut out = String::new();
            // `Connection: close` means the server ends the stream for us.
            let _ = s.read_to_string(&mut out);
            out
        };
        let host = format!("127.0.0.1:{}", addr.port());

        let page = ask(&format!("GET / HTTP/1.1\r\nHost: {host}\r\n\r\n"));
        assert!(page.starts_with("HTTP/1.1 200 OK\r\n"), "{page}");
        for header in [
            "Content-Security-Policy: default-src 'none'",
            "X-Content-Type-Options: nosniff",
            "Cross-Origin-Resource-Policy: same-origin",
            "Connection: close",
        ] {
            assert!(page.contains(header), "missing {header}");
        }
        assert!(!page.contains("Access-Control-Allow-Origin"));

        let api = ask(&format!("GET /api/status HTTP/1.1\r\nHost: {host}\r\n\r\n"));
        assert!(api.contains("\r\n\r\n{\"ok\":true"), "{api}");

        let rebound = ask("GET /api/status HTTP/1.1\r\nHost: evil.example\r\n\r\n");
        assert!(rebound.starts_with("HTTP/1.1 403 Forbidden"), "{rebound}");

        let huge = format!(
            "GET / HTTP/1.1\r\nHost: {host}\r\nX-Pad: {}\r\n\r\n",
            "a".repeat(MAX_HEAD + 2048)
        );
        assert!(ask(&huge).starts_with("HTTP/1.1 431"), "oversized head");
    }

    /// A taken port is an error for the caller, not a crash or a silent no-op.
    #[test]
    fn port_in_use_is_reported() {
        let hub = test_hub();
        let first = start(&hub, 0).unwrap();
        assert!(start(&hub, first.port()).is_err());
    }
}
