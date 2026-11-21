use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::devices::write_private;

pub const DEFAULT_PORT: u16 = 18093;
pub const DEFAULT_TUNNEL_PORT: u16 = 18094;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tunnel_port: Option<u16>,
    #[serde(default)]
    pub allow_hosts: Vec<String>,
    #[serde(default)]
    pub allow_tunnel_hosts: Vec<String>,
    #[serde(default)]
    pub urls: Vec<String>,
    #[serde(default)]
    pub notify: Vec<Hook>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tailscale: Option<Tailscale>,
    #[serde(flatten)]
    pub rest: Map<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Hook {
    Url(String),
    Webhook { kind: String, url: String },
}

impl Hook {
    pub fn webhook(url: &str) -> Self {
        Self::Webhook { kind: "webhook".to_string(), url: url.to_string() }
    }

    fn url(&self) -> Option<&str> {
        match self {
            Self::Url(url) => Some(url),
            Self::Webhook { kind, url } => (kind == "webhook").then_some(url.as_str()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tailscale {
    pub https_port: u16,
    pub host: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Effective {
    pub name: String,
    pub port: u16,
    pub tunnel_port: Option<u16>,
    pub allow_hosts: Vec<String>,
    pub allow_tunnel_hosts: Vec<String>,
    pub urls: Vec<String>,
    pub hooks: Vec<String>,
}

pub fn path(data_dir: &Path) -> PathBuf {
    data_dir.join("remote").join("serve.json")
}

pub fn exists(data_dir: &Path) -> bool {
    path(data_dir).is_file()
}

pub fn load(data_dir: &Path) -> anyhow::Result<Option<Settings>> {
    let path = path(data_dir);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    serde_json::from_str(&text).map(Some).with_context(|| format!("parse {}", path.display()))
}

pub fn save(data_dir: &Path, settings: &Settings) -> anyhow::Result<()> {
    let mut text = serde_json::to_string_pretty(settings)?;
    text.push('\n');
    write_private(&path(data_dir), text.as_bytes())
}

fn union(first: &[String], second: &[String]) -> Vec<String> {
    let mut joined: Vec<String> = Vec::new();
    for value in first.iter().chain(second) {
        if !joined.contains(value) {
            joined.push(value.clone());
        }
    }
    joined
}

impl Settings {
    pub fn merged(&self, flags: &Settings) -> Settings {
        let mut notify = self.notify.clone();
        for hook in &flags.notify {
            if !notify.contains(hook) {
                notify.push(hook.clone());
            }
        }
        Settings {
            name: flags.name.clone().or_else(|| self.name.clone()),
            port: flags.port.or(self.port),
            tunnel_port: flags.tunnel_port.or(self.tunnel_port),
            allow_hosts: union(&self.allow_hosts, &flags.allow_hosts),
            allow_tunnel_hosts: union(&self.allow_tunnel_hosts, &flags.allow_tunnel_hosts),
            urls: union(&self.urls, &flags.urls),
            notify,
            tailscale: self.tailscale.clone(),
            rest: self.rest.clone(),
        }
    }

    pub fn effective(&self, default_name: &str) -> Effective {
        let derived: Vec<String> =
            self.allow_hosts.iter().chain(&self.allow_tunnel_hosts).map(|host| format!("wss://{host}/v1/ws")).collect();
        let tunnel_port =
            (!self.allow_tunnel_hosts.is_empty()).then(|| self.tunnel_port.unwrap_or(DEFAULT_TUNNEL_PORT));
        Effective {
            name: self.name.clone().filter(|name| !name.is_empty()).unwrap_or_else(|| default_name.to_string()),
            port: self.port.unwrap_or(DEFAULT_PORT),
            tunnel_port,
            allow_hosts: self.allow_hosts.clone(),
            allow_tunnel_hosts: self.allow_tunnel_hosts.clone(),
            urls: union(&self.urls, &derived),
            hooks: self.notify.iter().filter_map(Hook::url).map(str::to_string).collect(),
        }
    }
}

pub fn check_host(name: &str) -> anyhow::Result<()> {
    let valid = !name.is_empty()
        && name.len() <= 253
        && name.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b':' | b'[' | b']'));
    anyhow::ensure!(valid, "{name:?} is not a host name: give the bare name, such as studio.example.com");
    Ok(())
}
