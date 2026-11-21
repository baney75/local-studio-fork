use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aes_gcm::aead::{Aead, Payload};
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use hkdf::Hkdf;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use x25519_dalek::{PublicKey, StaticSecret};

use crate::platform::Mode;

mod setup;

pub use setup::{
    ingress, normalize_host, path_ports, platform, tailnet_mounts, tailnet_off, tailnet_on, waitlist, MCP_PATHS,
    SERVE_PATHS, SERVICE_LABEL, SERVICE_UNIT, TAILNET_SERVE_PORT, WAITLIST_URL,
};

pub const DEFAULT_PORTS: Ports = Ports { serve: 18094, mcp: 8787 };
const CONTROLLER_PORT: u16 = 18091;
pub const CLOUDFLARED_VERSION: &str = "2026.10.7";
const MAX_DOWNLOAD: u64 = 80 * 1024 * 1024;
const BACKOFF_MINUTES: [u64; 4] = [1, 5, 15, 60];
const AUTH_ERRORS: [&str; 5] =
    ["unauthorized", "invalid tunnel secret", "token is not valid", "failed to authenticate", "authentication failed"];

struct Pin {
    os: &'static str,
    arch: &'static str,
    asset: &'static str,
    asset_sha256: &'static str,
    binary_sha256: &'static str,
    archive: bool,
}

const PINS: [Pin; 4] = [
    Pin {
        os: "macos",
        arch: "aarch64",
        asset: "cloudflared-darwin-arm64.tgz",
        asset_sha256: "3586ccf397e98e5f437e60e7c307ca6ac463e110580503af8bb8e6f9ebf831bd",
        binary_sha256: "2136431d5eb467bf447d2ee514b713e192719b143aec1d9e09bf7dc290d5bb12",
        archive: true,
    },
    Pin {
        os: "macos",
        arch: "x86_64",
        asset: "cloudflared-darwin-amd64.tgz",
        asset_sha256: "02f3edfd87684dd5b705607688e4eb4b323f6e777b81e7b3a85af556aa356848",
        binary_sha256: "5c55e77da7012cebca19170357e196176c48c9647cc2ead404702fa125ddbd86",
        archive: true,
    },
    Pin {
        os: "linux",
        arch: "x86_64",
        asset: "cloudflared-linux-amd64",
        asset_sha256: "1f99fb518b83dec2ba8d870a7cc991d82a77e6196295a3d2754ba17ab9f91872",
        binary_sha256: "1f99fb518b83dec2ba8d870a7cc991d82a77e6196295a3d2754ba17ab9f91872",
        archive: false,
    },
    Pin {
        os: "linux",
        arch: "aarch64",
        asset: "cloudflared-linux-arm64",
        asset_sha256: "3df43aedc9ff291d1735b5cb576cb97e0b157488796db2e32b28aecabb445775",
        binary_sha256: "3df43aedc9ff291d1735b5cb576cb97e0b157488796db2e32b28aecabb445775",
        archive: false,
    },
];
const STABLE_SERVICE: Option<&str> = None;
const SEAL_ALG: &str = "X25519-HKDF-SHA256-A256GCM";
const SEAL_INFO: &[u8] = b"ls-tunnels sealed token v1";
const RUNNABLE: [&str; 3] = ["approved", "live", "down"];
const NOT_OPEN: &str = "Hosted tunnels aren't open yet. Use your own Cloudflare account: local-studio tunnel byo";

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ports {
    pub serve: u16,
    pub mcp: u16,
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub route: String,
    pub state: String,
    pub hostname: Option<String>,
    pub mcp_url: Option<String>,
    pub ws_url: Option<String>,
    pub ports: RoutePorts,
    pub position: Option<u64>,
    pub release_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub rotation_required: bool,
    pub message: String,
    pub next: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_command: Option<String>,
    pub checked_at: String,
}

impl Status {
    fn stepped(mut self, running: bool) -> Self {
        let (next, command) = match self.state.as_str() {
            "live" | "attached" | "detached" => ("ready", None),
            "approved" | "down" if !running => ("needs_start", Some("local-studio tunnel up --yes")),
            "confirming" => ("needs_signin", Some("local-studio tunnel request")),
            "approved" | "down" | "waiting" | "provisioning" => ("waiting", Some("local-studio tunnel status --json")),
            "suspended" => ("suspended", Some("local-studio tunnel status --json")),
            _ if self.message == NOT_OPEN => ("byo", Some("local-studio tunnel byo")),
            _ => ("needs_request", Some("local-studio tunnel request")),
        };
        self.next = next.into();
        self.next_command = command.map(ToString::to_string);
        self
    }
}

#[derive(Serialize, Clone, Debug)]
pub struct Failure {
    pub code: String,
    pub message: String,
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl From<String> for Failure {
    fn from(message: String) -> Self {
        Self { code: "error".into(), message }
    }
}

impl From<&str> for Failure {
    fn from(message: &str) -> Self {
        Self { code: "error".into(), message: message.to_string() }
    }
}

fn fail(code: impl Into<String>, message: impl Into<String>) -> Failure {
    Failure { code: code.into(), message: message.into() }
}

#[derive(Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct Saved {
    request_id: Option<String>,
    hostname: Option<String>,
    ports: Option<Ports>,
    #[serde(default)]
    pending: Vec<String>,
    #[serde(default, skip_serializing_if = "is_zero")]
    token_failures: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    next_token_at: Option<u64>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct RoutePorts {
    pub mcp: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serve: Option<u16>,
}

impl From<Ports> for RoutePorts {
    fn from(ports: Ports) -> Self {
        Self { mcp: ports.mcp, serve: Some(ports.serve) }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Route {
    pub v: u32,
    pub owner: String,
    pub route: String,
    pub hostname: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aliases: Option<Vec<String>>,
    pub ports: RoutePorts,
}

#[derive(Clone, Debug)]
pub enum Cloudflared {
    Dev(PathBuf),
    Pinned(PathBuf),
}

impl Cloudflared {
    #[must_use]
    pub fn path(&self) -> &Path {
        match self {
            Self::Dev(path) | Self::Pinned(path) => path,
        }
    }

    fn verify(&self) -> Result<(), Failure> {
        match self {
            Self::Dev(_) => Ok(()),
            Self::Pinned(path) if pinned_binary(path) => Ok(()),
            Self::Pinned(path) => Err(fail(
                "cloudflared_mismatch",
                format!(
                    "{} doesn't match cloudflared {CLOUDFLARED_VERSION}'s pinned digest, so it wasn't started.",
                    path.display()
                ),
            )),
        }
    }
}

#[derive(Serialize, Deserialize)]
struct KeyFile {
    sign: String,
    seal: String,
}

struct Keys {
    sign: SigningKey,
    seal: StaticSecret,
}

impl Keys {
    fn sign_key(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.sign.verifying_key().as_bytes())
    }

    fn seal_key(&self) -> String {
        URL_SAFE_NO_PAD.encode(PublicKey::from(&self.seal).as_bytes())
    }
}

pub struct Tunnel {
    dir: PathBuf,
    service: Option<String>,
    http: reqwest::Client,
}

impl Tunnel {
    pub fn open(data_dir: Option<&str>) -> Result<Self, String> {
        let root = match data_dir.filter(|value| !value.is_empty()) {
            Some(explicit) => PathBuf::from(explicit),
            None => match std::env::var_os("LOCAL_STUDIO_DATA_DIR").filter(|value| !value.is_empty()) {
                Some(value) => PathBuf::from(value),
                None => crate::store::dot_dir()?,
            },
        };
        let service = std::env::var("LS_TUNNEL_SERVICE")
            .ok()
            .filter(|value| !value.is_empty())
            .or_else(|| STABLE_SERVICE.map(ToString::to_string))
            .map(|value| value.trim_end_matches('/').to_string());
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| error.to_string())?;
        Ok(Self { dir: root.join("tunnel"), service, http })
    }

    #[must_use]
    pub fn token_path(&self) -> PathBuf {
        self.dir.join("token")
    }

    fn ensure_dir(&self) -> Result<(), String> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.dir)
            .map_err(|error| format!("{}: {error}", self.dir.display()))?;
        crate::platform::set_mode(&self.dir, 0o700).map_err(|error| error.to_string())
    }

    fn private_write(&self, name: &str, bytes: &[u8]) -> Result<(), String> {
        self.ensure_dir()?;
        let path = self.dir.join(name);
        let temp = self.dir.join(format!(".{name}.{}", std::process::id()));
        let _ = fs::remove_file(&temp);
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)
            .map_err(|error| format!("{}: {error}", temp.display()))?;
        file.write_all(bytes).and_then(|()| file.sync_all()).map_err(|error| error.to_string())?;
        fs::rename(&temp, &path).map_err(|error| format!("{}: {error}", path.display()))
    }

    fn keys(&self, create: bool) -> Result<Option<Keys>, String> {
        let path = self.dir.join("device.json");
        match fs::read(&path) {
            Ok(bytes) => {
                let file: KeyFile =
                    serde_json::from_slice(&bytes).map_err(|error| format!("{}: {error}", path.display()))?;
                let sign = seed(&file.sign).ok_or("device.json: bad signing key")?;
                let seal = seed(&file.seal).ok_or("device.json: bad sealing key")?;
                Ok(Some(Keys { sign: SigningKey::from_bytes(&sign), seal: StaticSecret::from(seal) }))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && create => {
                let sign = random::<32>()?;
                let seal = random::<32>()?;
                let file = KeyFile { sign: URL_SAFE_NO_PAD.encode(sign), seal: URL_SAFE_NO_PAD.encode(seal) };
                self.private_write("device.json", &serde_json::to_vec(&file).map_err(|error| error.to_string())?)?;
                Ok(Some(Keys { sign: SigningKey::from_bytes(&sign), seal: StaticSecret::from(seal) }))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(format!("{}: {error}", path.display())),
        }
    }

    fn saved(&self) -> Saved {
        fs::read(self.dir.join("state.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    fn save(&self, saved: &Saved) -> Result<(), String> {
        self.private_write("state.json", &serde_json::to_vec_pretty(saved).map_err(|error| error.to_string())?)
    }

    fn service(&self) -> Result<&str, Failure> {
        self.service.as_deref().ok_or_else(|| fail("not_open", NOT_OPEN))
    }

    async fn call(&self, keys: &Keys, method: &str, path: &str, body: Option<&Value>) -> Result<(u16, Value), Failure> {
        let mut answer = self.call_once(keys, method, path, body).await?;
        for wait in [2, 4, 8] {
            if answer.0 != 429 || answer.1.get("error").and_then(Value::as_str) != Some("slow_down") {
                break;
            }
            tokio::time::sleep(Duration::from_secs(wait)).await;
            answer = self.call_once(keys, method, path, body).await?;
        }
        Ok(answer)
    }

    async fn call_once(
        &self,
        keys: &Keys,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<(u16, Value), Failure> {
        let service = self.service()?;
        let url = reqwest::Url::parse(&format!("{service}{path}")).map_err(|error| error.to_string())?;
        let host = url.host_str().ok_or("the tunnel service address has no host")?.to_lowercase();
        let bytes = body.map(|value| serde_json::to_vec(value).unwrap_or_default()).unwrap_or_default();
        let ts = SystemTime::now().duration_since(UNIX_EPOCH).map_err(|error| error.to_string())?.as_secs();
        let message = format!("{method}\n{host}\n{path}\n{ts}\n{}", hex(&Sha256::digest(&bytes)));
        let sig = URL_SAFE_NO_PAD.encode(keys.sign.sign(message.as_bytes()).to_bytes());
        let method = reqwest::Method::from_bytes(method.as_bytes()).map_err(|error| error.to_string())?;
        let mut request = self
            .http
            .request(method, url)
            .header("Authorization", format!("LSDevice key={},ts={ts},sig={sig}", keys.sign_key()));
        if body.is_some() {
            request = request.header("Content-Type", "application/json").body(bytes);
        }
        let response = request
            .send()
            .await
            .map_err(|error| fail("unreachable", format!("couldn't reach the tunnel service: {error}")))?;
        let code = response.status().as_u16();
        let value = response.json::<Value>().await.unwrap_or(Value::Null);
        Ok((code, value))
    }

    async fn remote(&self, keys: &Keys) -> Result<Value, Failure> {
        let (code, value) = self.call(keys, "GET", "/v1/tunnel", None).await?;
        if code == 200 {
            return Ok(value);
        }
        if matches!(code, 401 | 403 | 404 | 410) {
            self.stop();
            let _ = fs::remove_file(self.token_path());
        }
        if matches!(code, 404 | 410) {
            self.drop_pending();
        }
        Err(refusal("status", code, &value))
    }

    fn settle(&self, status: &Status) {
        if !RUNNABLE.contains(&status.state.as_str()) {
            self.stop();
            let _ = fs::remove_file(self.token_path());
        }
        if matches!(status.state.as_str(), "released" | "denied") {
            self.drop_pending();
            self.remove_own_route();
        }
    }

    fn drop_pending(&self) {
        let mut saved = self.saved();
        if !saved.pending.is_empty() {
            saved.pending.clear();
            let _ = self.save(&saved);
        }
    }

    pub async fn status(&self) -> Result<Status, Failure> {
        let saved = self.saved();
        let ports = saved.ports.unwrap_or(DEFAULT_PORTS);
        let keys = self.keys(false)?;
        if keys.is_none() {
            if let Some(route) = self.route().ok().flatten().filter(|route| !route.is_hosted()) {
                return Ok(route_status(&route));
            }
        }
        if self.service.is_none() {
            return Ok(local_status("none", NOT_OPEN, ports));
        }
        let Some(keys) = keys else {
            return Ok(local_status("none", "Request a tunnel, or use your own.", ports));
        };
        self.flush_reports(&keys).await;
        let value = self.remote(&keys).await?;
        let status = self.status_from(&value, ports);
        self.settle(&status);
        Ok(status)
    }

    fn status_from(&self, value: &Value, fallback: Ports) -> Status {
        let text = |key: &str| value.get(key).and_then(Value::as_str).map(ToString::to_string);
        let mut state = text("state").unwrap_or_else(|| "none".to_string());
        let ports = value
            .get("ports")
            .and_then(|ports| serde_json::from_value::<Ports>(ports.clone()).ok())
            .unwrap_or(fallback);
        let runnable = RUNNABLE.contains(&state.as_str());
        let running = self.running().is_some();
        let message = if state == "live" && running {
            "Live. claude.ai, ChatGPT and grok.com can use the MCP address.".to_string()
        } else if state == "live" {
            state = "down".to_string();
            "Cloudflare still shows a connector for this tunnel. If this machine just stopped, that clears within a minute; if not, another machine holds this tunnel's token.".to_string()
        } else if runnable && running {
            "cloudflared is running on this machine. Cloudflare shows it connected within a minute.".to_string()
        } else {
            text("message").unwrap_or_default()
        };
        let hostname = if runnable { text("hostname") } else { None };
        Status {
            route: if state == "none" { "none" } else { "hosted" }.to_string(),
            mcp_url: hostname.as_ref().map(|host| format!("https://{host}/mcp")),
            ws_url: hostname.as_ref().map(|host| format!("wss://{host}/v1/ws")),
            hostname,
            ports: ports.into(),
            position: value.get("position").and_then(Value::as_u64),
            release_at: text("releaseAt"),
            reason: text("reason"),
            rotation_required: value.get("rotationRequired").and_then(Value::as_bool).unwrap_or(false),
            message,
            next: String::new(),
            next_command: None,
            checked_at: now_rfc3339(),
            state,
        }
        .stepped(running)
    }

    pub async fn request(&self, ports: Ports, channel: &str, on_url: impl FnOnce(&str)) -> Result<Status, Failure> {
        self.refuse_other_route()?;
        check_ports(ports.serve, ports.mcp)?;
        self.service()?;
        let keys = self.keys(true)?.ok_or("no device keys")?;
        let listener = loopback()?;
        let port = listener.local_addr().map_err(|error| error.to_string())?.port();
        let verifier = random::<32>()?;
        let body = json!({
            "signKey": keys.sign_key(),
            "sealKey": keys.seal_key(),
            "channel": channel,
            "version": crate::VERSION,
            "platform": format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
            "ports": ports,
            "confirmChallenge": URL_SAFE_NO_PAD.encode(Sha256::digest(verifier)),
            "loopbackPort": port,
        });
        let (code, value) = self.call(&keys, "POST", "/v1/requests", Some(&body)).await?;
        if code != 201 {
            return Err(refusal("request", code, &value));
        }
        let id = value.get("id").and_then(Value::as_str).ok_or("the service gave no request id")?.to_string();
        let url = value.get("url").and_then(Value::as_str).ok_or("the service gave no sign-in address")?;
        let mut saved = self.saved();
        saved.request_id = Some(id.clone());
        saved.ports = Some(ports);
        self.save(&saved)?;
        on_url(url);
        let code = tokio::task::spawn_blocking(move || await_code(&listener, Duration::from_mins(15)))
            .await
            .map_err(|error| error.to_string())??;
        let confirm = json!({ "code": code, "verifier": URL_SAFE_NO_PAD.encode(verifier) });
        let (status, value) = self.call(&keys, "POST", &format!("/v1/requests/{id}/confirm"), Some(&confirm)).await?;
        if status != 200 {
            return Err(refusal("confirm", status, &value));
        }
        self.status().await
    }

    pub async fn up(&self, consent: impl FnOnce(&Path, &[String]) -> bool) -> Result<Status, Failure> {
        self.refuse_other_route()?;
        let keys = self
            .keys(false)?
            .ok_or_else(|| fail("no_tunnel", "No tunnel yet. Run local-studio tunnel request first."))?;
        let _lock = UpLock::take(&self.dir, false)?;
        let value = self.remote(&keys).await?;
        let status = self.status_from(&value, self.saved().ports.unwrap_or(DEFAULT_PORTS));
        self.settle(&status);
        if self.running().is_some() {
            return Ok(status);
        }
        let (hostname, ports) = startable(&status)?;
        serve_allow(&hostname);
        self.going_live(&hostname, ports).await?;
        self.token_allowed()?;
        let cloudflared = self.cloudflared(consent, "local-studio tunnel up --yes").await?;
        let _ = fs::remove_file(self.token_path());
        self.fetch_and_write(&keys, &value, &hostname).await?;
        let started = self.launch(&keys, &cloudflared, hostname, ports).await;
        if started.is_err() {
            self.stop();
            let _ = fs::remove_file(self.token_path());
        }
        started
    }

    pub async fn set_ports(&self, serve: Option<u16>, mcp: Option<u16>) -> Result<Status, Failure> {
        let keys = self.keys(false)?.ok_or_else(|| {
            fail("no_tunnel", "No hosted tunnel on this machine. Run local-studio tunnel request first.")
        })?;
        let _lock = UpLock::take(&self.dir, true)?;
        let value = self.remote(&keys).await?;
        let status = self.status_from(&value, self.saved().ports.unwrap_or(DEFAULT_PORTS));
        self.settle(&status);
        if !RUNNABLE.contains(&status.state.as_str()) {
            return Err(fail(
                "not_startable",
                format!("Nothing to change: the tunnel is {}. {}", status.state, status.message),
            ));
        }
        let old = Ports { serve: status.ports.serve.unwrap_or(DEFAULT_PORTS.serve), mcp: status.ports.mcp };
        let ports = Ports { serve: serve.unwrap_or(old.serve), mcp: mcp.unwrap_or(old.mcp) };
        check_ports(ports.serve, ports.mcp)?;
        let was_running = self.running().is_some();
        let cloudflared = if was_running {
            Some(self.cloudflared(|_, _| false, "local-studio tunnel up --yes").await?)
        } else {
            None
        };
        self.stop();
        let body = json!({ "serve": ports.serve, "mcp": ports.mcp });
        let refused = match self.call(&keys, "PUT", "/v1/tunnel/ports", Some(&body)).await {
            Ok((200, _)) => None,
            Ok((code, answer)) => Some(refusal("ports", code, &answer)),
            Err(error) => Some(error),
        };
        let changed = refused.is_none();
        let ports = if changed { ports } else { old };
        if changed {
            let mut saved = self.saved();
            saved.ports = Some(ports);
            self.save(&saved)?;
            if let Some(mut route) = self.route().ok().flatten().filter(Route::is_hosted) {
                route.ports = ports.into();
                self.write_route(&route)?;
            }
        }
        let Some(cloudflared) = cloudflared else {
            let _ = fs::remove_file(self.token_path());
            return match refused {
                Some(error) => Err(error),
                None => Ok(self.status_from(&self.remote(&keys).await?, ports)),
            };
        };
        let restarted = self.restart(&keys, &cloudflared, &value, &status, ports).await;
        if restarted.is_err() {
            self.stop();
            let _ = fs::remove_file(self.token_path());
        }
        match (refused, restarted) {
            (Some(error), Ok(_)) => {
                Err(fail(error.code.clone(), format!("{error} cloudflared runs again on the old ports.")))
            }
            (Some(error), Err(stopped)) => {
                Err(fail(error.code.clone(), format!("{error} cloudflared stays stopped: {stopped}")))
            }
            (None, Ok(status)) => Ok(status),
            (None, Err(stopped)) => Err(fail(
                stopped.code.clone(),
                format!(
                    "The ports changed to serve {} and mcp {}, but cloudflared stays stopped: {stopped}",
                    ports.serve, ports.mcp
                ),
            )),
        }
    }

    async fn restart(
        &self,
        keys: &Keys,
        cloudflared: &Cloudflared,
        value: &Value,
        status: &Status,
        ports: Ports,
    ) -> Result<Status, Failure> {
        let (hostname, _) = startable(status)?;
        self.going_live(&hostname, ports).await?;
        if !self.token_path().exists() {
            self.token_allowed()?;
            self.fetch_and_write(keys, value, &hostname).await?;
        }
        self.launch(keys, cloudflared, hostname, ports).await
    }

    async fn fetch_and_write(&self, keys: &Keys, value: &Value, hostname: &str) -> Result<(), Failure> {
        let request_id = value.get("requestId").and_then(Value::as_str).ok_or("the service gave no request id")?;
        let token = self.fetch_token(keys, request_id, hostname).await?;
        self.private_write("token", token.as_bytes())?;
        drop(token);
        Ok(())
    }

    fn token_allowed(&self) -> Result<(), Failure> {
        let saved = self.saved();
        let Some(at) = saved.next_token_at else { return Ok(()) };
        let now = unix_now();
        if now >= at {
            return Ok(());
        }
        Err(fail(
            "backoff",
            format!(
            "cloudflared failed to authenticate {} time{} in a row. The next token fetch is allowed at {} (in {} min).",
            saved.token_failures,
            if saved.token_failures == 1 { "" } else { "s" },
            rfc3339(at),
            (at - now).div_ceil(60)
        ),
        ))
    }

    fn auth_failed(&self, log_path: &Path) -> Failure {
        let mut saved = self.saved();
        saved.token_failures = saved.token_failures.saturating_add(1);
        let step = usize::try_from(saved.token_failures).unwrap_or(usize::MAX).min(BACKOFF_MINUTES.len()) - 1;
        let at = unix_now() + BACKOFF_MINUTES[step] * 60;
        saved.next_token_at = Some(at);
        let _ = self.save(&saved);
        fail(
            "cloudflared_auth",
            format!(
                "cloudflared couldn't authenticate with the tunnel token. The next token fetch is allowed at {}; see {}",
                rfc3339(at),
                log_path.display()
            ),
        )
    }

    async fn launch(
        &self,
        keys: &Keys,
        cloudflared: &Cloudflared,
        hostname: String,
        ports: Ports,
    ) -> Result<Status, Failure> {
        let mut saved = self.saved();
        saved.hostname = Some(hostname.clone());
        saved.ports = Some(ports);
        self.save(&saved)?;
        cloudflared.verify()?;
        let client_id = self.start(cloudflared.path())?;
        let mut saved = self.saved();
        if !saved.pending.contains(&client_id) {
            saved.pending.push(client_id);
        }
        self.save(&saved)?;
        for wait in [0, 1, 2, 4] {
            tokio::time::sleep(Duration::from_secs(wait)).await;
            self.flush_reports(keys).await;
            if self.saved().pending.is_empty() {
                break;
            }
        }
        if self.running().is_none() {
            return Err(fail(
                "cloudflared_failed",
                format!("cloudflared stopped; see {}", self.dir.join("cloudflared.log").display()),
            ));
        }
        let status = self.status_from(&self.remote(keys).await?, ports);
        if !RUNNABLE.contains(&status.state.as_str()) {
            return Err(fail(
                "not_startable",
                format!("The tunnel became {} while it started. {}", status.state, status.message),
            ));
        }
        self.write_route(&Route {
            v: 1,
            owner: "local-studio".into(),
            route: "hosted".into(),
            aliases: self.route().ok().flatten().filter(Route::is_hosted).and_then(|route| route.aliases),
            hostname,
            ports: ports.into(),
        })?;
        Ok(status)
    }

    pub async fn down(&self) -> Result<Status, Failure> {
        let lock = UpLock::take(&self.dir, true)?;
        self.stop();
        drop(lock);
        let _ = fs::remove_file(self.token_path());
        self.status().await
    }

    pub async fn release(&self) -> Result<Status, Failure> {
        let keys = self.keys(false)?.ok_or_else(|| fail("no_tunnel", "No tunnel to release."))?;
        let _lock = UpLock::take(&self.dir, true)?;
        self.stop();
        let _ = fs::remove_file(self.token_path());
        let (code, value) = self.call(&keys, "DELETE", "/v1/tunnel", None).await?;
        if code != 200 {
            return Err(refusal("release", code, &value));
        }
        let mut saved = self.saved();
        saved.hostname = None;
        saved.request_id = None;
        saved.pending.clear();
        let ports = saved.ports.unwrap_or(DEFAULT_PORTS);
        self.save(&saved)?;
        self.remove_own_route();
        Ok(self.status_from(&value, ports))
    }

    async fn fetch_token(&self, keys: &Keys, expect_request: &str, expect_host: &str) -> Result<String, Failure> {
        for _ in 0..2 {
            let (code, value) = self.call(keys, "GET", "/v1/tunnel?token=1", None).await?;
            if code == 409 {
                continue;
            }
            if code != 200 {
                return Err(refusal("token", code, &value));
            }
            let request_id = value.get("requestId").and_then(Value::as_str).ok_or("no request id with the token")?;
            let hostname = value.get("hostname").and_then(Value::as_str).ok_or("no hostname with the token")?;
            if request_id != expect_request || hostname != expect_host {
                return Err(
                    "The token came for a different request or hostname than the one checked. Run up again.".into()
                );
            }
            let sealed = value.get("sealed").ok_or("the service sent no token")?;
            return unseal(sealed, &keys.seal, request_id, hostname).map_err(Failure::from);
        }
        Err("The tunnel changed while the token was fetched. Check status and try again.".into())
    }

    async fn going_live(&self, hostname: &str, ports: Ports) -> Result<(), Failure> {
        self.going_live_checks(hostname, ports).await.map_err(|message| fail("going_live", message))
    }

    async fn going_live_checks(&self, hostname: &str, ports: Ports) -> Result<(), String> {
        let mcp = self
            .http
            .get(format!("http://127.0.0.1:{}/mcp", ports.mcp))
            .header("Host", hostname)
            .send()
            .await
            .map_err(|_| format!("Tychicus serve isn't answering on 127.0.0.1:{}. No auth, no tunnel.", ports.mcp))?;
        let challenge = mcp.headers().get("www-authenticate").and_then(|value| value.to_str().ok()).unwrap_or("");
        if mcp.status().as_u16() != 401 || !challenge.contains("resource_metadata") {
            return Err(format!(
                "127.0.0.1:{}/mcp must answer 401 with WWW-Authenticate naming resource_metadata; it answered {}. No auth, no tunnel.",
                ports.mcp,
                mcp.status().as_u16()
            ));
        }
        let health = |host: String| {
            self.http.get(format!("http://127.0.0.1:{}/v1/health", ports.serve)).header("Host", host).send()
        };
        let ours = health(hostname.to_string())
            .await
            .map_err(|_| format!("local-studio-serve isn't answering on 127.0.0.1:{}.", ports.serve))?;
        let ok = ours.status().as_u16() == 200
            && ours.json::<Value>().await.ok().and_then(|value| value.get("ok").and_then(Value::as_bool)) == Some(true);
        if !ok {
            return Err(format!(
                "local-studio-serve on 127.0.0.1:{} didn't answer health for {hostname}.",
                ports.serve
            ));
        }
        let other = health("check.invalid".to_string()).await.map_err(|error| error.to_string())?;
        if other.status().as_u16() != 403 {
            return Err(format!(
                "local-studio-serve answered {} for another Host; start it with --tunnel-port {} --allow-tunnel-host {hostname}.",
                other.status().as_u16(),
                ports.serve
            ));
        }
        Ok(())
    }

    async fn flush_reports(&self, keys: &Keys) {
        let mut saved = self.saved();
        if saved.pending.is_empty() {
            return;
        }
        let mut left = Vec::new();
        let mut reported = false;
        for id in &saved.pending {
            match self.call(keys, "POST", "/v1/tunnel/connector", Some(&json!({ "clientId": id }))).await {
                Ok((200, _)) => reported = true,
                Ok((404 | 410, _)) => {}
                _ => left.push(id.clone()),
            }
        }
        if left != saved.pending || (reported && (saved.token_failures != 0 || saved.next_token_at.is_some())) {
            saved.pending = left;
            if reported {
                saved.token_failures = 0;
                saved.next_token_at = None;
            }
            let _ = self.save(&saved);
        }
    }

    fn start(&self, cloudflared: &Path) -> Result<String, Failure> {
        self.ensure_dir()?;
        let log_path = self.dir.join("cloudflared.log");
        let log = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&log_path)
            .map_err(|error| error.to_string())?;
        let err = log.try_clone().map_err(|error| error.to_string())?;
        let mut command = Command::new(cloudflared);
        command
            .args(["tunnel", "--no-autoupdate", "run", "--token-file"])
            .arg(self.token_path())
            .stdin(Stdio::null())
            .stdout(log)
            .stderr(err);
        crate::platform::own_group(&mut command);
        let child = command.spawn().map_err(|error| {
            fail("cloudflared_failed", format!("couldn't start {}: {error}", cloudflared.display()))
        })?;
        let pid = child.id();
        let started = process_info(pid).map(|(_, started)| started).unwrap_or_default();
        self.private_write("cloudflared.start", started.as_bytes())?;
        self.private_write("cloudflared.pid", format!("{pid}\n").as_bytes())?;
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            let text = fs::read_to_string(&log_path).unwrap_or_default();
            if let Some(id) = connector_id(&text) {
                return Ok(id);
            }
            if self.running().is_none() {
                let text = fs::read_to_string(&log_path).unwrap_or_default();
                if let Some(id) = connector_id(&text) {
                    return Ok(id);
                }
                let lower = text.to_lowercase();
                if AUTH_ERRORS.iter().any(|error| lower.contains(error)) {
                    return Err(self.auth_failed(&log_path));
                }
                return Err(fail("cloudflared_failed", format!("cloudflared exited; see {}", log_path.display())));
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        self.stop();
        Err(fail(
            "cloudflared_failed",
            format!("cloudflared printed no connector id in 30 seconds; see {}", log_path.display()),
        ))
    }

    #[cfg(unix)]
    fn running(&self) -> Option<u32> {
        let pid: u32 = fs::read_to_string(self.dir.join("cloudflared.pid")).ok()?.trim().parse().ok()?;
        let recorded = fs::read_to_string(self.dir.join("cloudflared.start")).ok()?;
        let (stat, started) = process_info(pid)?;
        (!recorded.trim().is_empty() && started == recorded.trim() && !stat.starts_with('Z')).then_some(pid)
    }

    #[cfg(windows)]
    fn running(&self) -> Option<u32> {
        let pid: u32 = fs::read_to_string(self.dir.join("cloudflared.pid")).ok()?.trim().parse().ok()?;
        let line = crate::platform::command_line(pid)?;
        (line.contains("--token-file") && line.contains("cloudflared")).then_some(pid)
    }

    #[cfg(unix)]
    fn signal(pid: u32, signal: &str) {
        let _ = Command::new("kill").args([signal, &pid.to_string()]).status();
    }

    #[cfg(windows)]
    fn signal(pid: u32, _signal: &str) {
        crate::platform::taskkill(pid, false);
    }

    fn stop(&self) {
        let Some(pid) = self.running() else {
            let _ = fs::remove_file(self.dir.join("cloudflared.pid"));
            let _ = fs::remove_file(self.dir.join("cloudflared.start"));
            return;
        };
        Self::signal(pid, "-TERM");
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.running().is_some() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        if self.running().is_some() {
            Self::signal(pid, "-KILL");
        }
        let _ = fs::remove_file(self.dir.join("cloudflared.pid"));
        let _ = fs::remove_file(self.dir.join("cloudflared.start"));
    }
}

impl Tunnel {
    #[must_use]
    pub fn cloudflared_target(&self) -> PathBuf {
        self.dir.join("bin").join("cloudflared")
    }

    #[must_use]
    pub fn find_cloudflared(&self) -> (Option<Cloudflared>, Vec<String>) {
        if let Some(dev) = std::env::var_os("LS_CLOUDFLARED").filter(|value| !value.is_empty()) {
            return (Some(Cloudflared::Dev(PathBuf::from(dev))), Vec::new());
        }
        let mut notes = Vec::new();
        let tychicus = std::env::var_os("TYCHICUS_CONFIG_DIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| crate::store::home_dir().map(|home| home.join(".config").join("tychicus")))
            .map(|dir| dir.join("bin").join("cloudflared"));
        for path in std::iter::once(self.cloudflared_target()).chain(tychicus) {
            if !path.exists() {
                continue;
            }
            if pinned_binary(&path) {
                return (Some(Cloudflared::Pinned(path)), notes);
            }
            notes.push(format!(
                "{} doesn't match cloudflared {CLOUDFLARED_VERSION}'s pinned digest, so it isn't used.",
                path.display()
            ));
        }
        (None, notes)
    }

    pub async fn cloudflared(
        &self,
        consent: impl FnOnce(&Path, &[String]) -> bool,
        retry: &str,
    ) -> Result<Cloudflared, Failure> {
        let (found, notes) = self.find_cloudflared();
        if let Some(found) = found {
            return Ok(found);
        }
        if pin().is_none() {
            return Err(fail(
                "no_pinned_cloudflared",
                format!(
                    "Local Studio has no pinned cloudflared for {} {}.",
                    std::env::consts::OS,
                    std::env::consts::ARCH
                ),
            ));
        }
        if !consent(&self.cloudflared_target(), &notes) {
            let mut lines = notes;
            lines.push(format!(
                "cloudflared {CLOUDFLARED_VERSION} isn't downloaded yet. To download it from github.com/cloudflare/cloudflared, run: {retry}"
            ));
            return Err(fail("needs_cloudflared", lines.join("\n")));
        }
        self.download_cloudflared().await
    }

    pub async fn download_cloudflared(&self) -> Result<Cloudflared, Failure> {
        let pin = pin().ok_or("Local Studio has no pinned cloudflared for this platform.")?;
        self.ensure_dir()?;
        let bin = self.dir.join("bin");
        fs::DirBuilder::new().recursive(true).mode(0o700).create(&bin).map_err(|error| error.to_string())?;
        crate::platform::set_mode(&bin, 0o700).map_err(|error| error.to_string())?;
        let work = bin.join(format!(".download-{}", hex(&random::<8>()?)));
        fs::DirBuilder::new().mode(0o700).create(&work).map_err(|error| format!("{}: {error}", work.display()))?;
        let installed = self.download_into(pin, &work).await.map_err(|message| fail("download_failed", message));
        let _ = fs::remove_dir_all(&work);
        installed
    }

    async fn download_into(&self, pin: &Pin, work: &Path) -> Result<Cloudflared, String> {
        let url =
            format!("https://github.com/cloudflare/cloudflared/releases/download/{CLOUDFLARED_VERSION}/{}", pin.asset);
        let client = reqwest::Client::builder()
            .timeout(Duration::from_mins(5))
            .redirect(reqwest::redirect::Policy::limited(10))
            .https_only(true)
            .build()
            .map_err(|error| error.to_string())?;
        let failed =
            |error: String| format!("Couldn't download cloudflared: {error}. Check the connection and try again.");
        let mut response = client.get(&url).send().await.map_err(|error| failed(error.to_string()))?;
        if !response.status().is_success() {
            return Err(failed(format!("the server answered {}", response.status().as_u16())));
        }
        if response.content_length().is_some_and(|length| length > MAX_DOWNLOAD) {
            return Err(failed("the file is larger than expected".into()));
        }
        let file_path = work.join(pin.asset);
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&file_path)
            .map_err(|error| format!("{}: {error}", file_path.display()))?;
        let mut hasher = Sha256::new();
        let mut size = 0u64;
        while let Some(chunk) = response.chunk().await.map_err(|error| failed(error.to_string()))? {
            size += chunk.len() as u64;
            if size > MAX_DOWNLOAD {
                return Err(failed("the file is larger than expected".into()));
            }
            hasher.update(&chunk);
            file.write_all(&chunk).map_err(|error| error.to_string())?;
        }
        file.sync_all().map_err(|error| error.to_string())?;
        drop(file);
        if hex(&hasher.finalize()) != pin.asset_sha256 {
            return Err(format!(
                "The cloudflared download didn't match cloudflared {CLOUDFLARED_VERSION}'s pinned digest, so it was deleted and not used."
            ));
        }
        let binary = if pin.archive {
            let unpacked = work.join("unpacked");
            fs::DirBuilder::new().mode(0o700).create(&unpacked).map_err(|error| error.to_string())?;
            let out = Command::new("tar")
                .arg("-xzf")
                .arg(&file_path)
                .arg("-C")
                .arg(&unpacked)
                .arg("cloudflared")
                .stdin(Stdio::null())
                .output()
                .map_err(|error| format!("Couldn't unpack the cloudflared download: {error}"))?;
            if !out.status.success() {
                return Err(format!(
                    "Couldn't unpack the cloudflared download: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
            }
            unpacked.join("cloudflared")
        } else {
            file_path
        };
        let meta = fs::symlink_metadata(&binary).map_err(|error| format!("{}: {error}", binary.display()))?;
        if !meta.is_file() || meta.len() > MAX_DOWNLOAD || file_digest(&binary).as_deref() != Some(pin.binary_sha256) {
            return Err(format!(
                "The cloudflared download didn't hold the pinned cloudflared {CLOUDFLARED_VERSION} program, so it was not used."
            ));
        }
        crate::platform::set_mode(&binary, 0o755).map_err(|error| error.to_string())?;
        let target = self.cloudflared_target();
        fs::rename(&binary, &target).map_err(|error| format!("{}: {error}", target.display()))?;
        Ok(Cloudflared::Pinned(target))
    }

    #[must_use]
    pub fn route_path(&self) -> PathBuf {
        self.dir.join("route.json")
    }

    fn route_bytes(&self) -> Result<Option<Vec<u8>>, String> {
        let path = self.route_path();
        match fs::read(&path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(format!("{}: {error}", path.display())),
        }
    }

    pub fn route(&self) -> Result<Option<Route>, String> {
        let Some(bytes) = self.route_bytes()? else { return Ok(None) };
        serde_json::from_slice(&bytes).map(Some).map_err(|error| format!("{}: {error}", self.route_path().display()))
    }

    fn write_route(&self, route: &Route) -> Result<(), String> {
        self.private_write("route.json", &serde_json::to_vec_pretty(route).map_err(|error| error.to_string())?)
    }

    fn remove_own_route(&self) {
        if self.route().ok().flatten().is_some_and(|route| route.is_hosted()) {
            let _ = fs::remove_file(self.route_path());
        }
    }

    fn refuse_other_route(&self) -> Result<(), Failure> {
        match self.route()? {
            Some(route) if !route.is_hosted() => {
                let off = if route.owner == "tychicus" { " Turn it off first: tychicus remote off" } else { "" };
                Err(fail(
                    "other_route",
                    format!(
                        "This machine already has a {} route for {}, run by {}, and a machine has one tunnel.{off}",
                        route.route,
                        route.hostname,
                        if route.owner == "tychicus" { "Tychicus" } else { "Local Studio" }
                    ),
                ))
            }
            _ => Ok(()),
        }
    }
}

impl Route {
    #[must_use]
    pub fn is_hosted(&self) -> bool {
        self.owner == "local-studio" && self.route == "hosted"
    }
}

fn route_status(route: &Route) -> Status {
    let carried = route.ports.serve.is_some();
    let owner = match route.owner.as_str() {
        "tychicus" => "Tychicus",
        "local-studio" => "Local Studio",
        other => other,
    };
    let message = if carried {
        format!("{owner} runs this route, and it carries the phone address to local-studio-serve.")
    } else {
        format!("{owner} runs this route. It doesn't carry the phone address.")
    };
    Status {
        route: match route.route.as_str() {
            "cloudflare" => "byo".into(),
            "tailnet" => "tailscale".into(),
            other => other.into(),
        },
        state: if carried { "attached" } else { "detached" }.into(),
        hostname: Some(route.hostname.clone()),
        mcp_url: Some(format!("https://{}/mcp", route.hostname)),
        ws_url: carried.then(|| format!("wss://{}/v1/ws", route.hostname)),
        ports: route.ports,
        position: None,
        release_at: None,
        reason: None,
        rotation_required: false,
        message,
        next: String::new(),
        next_command: None,
        checked_at: now_rfc3339(),
    }
    .stepped(false)
}

fn serve_allow(hostname: &str) -> bool {
    Command::new(setup::serve_program())
        .args(["allow-tunnel-host", hostname])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn startable(status: &Status) -> Result<(String, Ports), Failure> {
    if !RUNNABLE.contains(&status.state.as_str()) {
        return Err(fail(
            "not_startable",
            format!("Nothing to start: the tunnel is {}. {}", status.state, status.message),
        ));
    }
    if status.rotation_required {
        return Err(fail(
            "rotation_required",
            "Restored after a second connector: rotate the Tychicus passcode, grants and phones first.",
        ));
    }
    let hostname = status.hostname.clone().ok_or("the service gave no hostname")?;
    Ok((hostname, Ports { serve: status.ports.serve.unwrap_or(DEFAULT_PORTS.serve), mcp: status.ports.mcp }))
}

pub fn check_ports(serve: u16, mcp: u16) -> Result<(), Failure> {
    if serve < 1024 || mcp < 1024 || serve == mcp || serve == CONTROLLER_PORT || mcp == CONTROLLER_PORT {
        return Err(fail("bad_ports", format!(
            "Those ports aren't allowed (serve {serve}, mcp {mcp}): use two different loopback ports from 1024 to 65535, not {CONTROLLER_PORT}."
        )));
    }
    Ok(())
}

fn pin() -> Option<&'static Pin> {
    PINS.iter().find(|pin| pin.os == std::env::consts::OS && pin.arch == std::env::consts::ARCH)
}

fn pinned_binary(path: &Path) -> bool {
    let Some(pin) = pin() else { return false };
    fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file())
        && file_digest(path).as_deref() == Some(pin.binary_sha256)
}

fn file_digest(path: &Path) -> Option<String> {
    let mut file = fs::File::open(path).ok()?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = file.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Some(hex(&hasher.finalize()))
}

fn process_info(pid: u32) -> Option<(String, String)> {
    let out = Command::new("ps")
        .env("LC_ALL", "C")
        .args(["-p", &pid.to_string(), "-o", "stat=", "-o", "lstart="])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let (stat, started) = line.split_once(char::is_whitespace)?;
    Some((stat.to_string(), started.trim().to_string()))
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_zero(value: &u32) -> bool {
    *value == 0
}

struct UpLock(#[allow(dead_code)] fs::File);

impl UpLock {
    fn take(dir: &Path, wait: bool) -> Result<Self, Failure> {
        fs::DirBuilder::new().recursive(true).mode(0o700).create(dir).map_err(|error| error.to_string())?;
        let path = dir.join("up.lock");
        let file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&path)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        if wait {
            file.lock().map_err(|error| format!("{}: {error}", path.display()))?;
        } else if file.try_lock().is_err() {
            return Err(fail("busy", "Another local-studio tunnel up is already starting this tunnel."));
        }
        Ok(Self(file))
    }
}

fn local_status(state: &str, message: &str, ports: Ports) -> Status {
    Status {
        route: "none".into(),
        state: state.into(),
        hostname: None,
        mcp_url: None,
        ws_url: None,
        ports: ports.into(),
        position: None,
        release_at: None,
        reason: None,
        rotation_required: false,
        message: message.into(),
        next: String::new(),
        next_command: None,
        checked_at: now_rfc3339(),
    }
    .stepped(false)
}

fn refusal(what: &str, code: u16, value: &Value) -> Failure {
    let error = value.get("error").and_then(Value::as_str).unwrap_or("");
    let reason = match error {
        "has_tunnel" => "This machine already has a hosted tunnel.",
        "waitlist_full" => "The waitlist is full. Use your own Cloudflare account: local-studio tunnel byo",
        "too_many_requests" | "rate_limited" => "Too many tries. Wait an hour and try again.",
        "slow_down" => "The tunnel service asked to slow down. Try again in a minute.",
        "bad_ports" => "Those ports aren't allowed: use two different loopback ports from 1024 to 65535.",
        "too_many_port_changes" => "The ports can change 6 times a day. Try again tomorrow.",
        "too_many_token_fetches" => "Too many token fetches this hour. Wait an hour and try again.",
        "not_approved" => "Only an approved tunnel's ports can change.",
        _ => "",
    };
    let id = if error.is_empty() { format!("service_{code}") } else { error.to_string() };
    if reason.is_empty() {
        fail(id, format!("{what} refused: {code} {error}").trim_end())
    } else {
        fail(id, reason)
    }
}

fn loopback() -> Result<TcpListener, String> {
    for _ in 0..20 {
        let bytes = random::<2>()?;
        let port = 49152 + u16::from_be_bytes(bytes) % (65535 - 49152);
        if let Ok(listener) = TcpListener::bind(("127.0.0.1", port)) {
            return Ok(listener);
        }
    }
    Err("no free loopback port for the sign-in".into())
}

fn await_code(listener: &TcpListener, wait: Duration) -> Result<String, String> {
    listener.set_nonblocking(true).map_err(|error| error.to_string())?;
    let deadline = Instant::now() + wait;
    while Instant::now() < deadline {
        match listener.accept() {
            Ok((stream, _)) => {
                if let Some(code) = answer(stream) {
                    return Ok(code);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(error) => return Err(error.to_string()),
        }
    }
    Err("The sign-in wasn't finished in 15 minutes. Request again.".into())
}

fn answer(mut stream: TcpStream) -> Option<String> {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut buf = [0u8; 4096];
    let n = stream.read(&mut buf).ok()?;
    let head = String::from_utf8_lossy(&buf[..n]);
    let target = head.lines().next()?.strip_prefix("GET ")?.split(' ').next()?.to_string();
    let code = target.strip_prefix("/confirmed?").and_then(|query| {
        query.split('&').find_map(|pair| pair.strip_prefix("code=")).filter(|code| {
            !code.is_empty() && code.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        })
    });
    let reply = if code.is_some() {
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nConnection: close\r\n\r\nConfirmed. Go back to Local Studio.\n"
    } else {
        "HTTP/1.1 404 Not Found\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
    };
    let _ = stream.write_all(reply.as_bytes());
    code.map(ToString::to_string)
}

fn unseal(sealed: &Value, secret: &StaticSecret, request_id: &str, hostname: &str) -> Result<String, String> {
    let field = |key: &str| {
        sealed
            .get(key)
            .and_then(Value::as_str)
            .and_then(|text| URL_SAFE_NO_PAD.decode(text).ok())
            .ok_or_else(|| format!("sealed token: bad {key}"))
    };
    if sealed.get("v").and_then(Value::as_u64) != Some(1) || sealed.get("alg").and_then(Value::as_str) != Some(SEAL_ALG)
    {
        return Err("sealed token: unknown format".into());
    }
    let epk: [u8; 32] = field("epk")?.try_into().map_err(|_| "sealed token: bad epk")?;
    let nonce: [u8; 12] = field("nonce")?.try_into().map_err(|_| "sealed token: bad nonce")?;
    let ct = field("ct")?;
    let recipient = PublicKey::from(secret);
    let shared = secret.diffie_hellman(&PublicKey::from(epk));
    if !shared.was_contributory() {
        return Err("sealed token: low-order key".into());
    }
    let mut salt = epk.to_vec();
    salt.extend_from_slice(recipient.as_bytes());
    let mut key = [0u8; 32];
    Hkdf::<Sha256>::new(Some(&salt), shared.as_bytes())
        .expand(SEAL_INFO, &mut key)
        .map_err(|_| "sealed token: key derivation failed")?;
    let aad = format!("ls-tunnels/v1\n{request_id}\n{hostname}");
    let plain = Aes256Gcm::new(&key.into())
        .decrypt(Nonce::from_slice(&nonce), Payload { msg: &ct, aad: aad.as_bytes() })
        .map_err(|_| "sealed token: it doesn't open with this device's key")?;
    String::from_utf8(plain).map_err(|_| "sealed token: not text".into())
}

fn connector_id(log: &str) -> Option<String> {
    log.lines().find_map(|line| {
        let rest = &line[line.find("Connector ID")? + "Connector ID".len()..];
        let id = rest.trim_start_matches([':', ' ', '=']).split_whitespace().next()?.to_lowercase();
        (id.len() == 36 && id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')).then_some(id)
    })
}

fn seed(text: &str) -> Option<[u8; 32]> {
    URL_SAFE_NO_PAD.decode(text).ok()?.try_into().ok()
}

fn random<const N: usize>() -> Result<[u8; N], String> {
    let mut bytes = [0u8; N];
    getrandom::getrandom(&mut bytes).map_err(|error| error.to_string())?;
    Ok(bytes)
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut out, b| {
        let _ = write!(out, "{b:02x}");
        out
    })
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn now_rfc3339() -> String {
    rfc3339(unix_now())
}

fn rfc3339(secs: u64) -> String {
    let days = i64::try_from(secs / 86_400).unwrap_or(0);
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}
