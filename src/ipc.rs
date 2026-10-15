//! IPC server: a Unix socket speaking JSON Lines (see `healthd-proto`).
//!
//! Frontends are pluggable because this is the *only* way to read data out of
//! the daemon — there is no other interface to the database. The protocol is
//! read-only; a client can ask questions but never change state.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use std::{fs, thread};

use healthd_proto::{Request, Response, MAX_LINE};

use crate::poll::Hub;

/// Maximum simultaneous client connections.
///
/// WHY 8: one human with a TUI, a CLI and a couple of scripts. A cap bounds
/// the memory a runaway client loop can consume.
const MAX_CONNECTIONS: usize = 8;

/// Stack size of a connection thread (see `poll::STACK_BYTES` for the idea).
///
/// WHY 256 KiB: a handler holds one request line (at most 4 KiB) and one
/// reply, so it touches a tiny part of the 2 MiB default; with up to
/// `MAX_CONNECTIONS` threads the smaller reservation keeps virtual size honest.
const CONN_STACK: usize = 256 * 1024;

/// Close a connection that sends nothing for this long.
///
/// WHY 60 s: the TUI polls every couple of seconds, so a live client is never
/// idle that long; a stuck one would otherwise hold a connection slot forever.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// Give up writing a reply after this long (client stopped reading).
///
/// WHY 5 s: even the largest reply (a few hundred KB) goes through a local
/// socket in milliseconds, so 5 s only trips on a client that stopped reading,
/// and then its thread and connection slot must be freed.
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

/// Pause after a failed `accept`, so e.g. fd exhaustion cannot spin the CPU.
///
/// WHY 200 ms: short enough that a transient error clears before anyone
/// notices (clients just retry), long enough to cap the loop at 5 wake-ups/s.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(200);

/// Releases a connection slot when dropped (even if the handler panics).
pub(crate) struct SlotGuard(Arc<AtomicUsize>);

impl Drop for SlotGuard {
    /// Give the slot back.
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Claim one of `max` connection slots, or `None` when all are taken.
///
/// Shared with the web server so both listeners cap concurrent clients the
/// same way. The slot is released when the returned guard is dropped.
pub(crate) fn try_acquire(active: &Arc<AtomicUsize>, max: usize) -> Option<SlotGuard> {
    if active.fetch_add(1, Ordering::AcqRel) >= max {
        active.fetch_sub(1, Ordering::AcqRel);
        return None;
    }
    Some(SlotGuard(Arc::clone(active)))
}

/// Bind the socket and serve clients forever; only returns on a fatal error.
pub fn serve(hub: &Arc<Hub>, path: &Path) -> io::Result<()> {
    let listener = bind(path)?;
    let active = Arc::new(AtomicUsize::new(0));
    // Blocking accept: zero CPU while no client is connected.
    for conn in listener.incoming() {
        let stream = match conn {
            Ok(s) => s,
            Err(e) => {
                eprintln!("healthd: accept failed: {e}");
                thread::sleep(ACCEPT_BACKOFF);
                continue;
            }
        };
        let Some(slot) = try_acquire(&active, MAX_CONNECTIONS) else {
            reject(stream);
            continue;
        };
        let hub = Arc::clone(hub);
        let spawned = thread::Builder::new()
            .name("ipc-conn".to_owned())
            .stack_size(CONN_STACK)
            .spawn(move || {
                let _slot = slot;
                handle_conn(&hub, stream);
            });
        if let Err(e) = spawned {
            // The closure (and its SlotGuard) is dropped, freeing the slot.
            eprintln!("healthd: cannot spawn connection thread: {e}");
        }
    }
    Ok(())
}

/// Create the socket directory/file with owner-only permissions.
///
/// A stale socket from a crashed run is removed; a live one means another
/// daemon is running, which is an error.
fn bind(path: &Path) -> io::Result<UnixListener> {
    if let Some(dir) = path.parent() {
        // 0700 on the directory is what actually keeps other users out.
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
    }
    if fs::symlink_metadata(path).is_ok() {
        if UnixStream::connect(path).is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!("another healthd is already listening on {}", path.display()),
            ));
        }
        fs::remove_file(path)?;
    }
    let listener = UnixListener::bind(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// Tell an over-limit client to come back later.
fn reject(mut stream: UnixStream) {
    let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
    write_response(&mut stream, &Response::failure("too many connections"));
}

/// Serialise `resp` plus a newline onto `out`; errors just end the connection.
fn write_response(out: &mut impl Write, resp: &Response) -> bool {
    let Ok(mut bytes) = serde_json::to_vec(resp) else {
        return false;
    };
    bytes.push(b'\n');
    out.write_all(&bytes).and_then(|()| out.flush()).is_ok()
}

/// Serve one client until it disconnects, idles out, or misbehaves.
fn handle_conn(hub: &Hub, stream: UnixStream) {
    if stream.set_read_timeout(Some(IDLE_TIMEOUT)).is_err()
        || stream.set_write_timeout(Some(WRITE_TIMEOUT)).is_err()
    {
        return;
    }
    let Ok(mut writer) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        // `take` caps how much one request line may allocate.
        match reader.by_ref().take(MAX_LINE as u64).read_line(&mut line) {
            Ok(0) | Err(_) => return, // EOF, timeout, or invalid UTF-8
            Ok(_) => {}
        }
        if !line.ends_with('\n') {
            // Either the line hit the cap or the client closed mid-request.
            if line.len() >= MAX_LINE {
                write_response(&mut writer, &Response::failure("request too long"));
            }
            return;
        }
        let resp = match serde_json::from_str::<Request>(line.trim()) {
            Ok(req) => dispatch(hub, req),
            Err(e) => Response::failure(format!("bad request: {e}")),
        };
        if !write_response(&mut writer, &resp) {
            return;
        }
    }
}

/// Execute one request against the hub.
///
/// `pub(crate)` because the web dashboard's `/api/*` routes run the very same
/// function: there is one API, with two transports.
pub(crate) fn dispatch(hub: &Hub, req: Request) -> Response {
    match req {
        Request::Status => Response::success(&hub.status()),
        Request::History {
            source,
            metric,
            since,
            limit,
        } => match hub.history(&source, &metric, since, limit) {
            Ok(data) => Response::success(&data),
            Err(e) => Response::failure(format!("history query failed: {e}")),
        },
        Request::Events {
            since,
            limit,
            min_severity,
        } => match hub.events(since, limit, min_severity) {
            Ok(data) => Response::success(&data),
            Err(e) => Response::failure(format!("events query failed: {e}")),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::db::Db;
    use healthd_proto::{Client, Severity};

    /// Full round trip over a real socket using the public client.
    #[test]
    fn serves_status_history_and_events() {
        let mut cfg = Config::builtin().unwrap();
        cfg.notify.desktop = false;
        let hub = Hub::new(Db::open_in_memory(), &cfg);
        hub.record("cpu", &[("temp_c", 61.0)], true);
        hub.log_event(Severity::Warn, "cpu", "hello");

        let dir = std::env::temp_dir().join(format!("healthd-ipc-test-{}", std::process::id()));
        let sock = dir.join("t.sock");
        let (h, s) = (Arc::clone(&hub), sock.clone());
        thread::spawn(move || {
            let _ = serve(&h, &s);
        });
        let mut client = (0..200)
            .find_map(|_| {
                Client::connect(&sock).ok().or_else(|| {
                    thread::sleep(Duration::from_millis(10));
                    None
                })
            })
            .expect("server did not come up");

        let status = client.status().unwrap();
        assert_eq!(status.metrics[0].metric, "temp_c");
        assert!(status
            .rules
            .iter()
            .any(|r| r.name == "cpu-hot" && r.op == ">" && r.threshold == 85.0));
        let hist = client.history("cpu", "temp_c", Some(0), None).unwrap();
        assert_eq!(hist.points.len(), 1);
        let ev = client.events(Some(0), None, Some(Severity::Warn)).unwrap();
        assert_eq!(ev.events[0].msg, "hello");

        // An unknown metric is not an error: it is simply an empty series, and
        // the connection stays usable afterwards.
        let empty = client
            .history("cpu", "no_such_metric", Some(0), None)
            .unwrap();
        assert!(empty.points.is_empty());
        assert!(client.status().is_ok());
        let _ = fs::remove_dir_all(&dir);
    }

    /// Garbage input yields `ok:false` and keeps the connection usable.
    #[test]
    fn malformed_requests_get_error_replies() {
        let mut cfg = Config::builtin().unwrap();
        cfg.notify.desktop = false;
        let hub = Hub::new(Db::open_in_memory(), &cfg);
        let (a, mut b) = UnixStream::pair().unwrap();
        let h = Arc::clone(&hub);
        thread::spawn(move || handle_conn(&h, a));
        b.write_all(b"not json\n{\"cmd\":\"status\"}\n").unwrap();
        let mut reader = BufReader::new(b);
        let mut first = String::new();
        reader.read_line(&mut first).unwrap();
        assert!(first.contains(r#""ok":false"#));
        let mut second = String::new();
        reader.read_line(&mut second).unwrap();
        assert!(second.contains(r#""ok":true"#));
    }
}
