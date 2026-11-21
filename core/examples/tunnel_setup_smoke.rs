use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use serde_json::{json, Value};

const SHIM_TUNNEL: &str = "7d3c9a52-1b2e-4f60-9c1d-2a3b4c5d6e7f";
const TYCHICUS_TUNNEL: &str = "0b6c1e2d-3f40-4a5b-8c7d-9e0f1a2b3c4d";
const DNS: &str = "studio-mac.tail1234.ts.net";

const CLOUDFLARED: &str = r#"#!/bin/sh
echo "cloudflared $*" >> "$SHIM_LOG"
last=
for a; do last=$a; done
case "$1 $2" in
  "tunnel list")
    if [ -f "$SHIM_STATE/cf-$last" ]; then printf '[{"id":"%s","name":"%s"}]\n' "$(cat "$SHIM_STATE/cf-$last")" "$last"; else echo '[]'; fi ;;
  "tunnel create")
    id=7d3c9a52-1b2e-4f60-9c1d-2a3b4c5d6e7f
    mkdir -p "$HOME/.cloudflared"
    echo '{}' > "$HOME/.cloudflared/$id.json"
    echo "$id" > "$SHIM_STATE/cf-$3"
    echo "Created tunnel $3 with id $id" ;;
  "tunnel route")
    echo "2026-11-21T00:00:00Z INF Added CNAME $5 which will route to this tunnel tunnelID=$4" >&2 ;;
esac
exit 0
"#;

const LAUNCHCTL: &str = r#"#!/bin/sh
echo "launchctl $*" >> "$SHIM_LOG"
svc="$SHIM_STATE/svc"
mkdir -p "$svc"
case "$1" in
  print)
    label=${2##*/}
    [ -f "$svc/$label" ] || exit 113
    if [ -f "$SHIM_STATE/fail-$label" ]; then echo "state = not running"; else echo "state = running"; fi ;;
  bootstrap)
    label=$(basename "$3" .plist)
    [ -f "$3" ] || { echo "Bootstrap failed: 5: Input/output error" >&2; exit 5; }
    touch "$svc/$label" ;;
  bootout)
    label=${2##*/}
    [ -f "$svc/$label" ] || { echo "Boot-out failed: 3: No such process" >&2; exit 3; }
    rm -f "$svc/$label" ;;
esac
exit 0
"#;

const SYSTEMCTL: &str = r#"#!/bin/sh
echo "systemctl $*" >> "$SHIM_LOG"
svc="$SHIM_STATE/svc"
mkdir -p "$svc"
[ "$1" = "--user" ] && shift
last=
for a; do last=$a; done
case "$1" in
  start|restart)
    [ -f "$HOME/.config/systemd/user/$2" ] || exit 5
    touch "$svc/$2" ;;
  is-active)
    if [ -f "$svc/$2" ] && [ ! -f "$SHIM_STATE/fail-$2" ]; then echo active; else echo inactive; exit 3; fi ;;
  stop) rm -f "$svc/$2" ;;
  disable) rm -f "$svc/$last" ;;
esac
exit 0
"#;

const TAILSCALE: &str = r#"#!/bin/sh
echo "tailscale $*" >> "$SHIM_LOG"
dns=studio-mac.tail1234.ts.net
mounts="$SHIM_STATE/ts-mounts"
touch "$mounts"
case "$*" in *unnel*) echo "funnel is not allowed in this smoke" >&2; exit 9 ;; esac
if [ "$1" = status ]; then
  if [ -f "$SHIM_STATE/ts-up" ]; then printf '{"BackendState":"Running","Self":{"DNSName":"%s."}}\n' "$dns"
  else echo '{"BackendState":"NeedsLogin","AuthURL":"https://login.tailscale.com/a/smoke","Self":{"DNSName":""}}'; fi
  exit 0
fi
if [ "$1 $2" = "serve status" ]; then
  printf '{"Web":{"%s:443":{"Handlers":{' "$dns"
  awk 'NF==2 {if(n++)printf(","); printf("\"%s\":{\"Proxy\":\"%s\"}", $1, $2)}' "$mounts"
  printf '}}}}\n'
  exit 0
fi
if [ "$1" = serve ]; then
  path=
  last=
  for a; do case "$a" in --set-path=*) path=${a#--set-path=} ;; esac; last=$a; done
  if [ "$last" = off ]; then
    awk -v p="$path" '$1 == p {f=1} END {exit !f}' "$mounts" || { echo "error: handler does not exist" >&2; exit 1; }
  fi
  awk -v p="$path" '$1 != p' "$mounts" > "$mounts.new"
  [ "$last" = off ] || echo "$path $last" >> "$mounts.new"
  mv "$mounts.new" "$mounts"
fi
exit 0
"#;

const TYCHICUS: &str = r#"#!/bin/sh
echo "tychicus $*" >> "$SHIM_LOG"
if [ "$1 $2" = "remote join" ]; then echo '{"ok":true,"state":"on","step":"joined"}'; exit 0; fi
exit 2
"#;

const SERVE: &str = r#"#!/bin/sh
echo "local-studio-serve $*" >> "$SHIM_LOG"
exit 0
"#;

struct Smoke {
    bin: PathBuf,
    root: PathBuf,
    home: PathBuf,
    data: PathBuf,
    tychicus: PathBuf,
    state: PathBuf,
    log: PathBuf,
    shims: PathBuf,
    check_url: String,
    stub_port: u16,
    closed_port: u16,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => {
            println!("tunnel_setup_smoke: every step passed");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("tunnel_setup_smoke: FAILED: {error}");
            ExitCode::FAILURE
        }
    }
}

fn ensure(ok: bool, what: impl Into<String>) -> Result<(), String> {
    if ok {
        Ok(())
    } else {
        Err(what.into())
    }
}

fn shim(dir: &Path, name: &str, body: &str) -> Result<PathBuf, String> {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    fs::write(&path, body).map_err(|e| format!("{}: {e}", path.display()))?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
    Ok(path)
}

fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path).map_or(0, |meta| meta.permissions().mode() & 0o777)
}

fn append(log: &Path, line: &str) {
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(log) {
        let _ = writeln!(file, "{line}");
    }
}

fn stub(listener: &TcpListener, log: &Path, state: &Path, tychicus_service: &str) {
    for stream in listener.incoming().flatten() {
        let _ = answer(stream, log, state, tychicus_service);
    }
}

fn answer(mut stream: TcpStream, log: &Path, state: &Path, tychicus_service: &str) -> std::io::Result<()> {
    stream.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let head = String::from_utf8_lossy(&buf).to_string();
    let path = head.lines().next().and_then(|line| line.split(' ').nth(1)).unwrap_or("").to_string();
    let host = head
        .lines()
        .find_map(|line| line.strip_prefix("host: ").or_else(|| line.strip_prefix("Host: ")))
        .unwrap_or("")
        .trim()
        .to_string();
    let tychicus_down = !state.join("svc").join(tychicus_service).exists();
    let (code, extra, body) = match path.as_str() {
        "/v1/health" if state.join("fail-after-stop").exists() && tychicus_down => (503, String::new(), "{}"),
        "/v1/health" => (200, String::new(), r#"{"ok":true}"#),
        "/mcp" => (
            401,
            format!(
                "WWW-Authenticate: Bearer resource_metadata=\"https://{host}/.well-known/oauth-protected-resource/mcp\"\r\n"
            ),
            r#"{"error":"unauthorized"}"#,
        ),
        _ => (404, String::new(), "{}"),
    };
    append(log, &format!("check {host} {path} {code}"));
    let reply = format!(
        "HTTP/1.1 {code} X\r\nContent-Type: application/json\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(reply.as_bytes())
}

impl Smoke {
    fn service_name(own: bool) -> &'static str {
        match (cfg!(target_os = "macos"), own) {
            (true, true) => "com.localstudio.tunnel",
            (true, false) => "com.tychicus.cloudflared",
            (false, true) => "local-studio-tunnel.service",
            (false, false) => "tychicus-cloudflared.service",
        }
    }

    fn service_file(&self, own: bool) -> PathBuf {
        let name = Self::service_name(own);
        if cfg!(target_os = "macos") {
            self.home.join("Library").join("LaunchAgents").join(format!("{name}.plist"))
        } else {
            self.home.join(".config").join("systemd").join("user").join(name)
        }
    }

    fn service_running(&self, own: bool) -> bool {
        self.state.join("svc").join(Self::service_name(own)).exists()
    }

    fn route_path(&self) -> PathBuf {
        self.data.join("tunnel").join("route.json")
    }

    fn yml_path(&self) -> PathBuf {
        self.data.join("tunnel").join("cloudflared.yml")
    }

    fn log_lines(&self) -> Vec<String> {
        fs::read_to_string(&self.log).unwrap_or_default().lines().map(ToString::to_string).collect()
    }

    fn mark(&self) -> usize {
        self.log_lines().len()
    }

    fn since(&self, mark: usize) -> Vec<String> {
        self.log_lines().into_iter().skip(mark).collect()
    }

    fn ls(&self, args: &[&str]) -> Result<(i32, Value), String> {
        let shim = |name: &str| self.shims.join(name);
        let out = Command::new(&self.bin)
            .args(args)
            .env("HOME", &self.home)
            .env("LOCAL_STUDIO_DATA_DIR", &self.data)
            .env("TYCHICUS_CONFIG_DIR", &self.tychicus)
            .env("LS_CLOUDFLARED", shim("cloudflared"))
            .env("LS_LAUNCHCTL", shim("launchctl"))
            .env("LS_SYSTEMCTL", shim("systemctl"))
            .env("LS_TAILSCALE", shim("tailscale"))
            .env("LS_TYCHICUS", shim("tychicus"))
            .env("LS_SERVE_BIN", shim("local-studio-serve"))
            .env("LS_TUNNEL_CHECK_URL", &self.check_url)
            .env("LS_TUNNEL_NO_BROWSER", "1")
            .env("SHIM_LOG", &self.log)
            .env("SHIM_STATE", &self.state)
            .env_remove("LS_TUNNEL_SERVICE")
            .env_remove("TUNNEL_ORIGIN_CERT")
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("{}: {e}", self.bin.display()))?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        let code = out.status.code().unwrap_or(-1);
        println!("$ local-studio {}\n  exit {code}: {}", args.join(" "), stdout.trim());
        let line = stdout.lines().rev().find(|line| !line.trim().is_empty()).unwrap_or("");
        let value: Value = serde_json::from_str(line).map_err(|e| format!("not one JSON state ({e}): {stdout}"))?;
        Ok((code, value))
    }

    fn has(lines: &[String], text: &str) -> bool {
        lines.iter().any(|line| line.contains(text))
    }

    fn at(lines: &[String], text: &str) -> Result<usize, String> {
        lines.iter().position(|line| line.contains(text)).ok_or_else(|| format!("the log has no line with {text:?}"))
    }

    fn stop_line(own: bool) -> String {
        if cfg!(target_os = "macos") {
            format!("launchctl bootout gui/{}/{}", uid(), Self::service_name(own))
        } else if own {
            format!("systemctl --user disable --now {}", Self::service_name(own))
        } else {
            format!("systemctl --user stop {}", Self::service_name(own))
        }
    }

    fn started_line(&self, own: bool) -> String {
        if cfg!(target_os = "macos") {
            format!("launchctl bootstrap gui/{} {}", uid(), self.service_file(own).display())
        } else if own {
            format!("systemctl --user restart {}", Self::service_name(own))
        } else {
            format!("systemctl --user start {}", Self::service_name(own))
        }
    }
}

fn uid() -> String {
    Command::new("/usr/bin/id")
        .arg("-u")
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .unwrap_or_default()
}

fn free_port() -> Result<u16, String> {
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    listener.local_addr().map(|addr| addr.port()).map_err(|e| e.to_string())
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [bin, scratch] = args.as_slice() else {
        return Err("usage: tunnel_setup_smoke <local-studio binary> <scratch dir>".into());
    };
    let bin = fs::canonicalize(bin).map_err(|e| format!("{bin}: {e}"))?;
    let root = PathBuf::from(scratch).join(format!("tunnel-setup-{}", std::process::id()));
    let dirs = ["home", "data", "tychicus", "state", "shims"].map(|name| root.join(name));
    for dir in &dirs {
        fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let root = fs::canonicalize(&root).map_err(|e| e.to_string())?;
    let [home, data, tychicus, state, shims] =
        ["home", "data", "tychicus", "state", "shims"].map(|name| root.join(name));
    for (name, body) in [
        ("cloudflared", CLOUDFLARED),
        ("launchctl", LAUNCHCTL),
        ("systemctl", SYSTEMCTL),
        ("tailscale", TAILSCALE),
        ("tychicus", TYCHICUS),
        ("local-studio-serve", SERVE),
    ] {
        shim(&shims, name, body)?;
    }
    let log = root.join("shim.log");
    fs::write(&log, "").map_err(|e| e.to_string())?;
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    let stub_port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let closed_port = free_port()?;
    {
        let (log, state) = (log.clone(), state.clone());
        std::thread::spawn(move || stub(&listener, &log, &state, Smoke::service_name(false)));
    }
    let smoke = Smoke {
        bin,
        root: root.clone(),
        home,
        data,
        tychicus,
        state,
        log,
        shims,
        check_url: format!("http://127.0.0.1:{stub_port}"),
        stub_port,
        closed_port,
    };
    println!("scratch: {}", smoke.root.display());
    cloudflare(&smoke)?;
    tailnet(&smoke)?;
    adopt(&smoke)?;
    adopt_fails_to_start(&smoke)?;
    adopt_fails_after_stop(&smoke)?;
    Ok(())
}

fn cloudflare(s: &Smoke) -> Result<(), String> {
    let closed = s.closed_port.to_string();
    let args = ["tunnel", "setup", "--cloudflare", "studio.example.com", "--alias", "alt.example.com", "--json"];
    let mut first: Vec<&str> = args.to_vec();
    first.extend(["--mcp", &closed]);
    let mark = s.mark();
    let (code, value) = s.ls(&first)?;
    ensure(code == 0 && value["state"] == "needs_signin", "setup without cert.pem must answer needs_signin")?;
    ensure(
        value["command"].as_str().is_some_and(|c| c.ends_with("cloudflared tunnel login")),
        "needs_signin must carry the cloudflared tunnel login command",
    )?;
    ensure(!Smoke::has(&s.since(mark), "tunnel create"), "nothing may be created before sign-in")?;
    ensure(!s.route_path().exists(), "no route.json before sign-in")?;

    let certs = s.home.join(".cloudflared");
    fs::create_dir_all(&certs).map_err(|e| e.to_string())?;
    fs::write(certs.join("cert.pem"), "smoke: not a certificate\n").map_err(|e| e.to_string())?;
    let mark = s.mark();
    let (code, value) = s.ls(&first)?;
    ensure(code == 0 && value["state"] == "ready", format!("setup --cloudflare must be ready: {value}"))?;
    ensure(value["checks"]["mcp"] == "not_listening" && value["checks"]["health"] == "ok", "checks with mcp closed")?;
    let lines = s.since(mark);
    for want in [
        "cloudflared tunnel list --output json --name local-studio-".to_string(),
        "cloudflared tunnel create local-studio-".to_string(),
        format!("cloudflared tunnel route dns {SHIM_TUNNEL} studio.example.com"),
        format!("cloudflared tunnel route dns {SHIM_TUNNEL} alt.example.com"),
        s.started_line(true),
        "local-studio-serve allow-tunnel-host studio.example.com".to_string(),
        "local-studio-serve allow-tunnel-host alt.example.com".to_string(),
        "check studio.example.com /v1/health 200".to_string(),
        "check alt.example.com /v1/health 200".to_string(),
    ] {
        Smoke::at(&lines, &want)?;
    }
    ensure(
        Smoke::at(&lines, &s.started_line(true))? < Smoke::at(&lines, "check studio.example.com /v1/health")?,
        "checks run after the service starts",
    )?;
    ensure(s.service_running(true), "Local Studio's service must be running")?;
    let route: Value =
        serde_json::from_slice(&fs::read(s.route_path()).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    let want = json!({"v":1,"owner":"local-studio","route":"cloudflare","hostname":"studio.example.com","aliases":["alt.example.com"],"ports":{"mcp":s.closed_port,"serve":18094}});
    ensure(route == want, format!("route.json {route} != {want}"))?;
    ensure(
        mode(&s.route_path()) == 0o600 && mode(&s.yml_path()) == 0o600,
        "route.json and cloudflared.yml must be 0600",
    )?;
    let yml = fs::read_to_string(s.yml_path()).map_err(|e| e.to_string())?;
    ensure(yml.matches("    path: '^/").count() == 22, "the yml must hold eleven paths for each of two hostnames")?;
    ensure(yml.trim_end().ends_with("- service: http_status:404"), "the yml must end in a 404")?;
    ensure(yml.contains(&format!("tunnel: {SHIM_TUNNEL}")), "the yml must name the created tunnel")?;
    let service = fs::read_to_string(s.service_file(true)).map_err(|e| e.to_string())?;
    ensure(
        service.contains(&s.shims.join("cloudflared").display().to_string())
            && service.contains("--no-autoupdate")
            && service.contains(&s.yml_path().display().to_string()),
        "the service must run cloudflared with Local Studio's yml",
    )?;

    let stub = s.stub_port.to_string();
    let mut again: Vec<&str> = args.to_vec();
    again.extend(["--mcp", &stub]);
    let mark = s.mark();
    let (code, value) = s.ls(&again)?;
    ensure(code == 0 && value["state"] == "ready" && value["checks"]["mcp"] == "ok", "setup again with mcp listening")?;
    let lines = s.since(mark);
    ensure(!Smoke::has(&lines, "tunnel create"), "setup again must reuse the tunnel")?;
    Smoke::at(&lines, "check studio.example.com /mcp 401")?;

    off(s)?;
    println!("ok: setup --cloudflare, needs_signin, ready, again, off");
    Ok(())
}

fn off(s: &Smoke) -> Result<(), String> {
    let mark = s.mark();
    let (code, value) = s.ls(&["tunnel", "off", "--json"])?;
    ensure(code == 0 && value["state"] == "off", "off must answer off")?;
    ensure(!s.route_path().exists() && !s.yml_path().exists(), "off removes route.json and cloudflared.yml")?;
    ensure(!s.service_file(true).exists() && !s.service_running(true), "off removes Local Studio's service")?;
    let lines = s.since(mark);
    ensure(!Smoke::has(&lines, "tunnel delete") && !Smoke::has(&lines, "route dns"), "off leaves the tunnel and DNS")?;
    Ok(())
}

fn tailnet(s: &Smoke) -> Result<(), String> {
    let stub = s.stub_port.to_string();
    let args = ["tunnel", "setup", "--tailnet", "--mcp", &stub, "--json"];
    let (code, value) = s.ls(&args)?;
    ensure(code == 0 && value["state"] == "needs_signin", "tailnet while signed out answers needs_signin")?;
    ensure(value["url"] == "https://login.tailscale.com/a/smoke", "needs_signin carries Tailscale's sign-in URL")?;
    fs::write(s.state.join("ts-up"), "").map_err(|e| e.to_string())?;
    let mark = s.mark();
    let (code, value) = s.ls(&args)?;
    ensure(code == 0 && value["state"] == "ready", format!("setup --tailnet must be ready: {value}"))?;
    ensure(value["hostname"] == DNS && value["ports"]["serve"] == 18093, "tailnet hostname and the local listener")?;
    let lines = s.since(mark);
    let mounts: Vec<&String> =
        lines.iter().filter(|line| line.starts_with("tailscale serve --bg --yes --https=443 --set-path=")).collect();
    ensure(mounts.len() == 11, format!("eleven set-path calls, got {}", mounts.len()))?;
    for path in ["/v1/health", "/v1/ws"] {
        Smoke::at(&lines, &format!("--set-path={path} http://127.0.0.1:18093{path}"))?;
    }
    for path in ["/mcp", "/.well-known/oauth-authorization-server/mcp", "/revoke", "/register", "/token", "/authorize"]
    {
        Smoke::at(&lines, &format!("--set-path={path} http://127.0.0.1:{stub}{path}"))?;
    }
    ensure(!s.log_lines().iter().any(|line| line.to_lowercase().contains("funnel")), "tailnet never mentions Funnel")?;
    Smoke::at(&lines, &format!("local-studio-serve allow-tunnel-host {DNS}"))?;
    Smoke::at(&lines, &format!("check {DNS} /mcp 401"))?;
    let route: Value =
        serde_json::from_slice(&fs::read(s.route_path()).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    ensure(
        route["route"] == "tailnet" && route["hostname"] == DNS && route["aliases"] == json!([]),
        "tailnet route.json",
    )?;
    let mark = s.mark();
    let (code, value) = s.ls(&["tunnel", "off", "--json"])?;
    ensure(code == 0 && value["state"] == "off", "off after tailnet")?;
    let lines = s.since(mark);
    let removed = lines
        .iter()
        .filter(|line| line.starts_with("tailscale serve --https=443 --set-path=") && line.ends_with(" off"))
        .count();
    ensure(removed == 11, format!("off removes the eleven paths it added, got {removed}"))?;
    let left = fs::read_to_string(s.state.join("ts-mounts")).unwrap_or_default();
    ensure(left.trim().is_empty() && !s.route_path().exists(), "no tailnet paths and no route.json after off")?;
    println!("ok: setup --tailnet, needs_signin, ready, off");
    Ok(())
}

fn tychicus_fixture(s: &Smoke) -> Result<(), String> {
    let credentials = s.home.join(".cloudflared").join(format!("{TYCHICUS_TUNNEL}.json"));
    fs::write(&credentials, "{}\n").map_err(|e| e.to_string())?;
    let remote = json!({"publicUrl":"https://old.example.com","port":s.stub_port,"route":"cloudflare"});
    fs::write(s.tychicus.join("remote.json"), remote.to_string()).map_err(|e| e.to_string())?;
    let yml = format!(
        "tunnel: {TYCHICUS_TUNNEL}\ncredentials-file: {}\nno-autoupdate: true\ningress:\n  - hostname: old.example.com\n    service: http://127.0.0.1:{}\n  - service: http_status:404\n",
        credentials.display(),
        s.stub_port
    );
    fs::write(s.tychicus.join("cloudflared.yml"), yml).map_err(|e| e.to_string())?;
    let file = s.service_file(false);
    fs::create_dir_all(file.parent().ok_or("no parent")?).map_err(|e| e.to_string())?;
    fs::write(&file, "tychicus service fixture\n").map_err(|e| e.to_string())?;
    fs::create_dir_all(s.state.join("svc")).map_err(|e| e.to_string())?;
    fs::write(s.state.join("svc").join(Smoke::service_name(false)), "").map_err(|e| e.to_string())?;
    Ok(())
}

fn adopt(s: &Smoke) -> Result<(), String> {
    tychicus_fixture(s)?;
    let mark = s.mark();
    let (code, value) = s.ls(&["tunnel", "adopt", "--hostname", "new.example.com", "--json"])?;
    ensure(code == 0 && value["state"] == "confirm", "adopt without --yes answers confirm")?;
    ensure(value["plan"].as_array().is_some_and(|plan| !plan.is_empty()), "confirm carries the plan")?;
    let lines = s.since(mark);
    ensure(
        !Smoke::has(&lines, "bootstrap")
            && !Smoke::has(&lines, "bootout")
            && !Smoke::has(&lines, "route dns")
            && !Smoke::has(&lines, "systemctl"),
        "confirm changes nothing",
    )?;
    ensure(!s.route_path().exists() && !s.service_file(true).exists(), "confirm writes nothing")?;

    let mark = s.mark();
    let (code, value) = s.ls(&["tunnel", "adopt", "--hostname", "new.example.com", "--yes", "--json"])?;
    ensure(
        code == 0 && value["state"] == "ready" && value["tychicus"] == "joined",
        format!("adopt must be ready: {value}"),
    )?;
    let lines = s.since(mark);
    Smoke::at(&lines, &format!("cloudflared tunnel route dns {TYCHICUS_TUNNEL} new.example.com"))?;
    let started = Smoke::at(&lines, &s.started_line(true))?;
    let checked = Smoke::at(&lines, "check new.example.com /v1/health 200")?;
    let old_checked = Smoke::at(&lines, "check old.example.com /v1/health 200")?;
    let stopped = Smoke::at(&lines, &Smoke::stop_line(false))?;
    let joined = Smoke::at(&lines, "tychicus remote join --json")?;
    ensure(
        started < checked && checked < stopped && old_checked < stopped && stopped < joined,
        format!(
            "order must be start {started} < checks {checked},{old_checked} < Tychicus stop {stopped} < join {joined}"
        ),
    )?;
    ensure(
        !s.service_running(false) && s.service_file(false).exists(),
        "Tychicus's service is stopped, its file kept",
    )?;
    ensure(s.service_running(true), "Local Studio's service runs")?;
    let route: Value =
        serde_json::from_slice(&fs::read(s.route_path()).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    ensure(
        route["hostname"] == "new.example.com"
            && route["aliases"] == json!(["old.example.com"])
            && route["ports"]["mcp"] == s.stub_port,
        format!("adopt route.json: {route}"),
    )?;
    let yml = fs::read_to_string(s.yml_path()).map_err(|e| e.to_string())?;
    ensure(
        yml.contains(&format!("tunnel: {TYCHICUS_TUNNEL}"))
            && yml.contains(&format!("{TYCHICUS_TUNNEL}.json"))
            && yml.contains("hostname: new.example.com")
            && yml.contains("hostname: old.example.com"),
        "the yml keeps Tychicus's tunnel and credentials path and both names",
    )?;
    off(s)?;
    println!("ok: adopt confirm, adopt --yes, Tychicus stopped after checks, join, off");
    Ok(())
}

fn rolled_back(s: &Smoke, value: &Value, code: i32) -> Result<(), String> {
    ensure(
        code == 1 && value["state"] == "error" && value["code"] == "adopt_rolled_back",
        format!("adopt_rolled_back: {value}"),
    )?;
    ensure(s.service_running(false), "Tychicus's service runs again")?;
    ensure(!s.service_file(true).exists() && !s.service_running(true), "no Local Studio service is left")?;
    ensure(!s.route_path().exists() && !s.yml_path().exists(), "no route.json or Local Studio yml is left")
}

fn adopt_fails_to_start(s: &Smoke) -> Result<(), String> {
    tychicus_fixture(s)?;
    let fail = s.state.join(format!("fail-{}", Smoke::service_name(true)));
    fs::write(&fail, "").map_err(|e| e.to_string())?;
    let mark = s.mark();
    let (code, value) = s.ls(&["tunnel", "adopt", "--hostname", "new.example.com", "--yes", "--json"])?;
    fs::remove_file(&fail).map_err(|e| e.to_string())?;
    rolled_back(s, &value, code)?;
    let lines = s.since(mark);
    ensure(
        !Smoke::has(&lines, &Smoke::stop_line(false)),
        "Tychicus's service is never stopped when the new one fails",
    )?;
    ensure(!Smoke::has(&lines, "tychicus remote join"), "no join after a rollback")?;
    println!("ok: adopt rolls back when Local Studio's service fails to start");
    Ok(())
}

fn adopt_fails_after_stop(s: &Smoke) -> Result<(), String> {
    tychicus_fixture(s)?;
    let fail = s.state.join("fail-after-stop");
    fs::write(&fail, "").map_err(|e| e.to_string())?;
    let mark = s.mark();
    let (code, value) = s.ls(&["tunnel", "adopt", "--yes", "--json"])?;
    fs::remove_file(&fail).map_err(|e| e.to_string())?;
    rolled_back(s, &value, code)?;
    let lines = s.since(mark);
    let stopped = Smoke::at(&lines, &Smoke::stop_line(false))?;
    let restarted = Smoke::at(&lines, &s.started_line(false))?;
    ensure(stopped < restarted, "Tychicus's service is started again after it was stopped")?;
    ensure(!Smoke::has(&lines, "tychicus remote join"), "no join after a rollback")?;
    println!("ok: adopt rolls back and restarts Tychicus when the route fails after the switch");
    Ok(())
}
