//! rcache: write-back block cache for a slow origin disk, backed by a file on
//! fast storage.
//!
//! ```text
//! rcache run    --origin /dev/sdb --cache /mnt/nvme/rcache.journal [--id N]
//! rcache stop   --id N
//! rcache status --id N
//! ```

mod cache;
mod log;
mod ublk_tgt;

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

fn usage() -> ! {
    eprintln!(
        "usage:
  rcache run    --origin <BLOCKDEV> --cache <FILE> [--id N] [--queues N] [--depth N]
  rcache stop   --id N
  rcache status --id N"
    );
    std::process::exit(2)
}

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_target(false)
        .format_timestamp_secs()
        .init();

    if let Err(e) = real_main() {
        eprintln!("rcache: {e:#}");
        std::process::exit(1);
    }
}

fn real_main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first() else { usage() };
    match cmd.as_str() {
        "run" => {
            let origin = arg(&args, "--origin")
                .map(PathBuf::from)
                .context("--origin is required")?;
            let cache = arg(&args, "--cache")
                .map(PathBuf::from)
                .context("--cache is required")?;
            let id: i32 = arg(&args, "--id")
                .unwrap_or_else(|| "-1".into())
                .parse()
                .context("--id must be an integer")?;
            let queues: u16 = arg(&args, "--queues")
                .unwrap_or_else(|| "1".into())
                .parse()?;
            let depth: u16 = arg(&args, "--depth")
                .unwrap_or_else(|| "64".into())
                .parse()?;

            // Mask the shutdown signals *before* spawning any thread, so every
            // thread inherits the mask and only our sigwait thread consumes
            // them (otherwise the default action kills us mid-flight).
            let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
            unsafe {
                libc::sigemptyset(&mut set);
                libc::sigaddset(&mut set, libc::SIGINT);
                libc::sigaddset(&mut set, libc::SIGTERM);
                libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
            }

            // `drain` is set by the signal thread and consumed by the target,
            // which pushes the journal to the origin but keeps serving IO:
            // a mounted filesystem still needs the device to stay alive.
            let drain = Arc::new(AtomicBool::new(false));
            let d = drain.clone();
            let mask = set;
            std::thread::spawn(move || {
                loop {
                    let mut sig: libc::c_int = 0;
                    unsafe {
                        libc::sigwait(&mask, &mut sig);
                    }
                    ::log::info!(
                        "rcache: caught signal, draining journal (device keeps serving IO)"
                    );
                    d.store(true, Ordering::SeqCst);
                }
            });

            ublk_tgt::Target {
                id,
                cache,
                origin,
                queues,
                depth,
            }
            .run(drain)
        }
        "stop" => {
            let id: i32 = arg(&args, "--id").context("--id is required")?.parse()?;
            ublk_tgt::remove(id)
        }
        "status" => {
            let id: i32 = arg(&args, "--id").context("--id is required")?.parse()?;
            let ctrl = libublk::ctrl::UblkCtrl::new_simple(id)
                .map_err(|e| anyhow::anyhow!("open ublk ctrl {id}: {e:?}"))?;
            ctrl.dump();
            Ok(())
        }
        _ => usage(),
    }
}
