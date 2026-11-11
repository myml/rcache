//! A tiny HTTP endpoint for monitoring: `/metrics`, `/healthz` and `/status`.
//!
//! Deliberately dependency-free and minimal -- it serves one request per
//! connection, emits only the three routes above and never blocks the IO
//! path (it reads the shared status files, not the cache).
//!
//! ```text
//! GET /metrics    Prometheus text exposition (all devices)
//! GET /status     JSON (all devices)
//! GET /healthz    200 "ok" / 503 when any device reports failures
//! ```

use crate::metrics;
use crate::shared;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Serve until `stop` is set.
pub fn serve(addr: std::net::SocketAddr, stop: Arc<AtomicBool>) -> anyhow::Result<()> {
    let listener = TcpListener::bind(addr)?;
    listener.set_nonblocking(true)?;
    log::info!("metrics endpoint listening on http://{addr}/metrics");

    while !stop.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _peer)) => {
                // A slow or broken client must not stall the loop.
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
                let _ = handle(stream);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                log::warn!("metrics listener: {e}");
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    }
    Ok(())
}

fn snapshot_all() -> Vec<metrics::Status> {
    shared::list_ids()
        .into_iter()
        .filter_map(|id| shared::read_status(id).ok().flatten())
        .collect()
}

fn handle(mut stream: TcpStream) -> std::io::Result<()> {
    let mut buf = [0u8; 1024];
    let n = stream.read(&mut buf)?;
    if n == 0 {
        return Ok(());
    }
    let req = String::from_utf8_lossy(&buf[..n]);
    let path = req
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("/");
    // Strip any query string.
    let path = path.split('?').next().unwrap_or("/");

    let all = snapshot_all();
    let (status, ctype, body) = match path {
        "/healthz" => {
            let ok = !all.is_empty() && all.iter().all(|s| s.healthy());
            let body = if ok { "ok\n" } else { "degraded\n" };
            (
                if ok { 200 } else { 503 },
                "text/plain; charset=utf-8",
                body.to_string(),
            )
        }
        "/metrics" => (
            200,
            "text/plain; version=0.0.4; charset=utf-8",
            metrics::render_prometheus(&all),
        ),
        "/status" => {
            let parts: Vec<String> = all.iter().map(metrics::render_json).collect();
            (200, "application/json", format!("[{}]\n", parts.join(",")))
        }
        "/" => (
            200,
            "text/plain; charset=utf-8",
            "rcache\nget /metrics, /status or /healthz\n".to_string(),
        ),
        _ => (404, "text/plain; charset=utf-8", "not found\n".to_string()),
    };

    let resp = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        reason(status),
        body.len()
    );
    stream.write_all(resp.as_bytes())?;
    stream.flush()
}

fn reason(code: u16) -> &'static str {
    match code {
        200 => "OK",
        404 => "Not Found",
        503 => "Service Unavailable",
        _ => "Unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reason_strings() {
        assert_eq!(reason(200), "OK");
        assert_eq!(reason(503), "Service Unavailable");
        assert_eq!(reason(404), "Not Found");
    }

    /// Exercise the request parser and the health verdict end to end over a
    /// real socket (no rcache devices publishing, so health is degraded).
    #[test]
    fn serves_healthz_and_404() {
        let dir = std::env::temp_dir().join(format!("rcache-http-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("RCACHE_RUN_DIR", &dir);

        let addr: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
        let listener = TcpListener::bind(addr).unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener); // hand the ephemeral port back for `serve`

        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        let handle = std::thread::spawn(move || {
            let _ = serve(addr, s);
        });
        // Wait for the listener to come up.
        let mut stream = None;
        for _ in 0..50 {
            if let Ok(st) = TcpStream::connect(addr) {
                stream = Some(st);
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let mut stream = stream.expect("server did not start");

        stream
            .write_all(b"GET /healthz HTTP/1.1\r\nHost: x\r\n\r\n")
            .unwrap();
        let mut resp = String::new();
        stream.read_to_string(&mut resp).unwrap();
        // No devices are publishing, so health is 503.
        assert!(resp.starts_with("HTTP/1.1 503"), "got: {resp}");

        let mut stream = TcpStream::connect(addr).unwrap();
        stream.write_all(b"GET /nope HTTP/1.1\r\n\r\n").unwrap();
        let mut resp = String::new();
        stream.read_to_string(&mut resp).unwrap();
        assert!(resp.starts_with("HTTP/1.1 404"));

        stop.store(true, Ordering::SeqCst);
        let _ = handle.join();
        std::fs::remove_dir_all(&dir).ok();
    }
}
