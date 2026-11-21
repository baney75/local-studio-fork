use std::fmt::Write as _;
use std::io::{Read, Write as _};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{bail, ensure, Context};
use serde_json::{json, Value};

use crate::cli::{Args, Out};
use crate::host::host_name;
use crate::settings::{self, Effective, Tailscale};
use crate::{lock, VERSION};

const LABEL: &str = "org.local.studio.serve";
const UNIT: &str = "local-studio-serve.service";
const PASSED: [&str; 2] = ["EUKHE_DAEMON_SOCKET", "EUKHE_CODING_AGENT_DIR"];
const PROBE_WITHIN: Duration = Duration::from_secs(2);

enum Manager {
    Launchd,
    Systemd,
}

struct Unit {
    manager: Manager,
    path: PathBuf,
    log: Option<PathBuf>,
}

fn home() -> anyhow::Result<PathBuf> {
    std::env::var_os("HOME").filter(|home| !home.is_empty()).map(PathBuf::from).context("HOME is not set")
}

fn unit() -> anyhow::Result<Unit> {
    let home = home()?;
    if cfg!(target_os = "macos") {
        Ok(Unit {
            manager: Manager::Launchd,
            path: home.join("Library").join("LaunchAgents").join(format!("{LABEL}.plist")),
            log: Some(home.join("Library").join("Logs").join("local-studio-serve.log")),
        })
    } else if cfg!(target_os = "linux") {
        Ok(Unit {
            manager: Manager::Systemd,
            path: home.join(".config").join("systemd").join("user").join(UNIT),
            log: None,
        })
    } else {
        bail!("a user service for serve exists on macOS (launchd) and Linux (systemd); the Windows logon task waits for the Windows core build (#45)")
    }
}

fn uid() -> anyhow::Result<String> {
    let output = Command::new("id").arg("-u").output().context("run id -u")?;
    ensure!(output.status.success(), "id -u failed");
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn checked(program: &str, args: &[&str]) -> anyhow::Result<String> {
    let output = Command::new(program).args(args).output().with_context(|| format!("run {program}"))?;
    let said = format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    ensure!(output.status.success(), "{program} {} failed: {}", args.join(" "), said.trim());
    Ok(said)
}

fn quiet(program: &str, args: &[&str]) -> bool {
    Command::new(program)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn xml(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

fn systemd_quoted(text: &str) -> String {
    let escaped = text.replace('\\', "\\\\").replace('"', "\\\"").replace('%', "%%").replace('$', "$$");
    format!("\"{escaped}\"")
}

fn environment(eukhe: Option<&Path>) -> Vec<(String, String)> {
    let mut variables = Vec::new();
    if let Some(path) = std::env::var_os("PATH").filter(|path| !path.is_empty()) {
        variables.push(("PATH".to_string(), path.to_string_lossy().into_owned()));
    }
    if let Some(eukhe) = eukhe {
        variables.push(("EUKHE_BIN".to_string(), eukhe.to_string_lossy().into_owned()));
    }
    for name in PASSED {
        if let Some(value) = std::env::var_os(name).filter(|value| !value.is_empty()) {
            variables.push((name.to_string(), value.to_string_lossy().into_owned()));
        }
    }
    variables
}

fn plist(arguments: &[String], variables: &[(String, String)], log: &Path) -> String {
    let mut text = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n",
    );
    writeln!(text, "\t<key>Label</key>\n\t<string>{LABEL}</string>").ok();
    text.push_str("\t<key>ProgramArguments</key>\n\t<array>\n");
    for argument in arguments {
        writeln!(text, "\t\t<string>{}</string>", xml(argument)).ok();
    }
    text.push_str("\t</array>\n\t<key>EnvironmentVariables</key>\n\t<dict>\n");
    for (name, value) in variables {
        writeln!(text, "\t\t<key>{}</key>\n\t\t<string>{}</string>", xml(name), xml(value)).ok();
    }
    text.push_str("\t</dict>\n\t<key>RunAtLoad</key>\n\t<true/>\n\t<key>KeepAlive</key>\n\t<true/>\n");
    let log = xml(&log.to_string_lossy());
    writeln!(text, "\t<key>StandardOutPath</key>\n\t<string>{log}</string>").ok();
    writeln!(text, "\t<key>StandardErrorPath</key>\n\t<string>{log}</string>").ok();
    text.push_str("</dict>\n</plist>\n");
    text
}

fn systemd_unit(arguments: &[String], variables: &[(String, String)]) -> String {
    let mut text = String::from(
        "[Unit]\nDescription=Local Studio: serves this machine's threads to paired phones and desktops\nAfter=network-online.target\n\n[Service]\n",
    );
    let command: Vec<String> = arguments.iter().map(|argument| systemd_quoted(argument)).collect();
    writeln!(text, "ExecStart={}", command.join(" ")).ok();
    for (name, value) in variables {
        writeln!(text, "Environment={}", systemd_quoted(&format!("{name}={value}"))).ok();
    }
    text.push_str("Restart=always\nRestartSec=2\n\n[Install]\nWantedBy=default.target\n");
    text
}

const TAILNET_PATHS: [&str; 2] = ["/v1/health", "/v1/ws"];

fn tailscale_bin() -> String {
    std::env::var("LS_TAILSCALE").ok().filter(|path| !path.is_empty()).unwrap_or_else(|| "tailscale".to_string())
}

fn mount(https_port: u16, path: &str, port: u16) -> Vec<String> {
    vec![
        "serve".to_string(),
        "--bg".to_string(),
        "--yes".to_string(),
        format!("--https={https_port}"),
        format!("--set-path={path}"),
        format!("http://127.0.0.1:{port}{path}"),
    ]
}

fn unmount(https_port: u16, path: &str) -> Vec<String> {
    vec!["serve".to_string(), format!("--https={https_port}"), format!("--set-path={path}"), "off".to_string()]
}

fn shown(commands: &[Vec<String>]) -> Vec<Vec<String>> {
    commands.iter().map(|command| std::iter::once(tailscale_bin()).chain(command.iter().cloned()).collect()).collect()
}

fn run_tailscale(command: &[String]) -> anyhow::Result<()> {
    let args: Vec<&str> = command.iter().map(String::as_str).collect();
    checked(&tailscale_bin(), &args).map(drop)
}

fn route_host(data_dir: &Path) -> anyhow::Result<Option<String>> {
    let path = data_dir.join("tunnel").join("route.json");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    };
    let record: Value = serde_json::from_str(&text).with_context(|| format!("{} is not JSON", path.display()))?;
    let host = record
        .get("hostname")
        .and_then(Value::as_str)
        .filter(|host| !host.is_empty())
        .with_context(|| format!("{} names no hostname", path.display()))?;
    settings::check_host(host)?;
    Ok(Some(host.to_string()))
}

fn tailscale_host() -> anyhow::Result<String> {
    let said = checked(&tailscale_bin(), &["status", "--json"])?;
    let status: Value = serde_json::from_str(&said).context("tailscale status --json is not JSON")?;
    let name = status.pointer("/Self/DNSName").and_then(Value::as_str).unwrap_or_default().trim_end_matches('.');
    ensure!(!name.is_empty(), "tailscale reports no DNS name for this machine: is it logged in to a tailnet?");
    Ok(name.to_string())
}

fn canonical(path: &str, what: &str) -> anyhow::Result<PathBuf> {
    let path = std::fs::canonicalize(path).with_context(|| format!("{what} {path}"))?;
    ensure!(path.is_file(), "{what} {} is not a file", path.display());
    Ok(path)
}

fn write_unit(path: &Path, text: &str) -> anyhow::Result<()> {
    let folder = path.parent().with_context(|| format!("{} has no folder", path.display()))?;
    std::fs::create_dir_all(folder).with_context(|| format!("create {}", folder.display()))?;
    std::fs::write(path, text).with_context(|| format!("write {}", path.display()))
}

fn load(unit: &Unit) -> anyhow::Result<()> {
    match unit.manager {
        Manager::Launchd => {
            let domain = format!("gui/{}", uid()?);
            quiet("launchctl", &["bootout", &format!("{domain}/{LABEL}")]);
            checked("launchctl", &["bootstrap", &domain, &unit.path.to_string_lossy()])?;
        }
        Manager::Systemd => {
            checked("systemctl", &["--user", "daemon-reload"])?;
            checked("systemctl", &["--user", "enable", UNIT])?;
            checked("systemctl", &["--user", "restart", UNIT])?;
        }
    }
    Ok(())
}

fn unload(unit: &Unit) -> anyhow::Result<bool> {
    Ok(match unit.manager {
        Manager::Launchd => quiet("launchctl", &["bootout", &format!("gui/{}/{LABEL}", uid()?)]),
        Manager::Systemd => quiet("systemctl", &["--user", "disable", "--now", UNIT]),
    })
}

pub fn install(mut args: Args) -> anyhow::Result<()> {
    let out = Out::new(&mut args);
    let tailscale = args.switch("--tailscale");
    let no_load = args.switch("--no-load");
    let https_port = args.number("--https-port")?.unwrap_or(443);
    let bin = args.take("--bin")?;
    let eukhe = args.take("--eukhe")?;
    let flags = args.serve_flags()?;
    let data_dir = args.data_dir()?;
    args.done()?;
    let bin = if let Some(bin) = bin {
        canonical(&bin, "--bin")?
    } else {
        let current = std::env::current_exe().context("find this executable")?;
        canonical(&current.to_string_lossy(), "this executable")?
    };
    let eukhe = eukhe.map(|eukhe| canonical(&eukhe, "--eukhe")).transpose()?;
    let unit = unit()?;
    let mut settings = settings::load(&data_dir)?.unwrap_or_default().merged(&flags);
    let config = settings.effective(&host_name());
    ensure!(config.tunnel_port != Some(config.port), "the tunnel port must differ from the local port {}", config.port);
    let mut served = Value::Null;
    if tailscale {
        let route = route_host(&data_dir)?;
        let host = match &route {
            Some(host) => host.clone(),
            None => tailscale_host()?,
        };
        let paths: Vec<String> =
            if route.is_some() { Vec::new() } else { TAILNET_PATHS.iter().map(ToString::to_string).collect() };
        let commands: Vec<Vec<String>> = paths.iter().map(|path| mount(https_port, path, config.port)).collect();
        let state = if route.is_some() {
            out.say(format_args!(
                "{} names {host}: its route owns tailscale serve, so install mounts nothing",
                data_dir.join("tunnel").join("route.json").display()
            ));
            "route"
        } else if no_load {
            for command in &commands {
                out.say(format_args!("--no-load: would run tailscale {}", command.join(" ")));
            }
            "planned"
        } else {
            for command in &commands {
                run_tailscale(command)?;
            }
            out.say(format_args!(
                "tailscale serve: https://{host}:{https_port} {} forward to http://127.0.0.1:{}",
                paths.join(" and "),
                config.port
            ));
            "mounted"
        };
        if !settings.allow_hosts.contains(&host) {
            settings.allow_hosts.push(host.clone());
        }
        let mounted = if state == "mounted" { paths.clone() } else { Vec::new() };
        served = json!({
            "state": state,
            "host": host,
            "httpsPort": https_port,
            "paths": paths,
            "commands": shown(&commands),
        });
        settings.tailscale = Some(Tailscale { https_port, host, paths: mounted });
    }
    settings::save(&data_dir, &settings)?;
    out.say(format_args!("Wrote {}", settings::path(&data_dir).display()));
    let arguments = vec![
        bin.to_string_lossy().into_owned(),
        "serve".to_string(),
        "--data-dir".to_string(),
        data_dir.to_string_lossy().into_owned(),
    ];
    let variables = environment(eukhe.as_deref());
    let text = match (&unit.manager, &unit.log) {
        (Manager::Launchd, Some(log)) => {
            let folder = log.parent().with_context(|| format!("{} has no folder", log.display()))?;
            std::fs::create_dir_all(folder).with_context(|| format!("create {}", folder.display()))?;
            plist(&arguments, &variables, log)
        }
        _ => systemd_unit(&arguments, &variables),
    };
    write_unit(&unit.path, &text)?;
    out.say(format_args!("Wrote {}", unit.path.display()));
    if no_load {
        out.say(format_args!("--no-load: the service is written but not loaded"));
    } else {
        load(&unit)?;
        out.say(format_args!("Loaded the service; it serves {} whenever the desktop app does not", data_dir.display()));
    }
    out.object(&json!({
        "state": if no_load { "written" } else { "installed" },
        "loaded": !no_load,
        "unit": unit.path,
        "log": unit.log,
        "settingsPath": settings::path(&data_dir),
        "serveJson": settings,
        "tailscale": served,
    }));
    Ok(())
}

pub fn uninstall(mut args: Args) -> anyhow::Result<()> {
    let out = Out::new(&mut args);
    let no_load = args.switch("--no-load");
    let data_dir = args.data_dir()?;
    args.done()?;
    let unit = unit()?;
    let mut removed: Vec<PathBuf> = Vec::new();
    let mut unloaded = false;
    let mut tailscale = Value::Null;
    if no_load {
        out.say(format_args!("--no-load: did not unload the service"));
    } else if unload(&unit)? {
        unloaded = true;
        out.say(format_args!("Stopped and unloaded the service"));
    }
    if unit.path.exists() {
        removed.push(unit.path.clone());
        std::fs::remove_file(&unit.path).with_context(|| format!("remove {}", unit.path.display()))?;
        out.say(format_args!("Removed {}", unit.path.display()));
    } else {
        out.say(format_args!("No service file at {}", unit.path.display()));
    }
    if !no_load && matches!(unit.manager, Manager::Systemd) {
        quiet("systemctl", &["--user", "daemon-reload"]);
    }
    if let Some(settings) = settings::load(&data_dir)? {
        if let Some(added) = &settings.tailscale {
            let commands: Vec<Vec<String>> = added.paths.iter().map(|path| unmount(added.https_port, path)).collect();
            let state = if commands.is_empty() {
                "none"
            } else if no_load {
                for command in &commands {
                    out.say(format_args!("--no-load: would run tailscale {}", command.join(" ")));
                }
                "left"
            } else {
                for command in &commands {
                    run_tailscale(command)?;
                }
                out.say(format_args!(
                    "Removed tailscale serve {} on https port {} ({})",
                    added.paths.join(" and "),
                    added.https_port,
                    added.host
                ));
                "removed"
            };
            tailscale = json!({
                "state": state,
                "host": added.host,
                "httpsPort": added.https_port,
                "paths": added.paths,
                "commands": shown(&commands),
            });
        }
        let path = settings::path(&data_dir);
        std::fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
        out.say(format_args!("Removed {}", path.display()));
        removed.push(path);
    }
    out.object(&json!({
        "state": if removed.is_empty() && !unloaded { "absent" } else { "uninstalled" },
        "unloaded": unloaded,
        "removed": removed,
        "tailscale": tailscale,
    }));
    Ok(())
}

pub(crate) fn probe(port: u16, host: &str) -> Option<u16> {
    let address = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = TcpStream::connect_timeout(&address, PROBE_WITHIN).ok()?;
    stream.set_read_timeout(Some(PROBE_WITHIN)).ok()?;
    stream.set_write_timeout(Some(PROBE_WITHIN)).ok()?;
    let request = format!("GET /v1/health HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).ok()?;
    let mut answer = String::new();
    stream.read_to_string(&mut answer).ok();
    answer.split_whitespace().nth(1)?.parse().ok()
}

fn listener(route: &str, port: u16, host: &str) -> (Value, String) {
    let status = probe(port, host);
    let line = match status {
        Some(200) => format!("{route} listener 127.0.0.1:{port}: answers /v1/health for Host {host}"),
        Some(status) => format!("{route} listener 127.0.0.1:{port}: listening, {status} for Host {host}"),
        None => format!("{route} listener 127.0.0.1:{port}: not listening"),
    };
    (json!({ "route": route, "port": port, "host": host, "listening": status.is_some(), "status": status }), line)
}

fn service_state(unit: &Unit) -> anyhow::Result<String> {
    Ok(match unit.manager {
        Manager::Launchd => {
            let output = Command::new("launchctl").args(["print", &format!("gui/{}/{LABEL}", uid()?)]).output()?;
            if !output.status.success() {
                return Ok("not loaded".to_string());
            }
            let said = String::from_utf8_lossy(&output.stdout);
            let field = |name: &str| {
                said.lines()
                    .map(str::trim)
                    .find_map(|line| line.strip_prefix(&format!("{name} = ")))
                    .map(str::to_string)
            };
            let state = field("state").unwrap_or_else(|| "loaded".to_string());
            match field("pid") {
                Some(pid) => format!("{state}, pid {pid}"),
                None => state,
            }
        }
        Manager::Systemd => {
            let output = Command::new("systemctl").args(["--user", "is-active", UNIT]).output()?;
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        }
    })
}

fn describe_settings(config: &Effective) -> String {
    format!(
        "name {}, port {}, tunnel port {}, hosts {:?}, tunnel hosts {:?}, urls {:?}, {} notify hooks",
        config.name,
        config.port,
        config.tunnel_port.map_or_else(|| "none".to_string(), |port| port.to_string()),
        config.allow_hosts,
        config.allow_tunnel_hosts,
        config.urls,
        config.hooks.len()
    )
}

pub fn status(mut args: Args) -> anyhow::Result<()> {
    let out = Out::new(&mut args);
    let no_load = args.switch("--no-load");
    let data_dir = args.data_dir()?;
    args.done()?;
    out.say(format_args!("local-studio-serve {VERSION}, data dir {}", data_dir.display()));
    let holder = lock::holder(&data_dir);
    let owner = match &holder {
        lock::Owner::Held(pid) => {
            out.say(format_args!("owner: {} holds {}", lock::describe(*pid), lock::path(&data_dir).display()));
            let name = pid.and_then(local_studio_core::process_name);
            json!({ "pid": pid, "kind": name.as_deref().map_or("unknown", lock::kind_id), "process": name })
        }
        lock::Owner::Free => {
            out.say(format_args!("owner: none; nothing holds {}", lock::path(&data_dir).display()));
            Value::Null
        }
    };
    let path = settings::path(&data_dir);
    let stored = settings::load(&data_dir)?;
    let config = stored.clone().unwrap_or_default().effective(&host_name());
    match &stored {
        Some(stored) => {
            out.say(format_args!("serve.json: {}: {}", path.display(), describe_settings(&config)));
            if let Some(added) = &stored.tailscale {
                out.say(format_args!(
                    "tailscale serve: {} on https port {} for {}, added by install",
                    if added.paths.is_empty() { "no paths".to_string() } else { added.paths.join(" and ") },
                    added.https_port,
                    added.host
                ));
            }
        }
        None => {
            out.say(format_args!("serve.json: none at {} (defaults: {})", path.display(), describe_settings(&config)));
        }
    }
    let mut listeners = vec![listener("local", config.port, &format!("127.0.0.1:{}", config.port))];
    if let Some(port) = config.tunnel_port {
        let host = config.allow_tunnel_hosts.first().cloned().unwrap_or_else(|| format!("127.0.0.1:{port}"));
        listeners.push(listener("tunnel", port, &host));
    }
    for line in &listeners {
        out.say(format_args!("{}", line.1));
    }
    let unit = unit()?;
    let installed = unit.path.is_file();
    let (service, state) = if !installed {
        ("not installed", "not installed".to_string())
    } else if no_load {
        ("installed", "installed (--no-load: the service manager was not asked)".to_string())
    } else {
        let state = service_state(&unit)?;
        (if state == "not loaded" { "installed" } else { "loaded" }, state)
    };
    out.say(format_args!("service: {} ({state})", unit.path.display()));
    if let Some(log) = unit.log.as_ref().filter(|_| installed) {
        out.say(format_args!("log: {}", log.display()));
    }
    let serving = listeners.first().is_some_and(|line| line.0["status"] == json!(200));
    let overall = match (&holder, serving) {
        (lock::Owner::Free, _) => "stopped",
        (lock::Owner::Held(_), true) => "serving",
        (lock::Owner::Held(_), false) => "owned",
    };
    out.object(&json!({
        "state": overall,
        "version": VERSION,
        "dataDir": data_dir,
        "owner": owner,
        "listeners": listeners.into_iter().map(|line| line.0).collect::<Vec<_>>(),
        "settingsPath": path,
        "serveJson": stored,
        "effective": {
            "name": config.name,
            "port": config.port,
            "tunnelPort": config.tunnel_port,
            "allowHosts": config.allow_hosts,
            "allowTunnelHosts": config.allow_tunnel_hosts,
            "urls": config.urls,
        },
        "service": {
            "state": service,
            "detail": state,
            "unit": unit.path,
            "log": unit.log.filter(|_| installed),
            "checked": !no_load,
        },
    }));
    Ok(())
}
