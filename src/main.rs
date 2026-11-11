//! rcache: write-back block cache for a slow origin disk, backed by a file on
//! fast storage.
//!
//! ```text
//! rcache run    --origin /dev/sdb --cache /mnt/nvme/rcache.journal [--id N]
//! rcache stop   --id N
//! rcache status --id N
//! ```

mod cache;
mod config;
mod httpd;
mod log;
mod metrics;
mod shared;
mod stats;
mod ublk_tgt;

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

fn usage() -> ! {
    eprintln!(
        "usage:
  rcache run    --origin <BLOCKDEV> --cache <FILE> [--id N] [--queues N] [--depth N]
                [--wb-high PCT] [--wb-low PCT] [--wb-step BLOCKS] [--wb-idle SECS]
                [--config FILE] [--metrics-listen HOST:PORT]
  rcache stop   --id N
  rcache status --id N [--format text|json|prometheus] [--watch SECS]
  rcache metrics [--id N] [--format prometheus|json|text]"
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
            // A config file provides the baseline; flags override it.
            let cfg = match arg(&args, "--config") {
                Some(p) => Some(config::Config::load(Path::new(&p))?),
                None => None,
            };
            let d = cfg.clone().unwrap_or_default();

            let origin = arg(&args, "--origin")
                .or(d.origin.clone())
                .map(PathBuf::from)
                .context("--origin is required (or `origin` in the config)")?;
            let cache = arg(&args, "--cache")
                .or(d.cache.clone())
                .map(PathBuf::from)
                .context("--cache is required (or `cache` in the config)")?;
            let id: i32 = match arg(&args, "--id") {
                Some(v) => v.parse().context("--id must be an integer")?,
                None => d.id.unwrap_or(-1),
            };
            let queues: u16 = match arg(&args, "--queues") {
                Some(v) => v.parse()?,
                None => d.ublk.queues,
            };
            let depth: u16 = match arg(&args, "--depth") {
                Some(v) => v.parse()?,
                None => d.ublk.depth,
            };

            let mut write_back = ublk_tgt::WriteBack {
                high_watermark: d.write_back.high_pct,
                low_watermark: d.write_back.low_pct,
                max_watermark: d.write_back.max_pct,
                step_blocks: d.write_back.step_blocks,
                idle: std::time::Duration::from_secs(d.write_back.idle_secs),
            };
            if let Some(v) = arg(&args, "--wb-high") {
                write_back.high_watermark = v.parse().context("--wb-high must be a percentage")?;
            }
            if let Some(v) = arg(&args, "--wb-low") {
                write_back.low_watermark = v.parse().context("--wb-low must be a percentage")?;
            }
            if let Some(v) = arg(&args, "--wb-step") {
                write_back.step_blocks = v.parse().context("--wb-step must be a block count")?;
            }
            if let Some(v) = arg(&args, "--wb-idle") {
                write_back.idle =
                    std::time::Duration::from_secs(v.parse().context("--wb-idle must be seconds")?);
            }
            if write_back.low_watermark >= write_back.high_watermark {
                anyhow::bail!("--wb-low must be below --wb-high");
            }

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
            let drain_flag = drain.clone();
            let mask = set;
            std::thread::spawn(move || loop {
                let mut sig: libc::c_int = 0;
                unsafe {
                    libc::sigwait(&mask, &mut sig);
                }
                ::log::info!("rcache: caught signal, draining journal (device keeps serving IO)");
                drain_flag.store(true, Ordering::SeqCst);
            });

            let metrics_listen = arg(&args, "--metrics-listen").or(d.metrics.listen.clone());

            let id = if id < 0 {
                ublk_tgt::next_free_id()?
            } else {
                id
            };
            ublk_tgt::Target {
                id,
                cache,
                origin,
                queues,
                depth,
                write_back,
            }
            .run(drain, metrics_listen)
        }
        "stop" => {
            let id: i32 = arg(&args, "--id").context("--id is required")?.parse()?;
            ublk_tgt::remove(id)
        }
        "status" => {
            let id: i32 = arg(&args, "--id").context("--id is required")?.parse()?;
            let format = arg(&args, "--format").unwrap_or_else(|| "text".into());
            let watch: Option<u64> = match arg(&args, "--watch") {
                Some(v) => Some(v.parse().context("--watch must be seconds")?),
                None => None,
            };
            let once = || -> Result<String> {
                let Some(st) = shared::read_status(id)? else {
                    anyhow::bail!("no running rcache device with id {id}");
                };
                Ok(match format.as_str() {
                    "text" => metrics::render_text(&st),
                    "json" => metrics::render_json(&st),
                    "prometheus" | "prom" => metrics::render_prometheus(&[st]),
                    other => anyhow::bail!("unknown --format {other} (text|json|prometheus)"),
                })
            };
            match watch {
                None => print!("{}", once()?),
                Some(secs) => loop {
                    print!(
                        "\x1b[H\x1b[2J{}",
                        once().unwrap_or_else(|e| format!("{e}\n"))
                    );
                    std::thread::sleep(std::time::Duration::from_secs(secs.max(1)));
                },
            }
            Ok(())
        }
        "metrics" => {
            let format = arg(&args, "--format").unwrap_or_else(|| "prometheus".into());
            // All devices publishing status, or only --id if given.
            let ids: Vec<i32> = match arg(&args, "--id") {
                Some(v) => vec![v.parse()?],
                None => shared::list_ids(),
            };
            let mut all = Vec::new();
            for id in ids {
                if let Some(st) = shared::read_status(id)? {
                    all.push(st);
                }
            }
            if all.is_empty() {
                anyhow::bail!("no running rcache devices");
            }
            match format.as_str() {
                "prometheus" | "prom" => print!("{}", metrics::render_prometheus(&all)),
                "json" => {
                    let parts: Vec<String> = all.iter().map(metrics::render_json).collect();
                    println!("[{}]", parts.join(","));
                }
                "text" => {
                    for st in &all {
                        print!("{}", metrics::render_text(st));
                        println!();
                    }
                }
                other => anyhow::bail!("unknown --format {other} (prometheus|json|text)"),
            }
            Ok(())
        }
        _ => usage(),
    }
}
