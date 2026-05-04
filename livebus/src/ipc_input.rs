//! Local IPC ingest: workers push events into the bus over a Unix socket
//! (POSIX) or named pipe (Windows).
//!
//! The wire framing is **newline-delimited JSON** — one [`crate::event::Event`]
//! per line. This keeps the protocol trivial to generate from any language
//! (workers in Node, Python, etc. could publish too).
//!
//! Connection lifecycle:
//!
//! 1. Worker connects to the configured socket/pipe path.
//! 2. Worker writes one event per line, terminated by `\n`.
//! 3. livebus parses each line, forwards the `Event` to the
//!    [`SubscriberManager`] for fan-out, and records publish metrics.
//! 4. Either side may close the connection at any time. Errors are logged
//!    and the connection is dropped — no retry is performed by the server.
//!
//! ## Path conventions
//!
//! - Linux/macOS: `$XDG_RUNTIME_DIR/mneme/livebus.sock` (falls back to
//!   `/tmp/mneme-livebus.sock`).
//! - Windows: `\\.\pipe\mneme-livebus`.
//!
//! Use [`default_ipc_path`] to compute a sensible default.

use std::path::PathBuf;

use interprocess::local_socket::tokio::Stream as IpcStream;
use interprocess::local_socket::traits::tokio::Listener as ListenerT;
#[cfg(unix)]
use interprocess::local_socket::{GenericFilePath, ToFsName};
#[cfg(windows)]
use interprocess::local_socket::{GenericNamespaced, ToNsName};
use interprocess::local_socket::{ListenerOptions, Name};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tracing::{debug, error, info, warn};

use crate::error::LivebusError;
use crate::event::Event;
use crate::subscriber::SubscriberManager;

/// Hard cap on a single JSON line so a malicious peer can't OOM us.
pub const MAX_FRAME_BYTES: usize = 1024 * 1024; // 1 MiB

/// Best-effort default IPC path.
pub fn default_ipc_path() -> PathBuf {
    #[cfg(windows)]
    {
        PathBuf::from(r"\\.\pipe\mneme-livebus")
    }
    #[cfg(unix)]
    {
        if let Ok(rt) = std::env::var("XDG_RUNTIME_DIR") {
            let mut p = PathBuf::from(rt);
            p.push("mneme");
            let _ = std::fs::create_dir_all(&p);
            p.push("livebus.sock");
            p
        } else {
            PathBuf::from("/tmp/mneme-livebus.sock")
        }
    }
}

/// Bind the IPC listener on the given path and accept connections forever.
///
/// Returns when the listener fails to bind. Per-connection failures are
/// logged but never propagated — the listener loop is resilient.
pub async fn run_ipc_listener(path: PathBuf, mgr: SubscriberManager) -> Result<(), LivebusError> {
    // Best-effort cleanup of stale Unix socket files. Named pipes don't need
    // this on Windows.
    #[cfg(unix)]
    if path.exists() {
        if let Err(e) = std::fs::remove_file(&path) {
            warn!(?path, error = %e, "failed to clean stale ipc socket; continuing");
        }
    }

    let name = build_socket_name(&path)?;
    let listener = ListenerOptions::new()
        .name(name)
        .create_tokio()
        .map_err(LivebusError::Io)?;
    info!(?path, "livebus ipc listener bound");

    loop {
        let conn = match ListenerT::accept(&listener).await {
            Ok(c) => c,
            Err(e) => {
                error!(error = %e, "ipc accept failed");
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                continue;
            }
        };
        let mgr_for_conn = mgr.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(conn, mgr_for_conn).await {
                debug!(error = %e, "ipc connection ended with error");
            }
        });
    }
}

fn build_socket_name(path: &std::path::Path) -> Result<Name<'static>, LivebusError> {
    #[cfg(windows)]
    {
        let s = path.to_string_lossy().into_owned();
        let short = s.strip_prefix(r"\\.\pipe\").unwrap_or(&s).to_string();
        short
            .to_ns_name::<GenericNamespaced>()
            .map(|n| n.into_owned())
            .map_err(|e| LivebusError::Bind(format!("ipc name: {e}")))
    }
    #[cfg(unix)]
    {
        let owned = path.to_path_buf();
        owned
            .to_fs_name::<GenericFilePath>()
            .map(|n| n.into_owned())
            .map_err(|e| LivebusError::Bind(format!("ipc name: {e}")))
    }
    #[cfg(not(any(unix, windows)))]
    {
        Err(LivebusError::Bind(
            "unsupported platform for ipc input".into(),
        ))
    }
}

async fn handle_connection(conn: IpcStream, mgr: SubscriberManager) -> Result<(), LivebusError> {
    let (rd, mut wr) = tokio::io::split(conn);
    let mut reader = BufReader::with_capacity(64 * 1024, rd);
    // BUG-A4-002 fix: bound the per-line read at MAX_FRAME_BYTES + 1
    // BEFORE allocating, so a peer that writes garbage without a newline
    // cannot drive the String to multi-GiB heap allocations. The
    // previous implementation called `read_line` (which grows the buffer
    // unboundedly) and only checked the size *after* the read returned,
    // making the cap a no-op against an OOM-DoS attempt.
    //
    // We use `AsyncBufReadExt::read_until` over a `Take` adapter on the
    // underlying reader: `Take` short-circuits the read at the byte
    // limit, after which `read_until` returns with whatever was buffered
    // and we treat the situation as an over-large frame. The +1 is so
    // we can distinguish "exactly at the cap" from "over the cap".
    let mut line_buf: Vec<u8> = Vec::with_capacity(4096);
    let bus = mgr.bus();

    loop {
        line_buf.clear();
        // Manual byte-by-byte (well, buffered-byte) read into `line_buf`
        // with a hard cap. We keep using the outer `BufReader`'s 64 KiB
        // buffer for throughput, but bound the per-frame allocation
        // ourselves rather than trusting `read_line` to honour the cap
        // (it doesn't -- it grows the destination String unboundedly).
        let cap: usize = MAX_FRAME_BYTES;
        let mut byte = [0u8; 1];
        let mut frame_done = false;
        let mut peer_closed = false;
        loop {
            match reader.read(&mut byte).await {
                Ok(0) => {
                    peer_closed = true;
                    break;
                }
                Ok(_) => {
                    line_buf.push(byte[0]);
                    if byte[0] == b'\n' {
                        frame_done = true;
                        break;
                    }
                    if line_buf.len() > cap {
                        return Err(LivebusError::FrameTooLarge(line_buf.len(), cap));
                    }
                }
                Err(e) => return Err(LivebusError::Io(e)),
            }
        }
        if peer_closed && line_buf.is_empty() {
            break; // clean EOF
        }
        if peer_closed && !frame_done {
            // half-line at EOF -- treat as protocol error and drop.
            return Err(LivebusError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "ipc peer closed mid-frame",
            )));
        }
        // UTF-8 validate -- malformed bytes get the same treatment as
        // bad JSON below (skip with an error ack), not connection drop.
        let line: &str = match std::str::from_utf8(&line_buf) {
            Ok(s) => s,
            Err(e) => {
                let msg = format!("{{\"op\":\"error\",\"message\":\"bad utf8: {e}\"}}\n");
                let _ = wr.write_all(msg.as_bytes()).await;
                continue;
            }
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let event: Event = match serde_json::from_str(trimmed) {
            Ok(ev) => ev,
            Err(e) => {
                let msg = format!("{{\"op\":\"error\",\"message\":\"bad json: {e}\"}}\n");
                let _ = wr.write_all(msg.as_bytes()).await;
                continue;
            }
        };

        if let Err(e) = bus.publish(event.clone()) {
            warn!(error = %e, "ipc: bus publish failed");
            let msg = format!("{{\"op\":\"error\",\"message\":\"{}\"}}\n", e);
            let _ = wr.write_all(msg.as_bytes()).await;
            continue;
        }
        // Fan out to direct subscribers as well — the bus broadcast covers
        // anything attached via `subscribe_raw`, but the SubscriberManager
        // path is the canonical one with backpressure tracking.
        mgr.dispatch(&event);
        // Best-effort ack so well-behaved publishers can flow-control.
        let _ = wr.write_all(b"{\"op\":\"ack\"}\n").await;
    }

    Ok(())
}
