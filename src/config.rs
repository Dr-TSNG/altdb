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
pub enum Backend {
    #[default]
    Tcp,
    Vsock,
}

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
    #[serde(default)]
    pub backend: Backend,
    pub port_mode: PortMode,
    pub fixed_port: u16,
    #[serde(default = "default_mdns_enabled")]
    pub mdns_enabled: bool,
    pub allow_adb_root: bool,
}

fn default_mdns_enabled() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: true,
            backend: Backend::Tcp,
            port_mode: PortMode::Random,
            fixed_port: 5555,
            mdns_enabled: true,
            allow_adb_root: false,
        }
    }
}

impl Config {
    pub fn mdns(&self) -> bool {
        self.backend == Backend::Tcp && self.mdns_enabled
    }

    pub fn endpoints_changed(&self, previous: &Self) -> bool {
        self.backend != previous.backend
            || self.enabled != previous.enabled
            || self.port() != previous.port()
            || self.mdns() != previous.mdns()
    }

    pub fn validate(&self) -> Result<()> {
        if self.backend == Backend::Tcp {
            ensure!(self.fixed_port >= 1024, "fixed port must be 1024–65535");
        }
        Ok(())
    }

    pub fn port(&self) -> u16 {
        if self.backend == Backend::Vsock {
            5555
        } else if self.port_mode == PortMode::Random {
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
    Toggle,
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
        assert!(c.mdns_enabled);
        assert!(!c.allow_adb_root);
        assert!(serde_json::from_str::<Config>(r#"{"enabled":true}"#).is_err());
    }
    #[test]
    fn legacy_config_enables_mdns() {
        let c: Config = serde_json::from_str(
            r#"{"enabled":true,"port_mode":"random","fixed_port":5555,"allow_adb_root":false}"#,
        )
        .unwrap();
        assert_eq!(c, Config::default());
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
