//! Configuration file support.
//!
//! A device can be described in TOML and started with
//! `rcache run --config /etc/rcache/rcache.toml`. Command-line flags override
//! the file, so operators can keep a base config and tweak a single value.
//!
//! ```toml
//! origin = "/dev/sdb"
//! cache  = "/var/lib/rcache/sdb.journal"
//! id     = -1
//!
//! [ublk]
//! queues = 1
//! depth  = 64
//!
//! [write_back]
//! high_pct     = 50
//! low_pct      = 20
//! max_pct      = 90
//! step_blocks  = 64
//! idle_secs    = 5
//!
//! [metrics]
//! listen = "127.0.0.1:9101"
//! ```

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub origin: Option<String>,
    pub cache: Option<String>,
    pub id: Option<i32>,
    #[serde(default)]
    pub ublk: UblkConfig,
    #[serde(default)]
    pub write_back: WriteBackConfig,
    #[serde(default)]
    pub metrics: MetricsConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UblkConfig {
    #[serde(default = "default_queues")]
    pub queues: u16,
    #[serde(default = "default_depth")]
    pub depth: u16,
}

impl Default for UblkConfig {
    fn default() -> Self {
        UblkConfig {
            queues: default_queues(),
            depth: default_depth(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WriteBackConfig {
    /// Start an eager drain above this journal usage percentage.
    #[serde(default = "default_high")]
    pub high_pct: u64,
    /// Consider a drain complete below this percentage.
    #[serde(default = "default_low")]
    pub low_pct: u64,
    /// Block writers above this percentage.
    #[serde(default = "default_max")]
    pub max_pct: u64,
    /// Blocks written back per step.
    #[serde(default = "default_step")]
    pub step_blocks: usize,
    /// Seconds a quiet journal may stay dirty.
    #[serde(default = "default_idle")]
    pub idle_secs: u64,
}

impl Default for WriteBackConfig {
    fn default() -> Self {
        WriteBackConfig {
            high_pct: default_high(),
            low_pct: default_low(),
            max_pct: default_max(),
            step_blocks: default_step(),
            idle_secs: default_idle(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricsConfig {
    /// `host:port` to serve `/metrics`, `/status` and `/healthz`. Unset means
    /// no HTTP endpoint.
    pub listen: Option<String>,
}

fn default_queues() -> u16 {
    1
}
fn default_depth() -> u16 {
    64
}
fn default_high() -> u64 {
    50
}
fn default_low() -> u64 {
    20
}
fn default_max() -> u64 {
    90
}
fn default_step() -> usize {
    // Matches `ublk_tgt::WriteBack::default()`: small bursts keep writer
    // latency low when the origin is slow. See the comment there.
    64
}
fn default_idle() -> u64 {
    5
}

impl Config {
    /// Load and validate a config file.
    pub fn load(path: &Path) -> Result<Config> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("read config {}", path.display()))?;
        let cfg: Config =
            toml::from_str(&text).with_context(|| format!("parse config {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Parse from a string (used by tests and `--config -`).
    #[cfg(test)]
    pub fn parse(text: &str) -> Result<Config> {
        let cfg: Config = toml::from_str(text)?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<()> {
        let wb = &self.write_back;
        if wb.high_pct == 0 || wb.high_pct > 100 {
            anyhow::bail!("write_back.high_pct must be in 1..=100");
        }
        if wb.max_pct == 0 || wb.max_pct > 100 {
            anyhow::bail!("write_back.max_pct must be in 1..=100");
        }
        if wb.low_pct >= wb.high_pct {
            anyhow::bail!("write_back.low_pct must be below high_pct");
        }
        if wb.high_pct > wb.max_pct {
            anyhow::bail!("write_back.high_pct must not exceed max_pct");
        }
        if wb.step_blocks == 0 {
            anyhow::bail!("write_back.step_blocks must be positive");
        }
        if self.ublk.queues == 0 || self.ublk.depth == 0 {
            anyhow::bail!("ublk.queues and ublk.depth must be positive");
        }
        if let Some(listen) = &self.metrics.listen {
            listen
                .parse::<std::net::SocketAddr>()
                .with_context(|| format!("metrics.listen `{listen}` is not host:port"))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_config_uses_defaults() {
        let c = Config::parse(
            r#"origin = "/dev/sdb"
cache = "/tmp/j""#,
        )
        .unwrap();
        assert_eq!(c.origin.as_deref(), Some("/dev/sdb"));
        assert_eq!(c.ublk.depth, 64);
        assert_eq!(c.write_back.high_pct, 50);
        assert_eq!(c.write_back.step_blocks, 64);
        assert!(c.metrics.listen.is_none());
    }

    #[test]
    fn full_config() {
        let c = Config::parse(
            r#"
origin = "/dev/sdb"
cache  = "/var/lib/rcache/sdb.journal"
id     = 3

[ublk]
queues = 2
depth  = 32

[write_back]
high_pct    = 40
low_pct     = 10
max_pct     = 80
step_blocks = 512
idle_secs   = 30

[metrics]
listen = "127.0.0.1:9101"
"#,
        )
        .unwrap();
        assert_eq!(c.id, Some(3));
        assert_eq!(c.ublk.queues, 2);
        assert_eq!(c.write_back.max_pct, 80);
        assert_eq!(c.write_back.idle_secs, 30);
        assert_eq!(c.metrics.listen.as_deref(), Some("127.0.0.1:9101"));
    }

    #[test]
    fn rejects_unknown_keys() {
        // A typo must not silently fall back to a default.
        let e = Config::parse(
            r#"origin = "/dev/sdb"
cache = "/tmp/j"
[write_back]
hihg_pct = 40
"#,
        );
        assert!(e.is_err(), "unknown key was accepted");
    }

    #[test]
    fn rejects_bad_watermarks() {
        let e = Config::parse(
            r#"low_pct = 60
high_pct = 50
"#,
        );
        assert!(e.is_err());
        let e = Config::parse("max_pct = 200");
        assert!(e.is_err());
        let e = Config::parse(
            r#"
[metrics]
listen = "not-an-address"
"#,
        );
        assert!(e.is_err());
    }

    #[test]
    fn rejects_zero_step() {
        let e = Config::parse(
            r#"
[write_back]
step_blocks = 0
"#,
        );
        assert!(e.is_err());
    }
}
