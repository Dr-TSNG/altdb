use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

pub const MIN_KSU: u32 = 32525;
pub const DATA_DIR: &str = "/data/adb/altdb";
pub const MAX_HOSTS: usize = 32;
pub const MAX_TRANSPORTS: usize = 8;
pub const MAX_STREAMS: usize = 64;
pub const PAIR_SECONDS: u64 = 300;
pub const MAX_PAIR_FAILURES: u32 = 10;

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PortMode {
    #[default]
    Random,
    Fixed,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct Config {
    pub enabled: bool,
    pub port_mode: PortMode,
    pub fixed_port: u16,
    pub allow_adb_root: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: true,
            port_mode: PortMode::Random,
            fixed_port: 5555,
            allow_adb_root: false,
        }
    }
}

impl Config {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.fixed_port >= 1024, "fixed port must be 1024–65535");
        Ok(())
    }

    pub fn port(&self) -> u16 {
        if self.port_mode == PortMode::Random {
            0
        } else {
            self.fixed_port
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Host {
    pub fingerprint: String,
    pub public_key: String,
    pub name: String,
    pub paired_at: u64,
    pub last_connected: Option<u64>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Control {
    Status,
    Configure { config: Config },
    PairStart,
    PairStop,
    Revoke { fingerprint: String },
    Logs,
    Stop,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn safe_defaults() {
        let c = Config::default();
        assert_eq!(c.port(), 0);
        assert!(!c.allow_adb_root);
        assert!(serde_json::from_str::<Config>(r#"{"enabled":true}"#).is_err());
    }
    #[test]
    fn reject_privileged_port() {
        let c = Config {
            fixed_port: 80,
            ..Config::default()
        };
        assert!(c.validate().is_err());
    }
}
