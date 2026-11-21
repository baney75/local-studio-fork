use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::io::Write as _;
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::{check_ports, fail, serve_allow, Failure, Ports, Route, RoutePorts, Tunnel, UpLock, DEFAULT_PORTS};
use crate::platform::Mode;

pub const WAITLIST_URL: &str = "https://local-studio-waitlist.magnus-b37.workers.dev/";
pub const SERVE_PATHS: [&str; 2] = ["/v1/health", "/v1/ws"];
pub const MCP_PATHS: [&str; 9] = [
    "/mcp",
    "/.well-known/oauth-protected-resource",
    "/.well-known/oauth-protected-resource/mcp",
    "/.well-known/oauth-authorization-server",
    "/.well-known/oauth-authorization-server/mcp",
    "/authorize",
    "/token",
    "/register",
    "/revoke",
];
pub const TAILNET_SERVE_PORT: u16 = 18093;
pub const SERVICE_LABEL: &str = "com.localstudio.tunnel";
pub const SERVICE_UNIT: &str = "local-studio-tunnel.service";
const OWN: Service = Service { label: SERVICE_LABEL, unit: SERVICE_UNIT, what: "Local Studio's tunnel" };
const TYCHICUS: Service =
    Service { label: "com.tychicus.cloudflared", unit: "tychicus-cloudflared.service", what: "Tychicus's tunnel" };
const CHECK_WAITS: [u64; 7] = [0, 1, 2, 4, 8, 15, 30];
const SERVICE_WAIT: Duration = Duration::from_secs(10);
const RUN_TIMEOUT: Duration = Duration::from_mins(1);
const YML: &str = "cloudflared.yml";
const TAILNET_RECORD: &str = "tailnet.json";

#[must_use]
pub fn platform() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(windows) {
        "windows"
    } else {
        "linux"
    }
}

#[must_use]
pub fn waitlist(channel: &str, platform: &str) -> Option<Value> {
    if channel == "dev" {
        return None;
    }
    let url = format!("{WAITLIST_URL}?channel={}&platform={}", query(channel), query(platform));
    Some(json!({
        "state": "waitlist",
        "next": "waitlist",
        "url": url,
        "channel": channel,
        "message": format!("Tunnel setup is on the dev channel only for now. Join the waitlist: {url}"),
    }))
}

fn query(text: &str) -> String {
    text.bytes().fold(String::new(), |mut out, b| {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
        out
    })
}

#[must_use]
pub fn path_ports(ports: Ports) -> Vec<(&'static str, u16)> {
    SERVE_PATHS.iter().map(|path| (*path, ports.serve)).chain(MCP_PATHS.iter().map(|path| (*path, ports.mcp))).collect()
}

fn anchored(path: &str) -> String {
    format!("^{}$", path.replace('.', "\\."))
}

fn yaml_text(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

#[must_use]
pub fn ingress(tunnel: &str, credentials: &Path, hostnames: &[String], ports: Ports) -> String {
    let mut out = format!(
        "tunnel: {tunnel}\ncredentials-file: {}\nno-autoupdate: true\ningress:\n",
        yaml_text(&credentials.display().to_string())
    );
    for host in hostnames {
        for (path, port) in path_ports(ports) {
            let _ = write!(
                out,
                "  - hostname: {host}\n    path: {}\n    service: http://127.0.0.1:{port}\n",
                yaml_text(&anchored(path))
            );
        }
    }
    out.push_str("  - service: http_status:404\n");
    out
}

#[must_use]
pub fn tailnet_mounts(ports: Ports) -> Vec<(String, String)> {
    path_ports(ports)
        .into_iter()
        .map(|(path, port)| (path.to_string(), format!("http://127.0.0.1:{port}{path}")))
        .collect()
}

#[must_use]
pub fn tailnet_on(path: &str, target: &str) -> Vec<String> {
    argv(&["serve", "--bg", "--yes", "--https=443", &format!("--set-path={path}"), target])
}

#[must_use]
pub fn tailnet_off(path: &str) -> Vec<String> {
    argv(&["serve", "--https=443", &format!("--set-path={path}"), "off"])
}

#[must_use]
pub fn normalize_host(raw: &str) -> Option<String> {
    let lower = raw.trim().to_lowercase();
    let host = lower.strip_prefix("https://").or_else(|| lower.strip_prefix("http://")).unwrap_or(&lower);
    let host = host.split('/').next().unwrap_or_default().trim_end_matches('.');
    let label_ok = |label: &str| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    };
    (host.len() <= 253 && host.contains('.') && host.split('.').all(label_ok)).then(|| host.to_string())
}

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(ToString::to_string).collect()
}

fn is_uuid(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(
            |(i, b)| {
                if matches!(i, 8 | 13 | 18 | 23) {
                    *b == b'-'
                } else {
                    b.is_ascii_hexdigit()
                }
            },
        )
}

fn find_uuid(text: &str) -> Option<String> {
    let lower = text.to_lowercase();
    (0..lower.len().saturating_sub(35))
        .filter(|start| lower.is_char_boundary(*start) && lower.is_char_boundary(start + 36))
        .map(|start| &lower[start..start + 36])
        .find(|candidate| is_uuid(candidate))
        .map(ToString::to_string)
}

fn new_route(kind: &str, hostname: &str, aliases: &[String], ports: Ports) -> Route {
    Route {
        v: 1,
        owner: "local-studio".into(),
        route: kind.into(),
        hostname: hostname.into(),
        aliases: Some(aliases.to_vec()),
        ports: ports.into(),
    }
}

enum Halt {
    State(Value),
    Fail(Failure),
}

impl From<Failure> for Halt {
    fn from(failure: Failure) -> Self {
        Self::Fail(failure)
    }
}

impl From<String> for Halt {
    fn from(message: String) -> Self {
        Self::Fail(message.into())
    }
}

fn settle(result: Result<Value, Halt>) -> Result<Value, Failure> {
    match result {
        Ok(value) | Err(Halt::State(value)) => Ok(value),
        Err(Halt::Fail(failure)) => Err(failure),
    }
}

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Out {
    fn ok(&self) -> bool {
        self.code == 0
    }

    fn text(&self) -> String {
        let mut text = self.stdout.trim().to_string();
        if !self.stderr.trim().is_empty() {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(self.stderr.trim());
        }
        let count = text.chars().count();
        if count > 400 {
            text = text.chars().skip(count - 400).collect();
        }
        text
    }
}

async fn run(program: &Path, args: &[String]) -> Result<Out, String> {
    let mut command = tokio::process::Command::new(program);
    command.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    let output = tokio::time::timeout(RUN_TIMEOUT, command.output())
        .await
        .map_err(|_| format!("{} {} took over a minute", program.display(), args.join(" ")))?
        .map_err(|error| format!("couldn't run {}: {error}", program.display()))?;
    Ok(Out {
        code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

fn tool(var: &str, name: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| crate::workspace::process::which(name))
}

pub(super) fn serve_program() -> PathBuf {
    tool("LS_SERVE_BIN", "local-studio-serve").unwrap_or_else(|| PathBuf::from("local-studio-serve"))
}

fn serve_disallow(hostname: &str) {
    let _ = std::process::Command::new(serve_program())
        .args(["allow-tunnel-host", "--remove", hostname])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn tailscale_program() -> Option<PathBuf> {
    tool("LS_TAILSCALE", "tailscale").or_else(|| {
        let app = PathBuf::from("/Applications/Tailscale.app/Contents/MacOS/Tailscale");
        (cfg!(target_os = "macos") && app.is_file()).then_some(app)
    })
}

fn home() -> Result<PathBuf, Failure> {
    crate::store::home_dir().ok_or_else(|| fail("no_home", "HOME is not set"))
}

fn cloudflared_dir() -> Result<PathBuf, Failure> {
    Ok(home()?.join(".cloudflared"))
}

fn tychicus_dir() -> Result<PathBuf, Failure> {
    match std::env::var_os("TYCHICUS_CONFIG_DIR").filter(|value| !value.is_empty()) {
        Some(dir) => Ok(PathBuf::from(dir)),
        None => Ok(home()?.join(".config").join("tychicus")),
    }
}

fn machine_slug() -> String {
    let name = std::process::Command::new("hostname")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_lowercase())
        .unwrap_or_default();
    let name = name.strip_suffix(".local").unwrap_or(&name);
    let slug: String = name.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    let slug: String = slug.trim_matches('-').chars().take(40).collect();
    let slug = slug.trim_end_matches('-').to_string();
    if slug.is_empty() {
        "machine".into()
    } else {
        slug
    }
}

fn listening(port: u16) -> bool {
    TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], port)), Duration::from_secs(1)).is_ok()
}

fn canonical(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn real_manager(var: &str, system: &Path) -> bool {
    match std::env::var_os(var).filter(|value| !value.is_empty()) {
        None => true,
        Some(chosen) => canonical(Path::new(&chosen)) == canonical(system),
    }
}

async fn account_home() -> Option<PathBuf> {
    let id = crate::workspace::process::which("id").unwrap_or_else(|| PathBuf::from("/usr/bin/id"));
    if cfg!(target_os = "macos") {
        let name = run(&id, &argv(&["-un"])).await.ok().filter(Out::ok)?.stdout.trim().to_string();
        if name.is_empty() || name.contains('/') {
            return None;
        }
        let out =
            run(Path::new("/usr/bin/dscl"), &argv(&[".", "-read", &format!("/Users/{name}"), "NFSHomeDirectory"]))
                .await
                .ok()
                .filter(Out::ok)?;
        let dir = out.stdout.lines().find_map(|line| line.strip_prefix("NFSHomeDirectory:"))?.trim().to_string();
        return (!dir.is_empty()).then(|| canonical(Path::new(&dir)));
    }
    let uid = run(&id, &argv(&["-u"])).await.ok().filter(Out::ok)?.stdout.trim().to_string();
    let getent = crate::workspace::process::which("getent")?;
    let out = run(&getent, &argv(&["passwd", &uid])).await.ok().filter(Out::ok)?;
    let dir = out.stdout.lines().next()?.split(':').nth(5)?.trim().to_string();
    (!dir.is_empty()).then(|| canonical(Path::new(&dir)))
}

fn service_guard(manager: &str, real: bool, home: &Path, account: Option<&Path>) -> Result<(), Failure> {
    let unsafe_path = |message: String| Err(fail("unsafe_service_path", message));
    match (real, account) {
        (_, None) => unsafe_path(format!(
            "Couldn't read this account's home folder, so Local Studio can't tell whether {manager} and HOME ({}) belong together. Nothing was changed.",
            home.display()
        )),
        (true, Some(account)) if account != home => unsafe_path(format!(
            "HOME is {}, not this account's home {}, but {manager} is the real one, so its services would change for a folder that isn't yours. Nothing was changed.",
            home.display(),
            account.display()
        )),
        (false, Some(account)) if account == home => unsafe_path(format!(
            "{manager} is a stand-in (LS_LAUNCHCTL or LS_SYSTEMCTL) while HOME is this account's real home {}, so real service files would be written for a fake service manager. Nothing was changed.",
            account.display()
        )),
        _ => Ok(()),
    }
}

#[derive(Clone, Copy)]
struct Service {
    label: &'static str,
    unit: &'static str,
    what: &'static str,
}

enum Manager {
    Launchd { launchctl: PathBuf, domain: String, agents: PathBuf },
    Systemd { systemctl: PathBuf, units: PathBuf },
}

impl Manager {
    async fn detect() -> Result<Self, Failure> {
        let home = home()?;
        if cfg!(target_os = "macos") {
            let launchctl = tool("LS_LAUNCHCTL", "launchctl").unwrap_or_else(|| PathBuf::from("/bin/launchctl"));
            let real = real_manager("LS_LAUNCHCTL", Path::new("/bin/launchctl"));
            service_guard("launchctl", real, &canonical(&home), account_home().await.as_deref())?;
            let uid = run(Path::new("/usr/bin/id"), &argv(&["-u"])).await?;
            let uid = uid.stdout.trim().to_string();
            if uid.is_empty() || !uid.bytes().all(|b| b.is_ascii_digit()) {
                return Err(fail("service_failed", "Couldn't read this user's id for launchd."));
            }
            return Ok(Self::Launchd {
                launchctl,
                domain: format!("gui/{uid}"),
                agents: home.join("Library").join("LaunchAgents"),
            });
        }
        if cfg!(target_os = "linux") {
            let systemctl = tool("LS_SYSTEMCTL", "systemctl").ok_or_else(|| {
                fail("no_service_manager", "systemctl wasn't found, so Local Studio can't keep the tunnel running.")
            })?;
            let system = crate::workspace::process::which("systemctl").unwrap_or_default();
            let real = real_manager("LS_SYSTEMCTL", &system);
            service_guard("systemctl", real, &canonical(&home), account_home().await.as_deref())?;
            return Ok(Self::Systemd { systemctl, units: home.join(".config").join("systemd").join("user") });
        }
        Err(fail(
            "unsupported",
            "Local Studio's own Cloudflare tunnel service isn't built for this system yet. local-studio tunnel setup --tailnet works here.",
        ))
    }

    fn file(&self, service: Service) -> PathBuf {
        match self {
            Self::Launchd { agents, .. } => agents.join(format!("{}.plist", service.label)),
            Self::Systemd { units, .. } => units.join(service.unit),
        }
    }

    async fn call(&self, args: &[String]) -> Result<Out, String> {
        match self {
            Self::Launchd { launchctl, .. } => run(launchctl, args).await,
            Self::Systemd { systemctl, .. } => {
                let mut all = vec!["--user".to_string()];
                all.extend_from_slice(args);
                run(systemctl, &all).await
            }
        }
    }

    fn target(&self, service: Service) -> String {
        match self {
            Self::Launchd { domain, .. } => format!("{domain}/{}", service.label),
            Self::Systemd { .. } => service.unit.to_string(),
        }
    }

    async fn running(&self, service: Service) -> bool {
        match self {
            Self::Launchd { .. } => self
                .call(&argv(&["print", &self.target(service)]))
                .await
                .is_ok_and(|out| out.ok() && out.stdout.lines().any(|line| line.trim() == "state = running")),
            Self::Systemd { .. } => {
                self.call(&argv(&["is-active", service.unit])).await.is_ok_and(|out| out.stdout.trim() == "active")
            }
        }
    }

    async fn loaded(&self, service: Service) -> bool {
        match self {
            Self::Launchd { .. } => self.call(&argv(&["print", &self.target(service)])).await.is_ok_and(|out| out.ok()),
            Self::Systemd { .. } => self.running(service).await,
        }
    }

    async fn wait_running(&self, service: Service) -> bool {
        let deadline = Instant::now() + SERVICE_WAIT;
        loop {
            if self.running(service).await {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    async fn install(&self, service: Service, program: &Path, args: &[String], log: &Path) -> Result<(), String> {
        let file = self.file(service);
        let text = match self {
            Self::Launchd { .. } => plist(service.label, program, args, log),
            Self::Systemd { .. } => unit(service.what, program, args),
        };
        if let Some(dir) = file.parent() {
            fs::create_dir_all(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
        }
        let temp = file.with_extension(format!("tmp-{}", std::process::id()));
        fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o644)
            .open(&temp)
            .and_then(|mut handle| handle.write_all(text.as_bytes()))
            .and_then(|()| fs::rename(&temp, &file))
            .map_err(|error| format!("{}: {error}", file.display()))?;
        match self {
            Self::Launchd { domain, .. } => {
                let _ = self.call(&argv(&["bootout", &self.target(service)])).await;
                let mut last = String::new();
                for wait in [0, 1, 2] {
                    tokio::time::sleep(Duration::from_secs(wait)).await;
                    let out = self.call(&argv(&["bootstrap", domain, &file.display().to_string()])).await?;
                    if out.ok() {
                        return Ok(());
                    }
                    last = out.text();
                }
                Err(format!("launchctl bootstrap {domain} {} failed: {last}", file.display()))
            }
            Self::Systemd { .. } => {
                for args in
                    [argv(&["daemon-reload"]), argv(&["enable", service.unit]), argv(&["restart", service.unit])]
                {
                    let out = self.call(&args).await?;
                    if !out.ok() {
                        return Err(format!("systemctl --user {} failed: {}", args.join(" "), out.text()));
                    }
                }
                Ok(())
            }
        }
    }

    async fn stop(&self, service: Service) -> Result<(), String> {
        let args = match self {
            Self::Launchd { .. } => argv(&["bootout", &self.target(service)]),
            Self::Systemd { .. } => argv(&["stop", service.unit]),
        };
        let out = self.call(&args).await?;
        if out.ok() || !self.loaded(service).await {
            return Ok(());
        }
        Err(format!("{} {} failed: {}", self.name(), args.join(" "), out.text()))
    }

    async fn start(&self, service: Service) -> Result<(), String> {
        let args = match self {
            Self::Launchd { domain, .. } => argv(&["bootstrap", domain, &self.file(service).display().to_string()]),
            Self::Systemd { .. } => argv(&["start", service.unit]),
        };
        let out = self.call(&args).await?;
        if out.ok() || self.running(service).await {
            return Ok(());
        }
        Err(format!("{} {} failed: {}", self.name(), args.join(" "), out.text()))
    }

    async fn remove(&self, service: Service) -> Result<bool, String> {
        let file = self.file(service);
        if !file.exists() {
            return Ok(false);
        }
        match self {
            Self::Launchd { .. } => self.stop(service).await?,
            Self::Systemd { .. } => {
                let out = self.call(&argv(&["disable", "--now", service.unit])).await?;
                if !out.ok() && self.running(service).await {
                    return Err(format!("systemctl --user disable --now {} failed: {}", service.unit, out.text()));
                }
            }
        }
        fs::remove_file(&file).map_err(|error| format!("{}: {error}", file.display()))?;
        if let Self::Systemd { .. } = self {
            let _ = self.call(&argv(&["daemon-reload"])).await;
        }
        Ok(true)
    }

    fn name(&self) -> &'static str {
        match self {
            Self::Launchd { .. } => "launchctl",
            Self::Systemd { .. } => "systemctl --user",
        }
    }
}

fn xml(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn plist(label: &str, program: &Path, args: &[String], log: &Path) -> String {
    let mut arguments = format!("\t\t<string>{}</string>\n", xml(&program.display().to_string()));
    for arg in args {
        let _ = writeln!(arguments, "\t\t<string>{}</string>", xml(arg));
    }
    let log = xml(&log.display().to_string());
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">
<plist version=\"1.0\">
<dict>
\t<key>Label</key>
\t<string>{label}</string>
\t<key>ProgramArguments</key>
\t<array>
{arguments}\t</array>
\t<key>RunAtLoad</key>
\t<true/>
\t<key>KeepAlive</key>
\t<true/>
\t<key>ThrottleInterval</key>
\t<integer>10</integer>
\t<key>StandardOutPath</key>
\t<string>{log}</string>
\t<key>StandardErrorPath</key>
\t<string>{log}</string>
</dict>
</plist>
"
    )
}

fn systemd_word(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\"").replace('%', "%%").replace('$', "$$"))
}

fn unit(what: &str, program: &Path, args: &[String]) -> String {
    let mut line = systemd_word(&program.display().to_string());
    for arg in args {
        line.push(' ');
        line.push_str(&systemd_word(arg));
    }
    format!(
        "[Unit]\nDescription={what}\n\n[Service]\nExecStart={line}\nRestart=always\nRestartSec=10\n\n[Install]\nWantedBy=default.target\n"
    )
}

struct TychicusTunnel {
    hostname: String,
    tunnel: String,
    credentials: PathBuf,
    port: Option<u16>,
}

fn unquote(text: &str) -> String {
    let text = text.trim();
    for quote in ['\'', '"'] {
        if let Some(inner) = text.strip_prefix(quote).and_then(|rest| rest.strip_suffix(quote)) {
            return inner.to_string();
        }
    }
    text.to_string()
}

fn yml_field(text: &str, key: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let line = line.trim().trim_start_matches("- ").trim();
        line.strip_prefix(key).and_then(|rest| rest.strip_prefix(':')).map(unquote).filter(|value| !value.is_empty())
    })
}

fn tychicus_route() -> Result<Option<(Value, PathBuf)>, Failure> {
    let dir = tychicus_dir()?;
    let path = dir.join("remote.json");
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(fail("tychicus_config", format!("{}: {error}", path.display()))),
    };
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|error| fail("tychicus_config", format!("{}: {error}", path.display())))?;
    Ok((value.get("route").and_then(Value::as_str) == Some("cloudflare")).then_some((value, dir)))
}

fn tychicus_tunnel() -> Result<Option<TychicusTunnel>, Failure> {
    let Some((remote, dir)) = tychicus_route()? else { return Ok(None) };
    let yml_path = dir.join(YML);
    let yml = fs::read_to_string(&yml_path)
        .map_err(|error| fail("tychicus_config", format!("{}: {error}", yml_path.display())))?;
    let bad = |what: &str| fail("tychicus_config", format!("{} has no {what}.", yml_path.display()));
    let tunnel = yml_field(&yml, "tunnel").filter(|id| is_uuid(&id.to_lowercase())).ok_or_else(|| bad("tunnel id"))?;
    let credentials = yml_field(&yml, "credentials-file")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| bad("credentials-file path"))?;
    let hostname = remote
        .get("publicUrl")
        .and_then(Value::as_str)
        .and_then(normalize_host)
        .or_else(|| yml_field(&yml, "hostname").as_deref().and_then(normalize_host))
        .ok_or_else(|| bad("hostname"))?;
    let port = remote.get("port").and_then(Value::as_u64).and_then(|port| u16::try_from(port).ok());
    Ok(Some(TychicusTunnel { hostname, tunnel: tunnel.to_lowercase(), credentials, port }))
}

fn needs_signin_cloudflare(cloudflared: &Path) -> Value {
    let command = format!("{} tunnel login", cloudflared.display());
    json!({
        "state": "needs_signin",
        "next": "needs_signin",
        "route": "cloudflare",
        "command": command,
        "message": format!("Sign in to Cloudflare first: run {command} in a terminal, choose the domain in the browser it opens, then run setup again."),
    })
}

fn needs_dns(hostname: &str, message: &str) -> Halt {
    Halt::State(json!({ "state": "needs_dns", "next": "needs_dns", "hostname": hostname, "message": message }))
}

async fn tunnel_id(cloudflared: &Path, name: &str, credentials: &Path) -> Result<String, Failure> {
    let listed = run(cloudflared, &argv(&["tunnel", "list", "--output", "json", "--name", name])).await?;
    if !listed.ok() {
        return Err(fail("cloudflare_tunnel", format!("cloudflared couldn't list your tunnels: {}", listed.text())));
    }
    if let Some(id) = find_uuid(&listed.stdout) {
        if !credentials.join(format!("{id}.json")).exists() {
            return Err(fail(
                "tunnel_elsewhere",
                format!(
                    "Your Cloudflare account has a tunnel named {name}, but its credentials aren't on this machine. If it belongs to a machine you no longer use, delete it with: cloudflared tunnel delete {name}. Then run setup again."
                ),
            ));
        }
        return Ok(id);
    }
    let made = run(cloudflared, &argv(&["tunnel", "create", name])).await?;
    match find_uuid(&made.text()) {
        Some(id) if made.ok() => Ok(id),
        _ => Err(fail("cloudflare_tunnel", format!("cloudflared couldn't make the tunnel {name}: {}", made.text()))),
    }
}

async fn route_dns(cloudflared: &Path, id: &str, host: &str) -> Result<(), Halt> {
    let out = run(cloudflared, &argv(&["tunnel", "route", "dns", id, host])).await?;
    let text = out.text();
    if !out.ok() {
        let lower = text.to_lowercase();
        let taken = ["already exists", "already in use", "record with that host"].iter().any(|s| lower.contains(s));
        let message = if taken {
            format!("{host} already has a DNS record in your Cloudflare account, so pick another name or delete that record. cloudflared said: {text}")
        } else {
            format!("cloudflared couldn't make the DNS record for {host}: {text}")
        };
        return Err(needs_dns(host, &message));
    }
    let added = text.lines().find_map(|line| {
        if let Some(at) = line.find("Added CNAME ") {
            return line[at + "Added CNAME ".len()..].split_whitespace().next().map(ToString::to_string);
        }
        let at = line.find(" is already configured to route to your tunnel")?;
        line[..at].split_whitespace().last().map(ToString::to_string)
    });
    if let Some(added) = added.map(|name| name.trim_end_matches('.').to_lowercase()) {
        if added != host {
            return Err(needs_dns(
                host,
                &format!(
                    "Your Cloudflare sign-in is for a different domain, so Cloudflare made the DNS record {added} instead of {host}. Delete {added} in the Cloudflare dashboard if you don't want it, then use a name on the signed-in domain, or sign in again choosing the domain for {host}."
                ),
            ));
        }
    }
    Ok(())
}

struct ServeState {
    handlers: BTreeMap<String, Option<String>>,
    raw_tcp: bool,
    funnel: bool,
}

fn serve_state(text: &str, dns: &str) -> Result<ServeState, String> {
    let text = if text.trim().is_empty() { "{}" } else { text };
    let value: Value =
        serde_json::from_str(text).map_err(|error| format!("tailscale serve status isn't readable: {error}"))?;
    let handlers = value
        .pointer(&format!("/Web/{}:443/Handlers", dns.replace('~', "~0").replace('/', "~1")))
        .and_then(Value::as_object)
        .map(|map| {
            map.iter()
                .map(|(path, handler)| {
                    (path.clone(), handler.get("Proxy").and_then(Value::as_str).map(ToString::to_string))
                })
                .collect()
        })
        .unwrap_or_default();
    let raw_tcp = value.pointer("/TCP/443").is_some_and(|tcp| tcp.get("HTTPS").and_then(Value::as_bool) != Some(true));
    let funnel = value
        .get("AllowFunnel")
        .and_then(Value::as_object)
        .is_some_and(|map| map.iter().any(|(key, on)| key.ends_with(":443") && on.as_bool() == Some(true)));
    Ok(ServeState { handlers, raw_tcp, funnel })
}

fn plan_mounts(state: &ServeState, wanted: &[(String, String)]) -> Result<Vec<(String, String)>, Failure> {
    if state.funnel {
        return Err(fail(
            "tailnet_funnel",
            "Funnel is on for port 443 on this machine, so these paths would be public, not tailnet only. Nothing was changed. Turn Funnel off for 443 first.",
        ));
    }
    if state.raw_tcp {
        return Err(fail(
            "tailscale_port",
            "Port 443 on this machine forwards raw TCP, so it can't serve these paths. Nothing was changed.",
        ));
    }
    let mut add = Vec::new();
    let mut taken = Vec::new();
    for (path, target) in wanted {
        match state.handlers.get(path) {
            None => add.push((path.clone(), target.clone())),
            Some(Some(proxy)) if proxy == target => {}
            Some(other) => taken.push(format!("{path} ({})", other.as_deref().unwrap_or("not a proxy"))),
        }
    }
    if !taken.is_empty() {
        return Err(fail(
            "tailnet_path",
            format!(
                "Port 443 on this machine already serves {} somewhere else. Nothing was changed. Turn those off in Tailscale first.",
                taken.join(", ")
            ),
        ));
    }
    Ok(add)
}

fn ready(route: &Route, checks: &Value, message: &str) -> Value {
    let ports = route.ports;
    json!({
        "state": "ready",
        "next": "ready",
        "route": route.route,
        "hostname": route.hostname,
        "aliases": route.aliases.clone().unwrap_or_default(),
        "ports": ports,
        "mcpUrl": format!("https://{}/mcp", route.hostname),
        "wsUrl": format!("wss://{}/v1/ws", route.hostname),
        "checks": checks,
        "message": message,
    })
}

impl Tunnel {
    fn check_base(hostname: &str) -> String {
        std::env::var("LS_TUNNEL_CHECK_URL")
            .ok()
            .filter(|value| !value.is_empty())
            .map_or_else(|| format!("https://{hostname}"), |base| base.trim_end_matches('/').to_string())
    }

    async fn check_once(&self, hostname: &str, mcp: bool, health: bool) -> Result<(), String> {
        let base = Self::check_base(hostname);
        let get = |path: &str| {
            self.http.get(format!("{base}{path}")).header("Host", hostname).timeout(Duration::from_secs(15)).send()
        };
        if health {
            let answer = get("/v1/health").await.map_err(|error| format!("https://{hostname}/v1/health: {error}"))?;
            let code = answer.status().as_u16();
            let ok = code == 200
                && answer.json::<Value>().await.ok().and_then(|value| value.get("ok").and_then(Value::as_bool))
                    == Some(true);
            if !ok {
                return Err(format!("https://{hostname}/v1/health answered {code}, not 200 from local-studio-serve."));
            }
        }
        if mcp {
            let answer = get("/mcp").await.map_err(|error| format!("https://{hostname}/mcp: {error}"))?;
            let challenge =
                answer.headers().get("www-authenticate").and_then(|value| value.to_str().ok()).unwrap_or("");
            if answer.status().as_u16() != 401 || !challenge.contains("resource_metadata") {
                return Err(format!(
                    "https://{hostname}/mcp must answer 401 with WWW-Authenticate naming resource_metadata; it answered {}. No auth, no tunnel.",
                    answer.status().as_u16()
                ));
            }
        }
        Ok(())
    }

    async fn public_checks(&self, hostnames: &[String], ports: Ports, health: bool) -> Result<Value, String> {
        let mcp = listening(ports.mcp);
        for hostname in hostnames {
            let mut last = String::new();
            let mut passed = false;
            for wait in CHECK_WAITS {
                tokio::time::sleep(Duration::from_secs(wait)).await;
                match self.check_once(hostname, mcp, health).await {
                    Ok(()) => {
                        passed = true;
                        break;
                    }
                    Err(error) => last = error,
                }
            }
            if !passed {
                return Err(last);
            }
        }
        Ok(
            json!({ "health": if health { "ok" } else { "unchecked" }, "mcp": if mcp { "ok" } else { "not_listening" } }),
        )
    }

    #[must_use]
    pub fn is_unset(&self) -> bool {
        self.route().ok().flatten().is_none() && self.keys(false).ok().flatten().is_none()
    }

    fn refuse_for(&self, kind: &str) -> Result<(), Failure> {
        match self.route()? {
            Some(route) if route.owner == "local-studio" && route.route == kind => Ok(()),
            Some(route) => {
                let (owner, off) = if route.owner == "tychicus" {
                    ("Tychicus", "tychicus remote off")
                } else {
                    ("Local Studio", "local-studio tunnel off")
                };
                Err(fail(
                    "other_route",
                    format!(
                        "This machine already has a {} route for {}, run by {owner}, and a machine has one tunnel. Turn it off first: {off}",
                        route.route, route.hostname
                    ),
                ))
            }
            None => Ok(()),
        }
    }

    fn refuse_tychicus_tunnel(hostname: Option<&str>) -> Result<(), Failure> {
        let Some((remote, _)) = tychicus_route()? else { return Ok(()) };
        let old = remote.get("publicUrl").and_then(Value::as_str).unwrap_or("a Cloudflare hostname");
        let adopt = match hostname {
            Some(host) => format!("local-studio tunnel adopt --hostname {host} --yes"),
            None => "local-studio tunnel adopt --yes".to_string(),
        };
        Err(fail(
            "tychicus_tunnel",
            format!(
                "Tychicus already runs a Cloudflare tunnel for {old} on this machine, and a machine has one tunnel. Take it over without a gap: {adopt}. Or turn it off first: tychicus remote off"
            ),
        ))
    }

    async fn start_cloudflare(
        &self,
        manager: &Manager,
        cloudflared: &Path,
        tunnel: &str,
        credentials: &Path,
        hostnames: &[String],
        ports: Ports,
    ) -> Result<Value, Failure> {
        let yml = self.dir.join(YML);
        self.private_write(YML, ingress(tunnel, credentials, hostnames, ports).as_bytes())?;
        let args = argv(&["tunnel", "--no-autoupdate", "--config", &yml.display().to_string(), "run"]);
        let log = self.dir.join("tunnel-service.log");
        if let Err(error) = manager.install(OWN, cloudflared, &args, &log).await {
            let _ = manager.remove(OWN).await;
            return Err(fail("service_failed", error));
        }
        if !manager.wait_running(OWN).await {
            let _ = manager.remove(OWN).await;
            return Err(fail(
                "service_failed",
                format!("Local Studio's tunnel service didn't stay running, so it was removed. See {}.", log.display()),
            ));
        }
        let allowed: Vec<bool> = hostnames.iter().map(|name| serve_allow(name)).collect();
        match self.public_checks(hostnames, ports, true).await {
            Ok(mut checks) => {
                checks["serveAllowed"] = json!(allowed.iter().all(|ok| *ok));
                Ok(checks)
            }
            Err(error) => {
                let _ = manager.remove(OWN).await;
                Err(fail(
                    "going_live",
                    format!("{error} The tunnel service was stopped and removed, so nothing is exposed."),
                ))
            }
        }
    }

    pub async fn setup_cloudflare(
        &self,
        hostname: String,
        aliases: Vec<String>,
        serve: Option<u16>,
        mcp: Option<u16>,
        consent: impl FnOnce(&Path, &[String]) -> bool,
    ) -> Result<Value, Failure> {
        settle(self.setup_cloudflare_steps(&hostname, &aliases, serve, mcp, consent).await)
    }

    async fn setup_cloudflare_steps(
        &self,
        hostname: &str,
        aliases: &[String],
        serve: Option<u16>,
        mcp: Option<u16>,
        consent: impl FnOnce(&Path, &[String]) -> bool,
    ) -> Result<Value, Halt> {
        let host = normalize_host(hostname)
            .ok_or_else(|| fail("bad_hostname", format!("{hostname} isn't a hostname, such as studio.example.com.")))?;
        let mut names = vec![host.clone()];
        for alias in aliases {
            let alias = normalize_host(alias)
                .ok_or_else(|| fail("bad_hostname", format!("{alias} isn't a hostname, such as old.example.com.")))?;
            if !names.contains(&alias) {
                names.push(alias);
            }
        }
        let ports = Ports { serve: serve.unwrap_or(DEFAULT_PORTS.serve), mcp: mcp.unwrap_or(DEFAULT_PORTS.mcp) };
        check_ports(ports.serve, ports.mcp)?;
        let _lock = UpLock::take(&self.dir, false)?;
        self.refuse_for("cloudflare")?;
        Self::refuse_tychicus_tunnel(Some(&host))?;
        let manager = Manager::detect().await?;
        let cloudflared =
            self.cloudflared(consent, &format!("local-studio tunnel setup --cloudflare {host} --yes")).await?;
        cloudflared.verify()?;
        let creds = cloudflared_dir()?;
        if !creds.join("cert.pem").exists() {
            return Ok(needs_signin_cloudflare(cloudflared.path()));
        }
        let name = format!("local-studio-{}", machine_slug());
        let id = tunnel_id(cloudflared.path(), &name, &creds).await?;
        for name in &names {
            route_dns(cloudflared.path(), &id, name).await?;
        }
        let credentials = creds.join(format!("{id}.json"));
        let checks = self.start_cloudflare(&manager, cloudflared.path(), &id, &credentials, &names, ports).await?;
        let route = new_route("cloudflare", &host, &names[1..], ports);
        self.write_route(&route)?;
        let mut value = ready(&route, &checks, "Live on your own Cloudflare account.");
        value["tunnel"] = json!(name);
        value["tunnelId"] = json!(id);
        Ok(value)
    }

    fn tailnet_record(&self) -> Vec<(String, String)> {
        fs::read(self.dir.join(TAILNET_RECORD))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .and_then(|value| value.get("mounts").cloned())
            .and_then(|mounts| serde_json::from_value::<Vec<(String, String)>>(mounts).ok())
            .unwrap_or_default()
    }

    fn save_tailnet_record(&self, mounts: &[(String, String)]) -> Result<(), String> {
        if mounts.is_empty() {
            let _ = fs::remove_file(self.dir.join(TAILNET_RECORD));
            return Ok(());
        }
        let bytes = serde_json::to_vec_pretty(&json!({ "mounts": mounts })).map_err(|error| error.to_string())?;
        self.private_write(TAILNET_RECORD, &bytes)
    }

    pub async fn setup_tailnet(&self, serve: Option<u16>, mcp: Option<u16>) -> Result<Value, Failure> {
        settle(self.setup_tailnet_steps(serve, mcp).await)
    }

    async fn setup_tailnet_steps(&self, serve: Option<u16>, mcp: Option<u16>) -> Result<Value, Halt> {
        let ports = Ports { serve: serve.unwrap_or(TAILNET_SERVE_PORT), mcp: mcp.unwrap_or(DEFAULT_PORTS.mcp) };
        check_ports(ports.serve, ports.mcp)?;
        let _lock = UpLock::take(&self.dir, false)?;
        self.refuse_for("tailnet")?;
        Self::refuse_tychicus_tunnel(None)?;
        let bin = tailscale_program().ok_or_else(|| {
            fail(
                "tailscale_missing",
                "Tailscale isn't installed. Install it from tailscale.com, sign in, then run setup again.",
            )
        })?;
        let status = run(&bin, &argv(&["status", "--json"])).await?;
        let parsed: Value = serde_json::from_str(&status.stdout).unwrap_or(Value::Null);
        let backend = parsed.get("BackendState").and_then(Value::as_str).unwrap_or("");
        let dns =
            parsed.pointer("/Self/DNSName").and_then(Value::as_str).unwrap_or("").trim_end_matches('.').to_lowercase();
        if backend != "Running" || normalize_host(&dns).is_none() {
            let url = parsed.get("AuthURL").and_then(Value::as_str).filter(|url| url.starts_with("https://"));
            let mut value = json!({
                "state": "needs_signin",
                "next": "needs_signin",
                "route": "tailnet",
                "command": "tailscale up",
                "message": "Tailscale isn't signed in and running on this machine. Sign in (tailscale up, or the Tailscale app), then run setup again.",
            });
            if let Some(url) = url {
                value["url"] = json!(url);
            }
            return Ok(value);
        }
        let read = run(&bin, &argv(&["serve", "status", "--json"])).await?;
        if !read.ok() {
            return Err(fail("tailnet", format!("tailscale serve status failed: {}", read.text())).into());
        }
        let wanted = tailnet_mounts(ports);
        let add = plan_mounts(&serve_state(&read.stdout, &dns)?, &wanted)?;
        let (added, problem) = tailnet_mount(&bin, &dns, &wanted, &add).await?;
        if let Some(problem) = problem {
            let left = self.unmount(&bin, &added).await;
            return Err(fail("tailnet", format!("{problem} {}", undo_note(&left))).into());
        }
        let mut record = self.tailnet_record();
        for mount in &added {
            if !record.contains(mount) {
                record.push(mount.clone());
            }
        }
        self.save_tailnet_record(&record)?;
        let allowed = allow_host(&dns).await;
        let checks = match self.public_checks(std::slice::from_ref(&dns), ports, allowed.is_ok()).await {
            Ok(checks) => checks,
            Err(error) => {
                let left = self.unmount(&bin, &added).await;
                let record: Vec<(String, String)> =
                    record.into_iter().filter(|mount| !added.contains(mount) || left.contains(&mount.0)).collect();
                self.save_tailnet_record(&record)?;
                return Err(fail("going_live", format!("{error} {}", undo_note(&left))).into());
            }
        };
        let route = new_route("tailnet", &dns, &[], ports);
        self.write_route(&route)?;
        let mut value = ready(&route, &checks, "Live on your tailnet only. No Funnel.");
        value["mounted"] = json!(added.iter().map(|(path, _)| path).collect::<Vec<_>>());
        if let Err(why) = allowed {
            let command = format!("local-studio-serve allow-host {dns} --json");
            value["state"] = json!("needs_serve_allow_host");
            value["next"] = json!(command);
            value["command"] = json!(command);
            value["message"] = json!(format!(
                "The tailnet paths are up, but local-studio-serve hasn't allowed {dns} on its local listener, so the phone path isn't checked yet. Update local-studio-serve, then run: {command} ({why})"
            ));
        }
        Ok(value)
    }

    async fn unmount(&self, bin: &Path, mounts: &[(String, String)]) -> Vec<String> {
        let mut left = Vec::new();
        for (path, _) in mounts {
            let gone = run(bin, &tailnet_off(path))
                .await
                .is_ok_and(|out| out.ok() || out.text().to_lowercase().contains("handler does not exist"));
            if !gone {
                left.push(path.clone());
            }
        }
        left
    }

    pub async fn adopt(
        &self,
        hostname: Option<String>,
        go: impl FnOnce(&Value) -> bool,
        consent: impl FnOnce(&Path, &[String]) -> bool,
    ) -> Result<Value, Failure> {
        settle(self.adopt_steps(hostname.as_deref(), go, consent).await)
    }

    async fn adopt_steps(
        &self,
        hostname: Option<&str>,
        go: impl FnOnce(&Value) -> bool,
        consent: impl FnOnce(&Path, &[String]) -> bool,
    ) -> Result<Value, Halt> {
        let _lock = UpLock::take(&self.dir, false)?;
        let old = tychicus_tunnel()?.ok_or_else(|| {
            fail(
                "no_tychicus_tunnel",
                "Tychicus runs no Cloudflare tunnel on this machine, so there is nothing to adopt. Set one up: local-studio tunnel setup --cloudflare <hostname>",
            )
        })?;
        if !old.credentials.exists() {
            return Err(fail(
                "no_credentials",
                format!(
                    "Tychicus's tunnel names {} for its credentials, and that file isn't on this machine, so it can't be adopted.",
                    old.credentials.display()
                ),
            )
            .into());
        }
        if let Some(route) = self.route()? {
            return Err(fail(
                "other_route",
                format!(
                    "This machine already has a {} route for {}, run by {}. Turn it off first: local-studio tunnel off",
                    route.route, route.hostname, route.owner
                ),
            )
            .into());
        }
        let host = match hostname {
            Some(raw) => normalize_host(raw)
                .ok_or_else(|| fail("bad_hostname", format!("{raw} isn't a hostname, such as studio.example.com.")))?,
            None => old.hostname.clone(),
        };
        let aliases = if host == old.hostname { Vec::new() } else { vec![old.hostname.clone()] };
        let ports = Ports { serve: DEFAULT_PORTS.serve, mcp: old.port.unwrap_or(DEFAULT_PORTS.mcp) };
        check_ports(ports.serve, ports.mcp)?;
        let mut names = vec![host.clone()];
        names.extend(aliases.iter().cloned());
        let manager = Manager::detect().await?;
        let command = match hostname {
            Some(_) => format!("local-studio tunnel adopt --hostname {host} --yes"),
            None => "local-studio tunnel adopt --yes".to_string(),
        };
        let plan = self.adopt_plan(&manager, &old, &names, ports, &command);
        if !go(&plan) {
            return Ok(plan);
        }
        let cloudflared = self.cloudflared(consent, &command).await?;
        cloudflared.verify()?;
        if host != old.hostname {
            if !cloudflared_dir()?.join("cert.pem").exists() {
                return Ok(needs_signin_cloudflare(cloudflared.path()));
            }
            route_dns(cloudflared.path(), &old.tunnel, &host).await?;
        }
        let was_running = manager.running(TYCHICUS).await;
        let mut stopped = false;
        let switched =
            self.adopt_switch(&manager, cloudflared.path(), &old, &names, ports, was_running, &mut stopped).await;
        let (route, checks) = match switched {
            Ok(done) => done,
            Err(cause) => {
                let new = (host != old.hostname).then_some(host.as_str());
                return Err(self.roll_back(&manager, &cause, stopped, was_running, new).await.into());
            }
        };
        let mut value = ready(&route, &checks, "Local Studio runs Tychicus's tunnel now.");
        value["tunnelId"] = json!(old.tunnel);
        match join_tychicus().await {
            Ok(()) => value["tychicus"] = json!("joined"),
            Err(why) => {
                value["state"] = json!("needs_tychicus_join");
                value["next"] = json!("needs_tychicus_join");
                value["tychicus"] = json!("not_joined");
                value["command"] = json!("tychicus remote join");
                value["message"] = json!(format!(
                    "Local Studio runs the tunnel now. Tychicus still needs to join it: run tychicus remote join. ({why})"
                ));
            }
        }
        Ok(value)
    }

    fn adopt_plan(
        &self,
        manager: &Manager,
        old: &TychicusTunnel,
        names: &[String],
        ports: Ports,
        command: &str,
    ) -> Value {
        let mut steps = Vec::new();
        if names.len() > 1 {
            steps.push(format!("Add the DNS record {} to tunnel {}.", names[0], old.tunnel));
        }
        steps.push(format!(
            "Write {} with the eleven paths for {}, using the same tunnel and its credentials file {} (never read).",
            self.dir.join(YML).display(),
            names.join(" and "),
            old.credentials.display()
        ));
        steps.push(format!("Install and start {} ({}).", OWN.what, manager.file(OWN).display()));
        steps.push(format!("Check {} answer.", names.join(" and ")));
        steps.push(format!(
            "Stop {} ({}), keeping its files, then check again.",
            TYCHICUS.what,
            manager.file(TYCHICUS).display()
        ));
        steps.push(format!("Write {}.", self.route_path().display()));
        steps.push("Run tychicus remote join.".into());
        json!({
            "state": "confirm",
            "next": "confirm",
            "route": "cloudflare",
            "hostname": names[0],
            "aliases": &names[1..],
            "tunnelId": old.tunnel,
            "ports": RoutePorts::from(ports),
            "plan": steps,
            "command": command,
            "message": format!("Adopt Tychicus's Cloudflare tunnel for {} without a gap. Nothing changes until you confirm: {command}", old.hostname),
        })
    }

    async fn roll_back(
        &self,
        manager: &Manager,
        cause: &str,
        stopped: bool,
        was_running: bool,
        new: Option<&str>,
    ) -> Failure {
        let _ = manager.remove(OWN).await;
        let _ = fs::remove_file(self.dir.join(YML));
        let mut message = format!("{cause} Adopting was rolled back: Local Studio's tunnel service was removed");
        if stopped {
            match manager.start(TYCHICUS).await {
                Ok(()) if manager.wait_running(TYCHICUS).await => {
                    message.push_str(", and Tychicus's tunnel runs again.");
                }
                Ok(()) => {
                    let _ = write!(
                        message,
                        ", but Tychicus's tunnel didn't come back up. Check {}.",
                        manager.file(TYCHICUS).display()
                    );
                }
                Err(error) => {
                    let _ = write!(message, ", but Tychicus's tunnel didn't start again: {error}");
                }
            }
        } else if was_running {
            message.push_str("; Tychicus's tunnel kept running.");
        } else {
            message.push('.');
        }
        if let Some(host) = new {
            let _ = write!(message, " The DNS record for {host} stays on the tunnel.");
        }
        fail("adopt_rolled_back", message)
    }

    #[allow(clippy::too_many_arguments)]
    async fn adopt_switch(
        &self,
        manager: &Manager,
        cloudflared: &Path,
        old: &TychicusTunnel,
        names: &[String],
        ports: Ports,
        was_running: bool,
        stopped: &mut bool,
    ) -> Result<(Route, Value), String> {
        let checks = self
            .start_cloudflare(manager, cloudflared, &old.tunnel, &old.credentials, names, ports)
            .await
            .map_err(|failure| failure.message)?;
        if was_running {
            manager.stop(TYCHICUS).await?;
            *stopped = true;
            self.public_checks(names, ports, true)
                .await
                .map_err(|error| format!("After Tychicus's tunnel stopped: {error}"))?;
        }
        let route = new_route("cloudflare", &names[0], &names[1..], ports);
        self.write_route(&route)?;
        Ok((route, checks))
    }

    pub async fn off(&self) -> Result<Value, Failure> {
        let _lock = UpLock::take(&self.dir, true)?;
        let route = self.route()?;
        if let Some(route) = route.as_ref().filter(|route| route.owner != "local-studio") {
            return Err(fail(
                "not_ours",
                format!(
                    "The {} route for {} is run by {}, not Local Studio. Turn it off there: {} remote off",
                    route.route, route.hostname, route.owner, route.owner
                ),
            ));
        }
        let mut removed = Vec::new();
        match Manager::detect().await {
            Ok(manager) => {
                if manager.remove(OWN).await.map_err(|error| fail("service_failed", error))? {
                    removed.push("service".to_string());
                }
            }
            Err(failure) if failure.code == "unsupported" => {}
            Err(failure) => return Err(failure),
        }
        let mounts = self.tailnet_record();
        if !mounts.is_empty() {
            let bin = tailscale_program().ok_or_else(|| {
                fail("tailscale_missing", "Tailscale isn't on this machine, so the tailnet paths can't be removed.")
            })?;
            let left = self.unmount(&bin, &mounts).await;
            let kept: Vec<(String, String)> = mounts.iter().filter(|(path, _)| left.contains(path)).cloned().collect();
            self.save_tailnet_record(&kept)?;
            if !left.is_empty() {
                return Err(fail(
                    "tailnet",
                    format!("tailscale serve couldn't remove {}. route.json is kept; run off again.", left.join(", ")),
                ));
            }
            removed.push("tailnet paths".to_string());
        }
        if route.as_ref().is_some_and(Route::is_hosted) && self.running().is_some() {
            self.stop();
            let _ = fs::remove_file(self.token_path());
            removed.push("cloudflared".to_string());
        }
        if let Some(route) = &route {
            serve_disallow(&route.hostname);
            for alias in route.aliases.iter().flatten() {
                serve_disallow(alias);
            }
        }
        if fs::remove_file(self.dir.join(YML)).is_ok() {
            removed.push(YML.to_string());
        }
        if route.is_some() {
            fs::remove_file(self.route_path()).map_err(|error| error.to_string())?;
            removed.push("route.json".to_string());
        }
        let message = if removed.is_empty() {
            "Nothing to turn off.".to_string()
        } else {
            "Off. The Cloudflare tunnel, its DNS records and the tailnet node stay.".to_string()
        };
        Ok(json!({ "state": "off", "next": "none", "removed": removed, "message": message }))
    }
}

async fn tailnet_mount(
    bin: &Path,
    dns: &str,
    wanted: &[(String, String)],
    add: &[(String, String)],
) -> Result<(Vec<(String, String)>, Option<String>), Failure> {
    let mut added: Vec<(String, String)> = Vec::new();
    let mut problem = None;
    for (path, target) in add {
        match run(bin, &tailnet_on(path, target)).await {
            Ok(out) if out.ok() => added.push((path.clone(), target.clone())),
            Ok(out) => {
                problem = Some(format!("tailscale serve couldn't mount {path}: {}", out.text()));
                break;
            }
            Err(error) => {
                problem = Some(error);
                break;
            }
        }
    }
    if problem.is_none() {
        let after = run(bin, &argv(&["serve", "status", "--json"])).await?;
        match serve_state(&after.stdout, dns) {
            Ok(state) if state.funnel => problem = Some("Funnel came on for port 443.".into()),
            Ok(state) => {
                let wrong: Vec<&str> = wanted
                    .iter()
                    .filter(|(path, target)| state.handlers.get(path) != Some(&Some(target.clone())))
                    .map(|(path, _)| path.as_str())
                    .collect();
                if !wrong.is_empty() {
                    problem = Some(format!("tailscale serve doesn't send {} where it should.", wrong.join(", ")));
                }
            }
            Err(error) => problem = Some(error),
        }
    }
    Ok((added, problem))
}

async fn allow_host(hostname: &str) -> Result<(), String> {
    let out = run(&serve_program(), &argv(&["allow-host", hostname, "--json"])).await?;
    if out.ok() {
        return Ok(());
    }
    Err(format!("local-studio-serve allow-host exited {}: {}", out.code, out.text()))
}

fn undo_note(left: &[String]) -> String {
    if left.is_empty() {
        "The tailnet paths this run added were removed.".to_string()
    } else {
        format!("Removing {} failed too; check tailscale serve status.", left.join(", "))
    }
}

async fn join_tychicus() -> Result<(), String> {
    let bin = tool("LS_TYCHICUS", "tychicus").ok_or("tychicus isn't on PATH")?;
    let out = run(&bin, &argv(&["remote", "join", "--json"])).await?;
    if out.ok() {
        return Ok(());
    }
    if out.code == 2 {
        return Err("this tychicus has no remote join yet; update Tychicus".into());
    }
    let said = serde_json::from_str::<Value>(out.stdout.trim())
        .ok()
        .and_then(|value| value.get("message").and_then(Value::as_str).map(ToString::to_string))
        .unwrap_or_else(|| out.text());
    Err(format!("tychicus remote join exited {}: {said}", out.code))
}
