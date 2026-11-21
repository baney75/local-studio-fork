use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use local_studio_core::plugins::{self, Capability, Catalog, Consent, Grants, Plugin, Source};
use local_studio_core::tunnel::{self, Cloudflared, Failure, Ports, Status, Tunnel};
use serde_json::Value;

const USAGE: &str = "usage: local-studio --version
       local-studio plugin validate <dir>
       local-studio plugin list
       local-studio plugin install <dir | catalog id | git url@full sha>
       local-studio plugin grant <id> <capability...>
       local-studio plugin revoke <id> [capability...]
       local-studio plugin capabilities
       local-studio plugin hash <dir> [excluded path...]
       local-studio tunnel status [--json]
       local-studio tunnel setup --cloudflare <hostname> [--alias <name>]... [--serve <port>] [--mcp <port>] [--yes] [--json]
       local-studio tunnel setup --tailnet [--serve <port>] [--mcp <port>] [--yes] [--json]
       local-studio tunnel adopt [--hostname <name>] [--yes] [--json]
       local-studio tunnel off [--json]
       local-studio tunnel request [--serve <port>] [--mcp <port>] [--json]
       local-studio tunnel up [--yes] [--json]
       local-studio tunnel down | release [--json]
       local-studio tunnel ports [--serve <port>] [--mcp <port>] [--json]
       local-studio tunnel cloudflared [--yes] [--json]
       local-studio tunnel byo";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut args: Vec<&str> = args.iter().map(String::as_str).collect();
    let json = args.first() == Some(&"tunnel") && args.contains(&"--json");
    if json {
        args.retain(|arg| *arg != "--json");
    }
    let outcome = match args.as_slice() {
        ["--version"] => {
            println!("local-studio {} ({})", local_studio_core::VERSION, local_studio_core::CHANNEL);
            Ok(())
        }
        ["plugin", "validate", dir] => validate(Path::new(dir)),
        ["plugin", "list"] => list(),
        ["plugin", "install", source] => install(source),
        ["plugin", "grant", id, names @ ..] if !names.is_empty() => grant(id, names),
        ["plugin", "revoke", id, names @ ..] => revoke(id, names),
        ["plugin", "capabilities"] => {
            for capability in Capability::ALL {
                println!("{}: {}", capability.name(), capability.description());
            }
            Ok(())
        }
        ["plugin", "hash", dir, exclude @ ..] => {
            let exclude: Vec<String> = exclude.iter().map(ToString::to_string).collect();
            plugins::tree_hash(Path::new(dir), &exclude).map(|tree| println!("{tree}"))
        }
        ["tunnel", "status"] => status(json),
        ["tunnel", "setup", flags @ ..] => gate(json).unwrap_or_else(|| setup(flags, json)),
        ["tunnel", "adopt", flags @ ..] => gate(json).unwrap_or_else(|| adopt(flags, json)),
        ["tunnel", "off"] => run_state(|t| Box::pin(t.off()), json),
        ["tunnel", "request", flags @ ..] => gate(json).unwrap_or_else(|| {
            request_ports(flags, tunnel::DEFAULT_PORTS).and_then(|ports| {
                run_tunnel(
                    move |t| {
                        Box::pin(t.request(ports, local_studio_core::CHANNEL, move |url| {
                        if json {
                            println!(
                                "{}",
                                serde_json::json!({ "state": "confirming", "next": "needs_signin", "signinUrl": url })
                            );
                        } else {
                            println!("Open this address and sign in with GitHub: {url}");
                        }
                        let _ = std::io::stdout().flush();
                        open_browser(url);
                    }))
                    },
                    json,
                )
            })
        }),
        ["tunnel", "up"] => run_tunnel(|t| Box::pin(t.up(consent(false, json))), json),
        ["tunnel", "up", "--yes"] => run_tunnel(|t| Box::pin(t.up(consent(true, json))), json),
        ["tunnel", "down"] => run_tunnel(|t| Box::pin(t.down()), json),
        ["tunnel", "release"] => run_tunnel(|t| Box::pin(t.release()), json),
        ["tunnel", "ports", flags @ ..] if !flags.is_empty() => {
            let unset = Ports { serve: 0, mcp: 0 };
            request_ports(flags, unset).and_then(|ports| {
                let serve = (ports.serve != 0).then_some(ports.serve);
                let mcp = (ports.mcp != 0).then_some(ports.mcp);
                run_tunnel(move |t| Box::pin(t.set_ports(serve, mcp)), json)
            })
        }
        ["tunnel", "cloudflared"] => cloudflared(false, json),
        ["tunnel", "cloudflared", "--yes"] => cloudflared(true, json),
        ["tunnel", "byo"] => byo(),
        _ => {
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            for line in error.lines() {
                println!("error: {line}");
            }
            ExitCode::FAILURE
        }
    }
}

fn root() -> Result<PathBuf, String> {
    plugins::plugins_dir(None).ok_or_else(|| "HOME is not set".to_string())
}

fn validate(dir: &Path) -> Result<(), String> {
    let plugin = plugins::load(dir).map_err(|errors| errors.join("\n"))?;
    println!(
        "OK {} {} ({} skills, {} prompts, {} commands)",
        plugin.id,
        plugin.version,
        plugin.skills.len(),
        plugin.prompts.len(),
        plugin.commands.len()
    );
    asks(&plugin);
    Ok(())
}

fn asks(plugin: &Plugin) {
    if plugin.capabilities.is_empty() {
        println!("  asks for no capabilities");
    }
    for capability in &plugin.capabilities {
        println!("  asks for {}: {}", capability.name(), capability.description());
    }
    if let Some(mcp) = &plugin.mcp {
        println!("  MCP server {}: {} {}", plugin.mcp_name(), mcp.command.display(), mcp.args.join(" "));
        for (name, host) in &mcp.env {
            println!("  MCP server reads {host} from your environment as {name}");
        }
    }
}

fn consent_line(plugin: &Plugin, grants: &Grants) -> String {
    match grants.consent(plugin) {
        Consent::Ready => "ready".to_string(),
        Consent::NeedsConsent { missing } => {
            let names: Vec<&str> = missing.iter().map(|capability| capability.name()).collect();
            format!(
                "needs consent for {} (local-studio plugin grant {} {})",
                names.join(", "),
                plugin.id,
                names.join(" ")
            )
        }
    }
}

fn sync(root: &Path) -> Result<(), String> {
    let Some(agent) = plugins::agent_dir() else { return Ok(()) };
    if plugins::sync_agent(root, &agent)? {
        println!("updated mcpServers in {}", agent.join("settings.json").display());
    }
    Ok(())
}

fn list() -> Result<(), String> {
    let root = root()?;
    println!("plugins dir: {}", root.display());
    let scan = plugins::scan(&root);
    let grants = Grants::load(&root)?;
    for plugin in &scan.plugins {
        println!("{} {}: {}", plugin.id, plugin.version, plugin.description);
        println!("  {}", consent_line(plugin, &grants));
        for skill in &plugin.skills {
            println!("  skill {} ({})", skill.name, plugin.source());
        }
        for prompt in &plugin.prompts {
            println!("  prompt {} ({})", prompt.name, plugin.source());
        }
        for capability in &plugin.capabilities {
            println!("  capability {}: {}", capability.name(), capability.description());
        }
    }
    for command in plugins::commands(&scan.plugins) {
        println!("command /{} ({}): {}", command.name, command.source, command.description);
    }
    for failure in &scan.failures {
        println!("failed {}:", failure.dir.display());
        for error in &failure.errors {
            println!("  error: {error}");
        }
    }
    if scan.failures.is_empty() {
        Ok(())
    } else {
        Err(format!("{} plugins failed to load", scan.failures.len()))
    }
}

fn install(arg: &str) -> Result<(), String> {
    let root = root()?;
    let source = Source::parse(arg)?;
    let catalog = Catalog::bundled()?;
    let installed = plugins::install(&root, &source, &catalog).map_err(|errors| {
        let mut lines = vec![format!("refused {arg}")];
        lines.extend(errors);
        lines.join("\n")
    })?;
    let plugin = &installed.plugin;
    match &installed.replaced {
        Some(old) => println!("installed {} {} over {old} into {}", plugin.id, plugin.version, plugin.dir.display()),
        None => println!("installed {} {} into {}", plugin.id, plugin.version, plugin.dir.display()),
    }
    println!("  {} files, tree {}", installed.files, installed.tree);
    asks(plugin);
    println!("  {}", consent_line(plugin, &Grants::load(&root)?));
    sync(&root)
}

fn installed(root: &Path, id: &str) -> Result<Plugin, String> {
    plugins::load(&root.join(id))
        .map_err(|errors| format!("{id} is not installed or does not load: {}", errors.join("; ")))
}

fn capabilities(names: &[&str]) -> Result<Vec<Capability>, String> {
    names.iter().map(|name| Capability::parse(name).ok_or_else(|| format!("unknown capability: {name}"))).collect()
}

fn grant(id: &str, names: &[&str]) -> Result<(), String> {
    let root = root()?;
    let plugin = installed(&root, id)?;
    let capabilities = capabilities(names)?;
    let mut grants = Grants::load(&root)?;
    grants.grant(&plugin, &capabilities)?;
    grants.save(&root)?;
    println!("{} {}: {}", plugin.id, plugin.version, consent_line(&plugin, &grants));
    sync(&root)
}

fn revoke(id: &str, names: &[&str]) -> Result<(), String> {
    let root = root()?;
    let capabilities = capabilities(names)?;
    let mut grants = Grants::load(&root)?;
    if !grants.revoke(id, &capabilities) {
        println!("{id}: nothing to revoke");
        return Ok(());
    }
    grants.save(&root)?;
    match installed(&root, id) {
        Ok(plugin) => println!("{} {}: {}", plugin.id, plugin.version, consent_line(&plugin, &grants)),
        Err(_) => println!("{id}: grants revoked"),
    }
    sync(&root)
}

type TunnelCall<'a> = std::pin::Pin<Box<dyn std::future::Future<Output = Result<Status, Failure>> + 'a>>;

fn failed(failure: &Failure, as_json: bool) -> String {
    if !as_json {
        return failure.message.clone();
    }
    let error =
        serde_json::json!({ "state": "error", "next": "error", "code": failure.code, "message": failure.message });
    println!("{error}");
    String::new()
}

fn run_tunnel(call: impl for<'a> FnOnce(&'a Tunnel) -> TunnelCall<'a>, as_json: bool) -> Result<(), String> {
    let tunnel = Tunnel::open(None).map_err(|error| failed(&error.into(), as_json))?;
    let runtime =
        tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|error| error.to_string())?;
    let status = runtime.block_on(call(&tunnel)).map_err(|failure| failed(&failure, as_json))?;
    if as_json {
        println!("{}", serde_json::to_string(&status).map_err(|error| error.to_string())?);
        return Ok(());
    }
    println!("{}: {}", status.state, status.message);
    if let Some(position) = status.position {
        println!("  place in line: {position}");
    }
    if let Some(mcp) = &status.mcp_url {
        println!("  MCP address: {mcp}");
    }
    if let Some(ws) = &status.ws_url {
        println!("  phone address: {ws}");
    }
    if let Some(at) = &status.release_at {
        println!("  released on {at} if left down");
    }
    Ok(())
}

type StateCall<'a> = std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value, Failure>> + 'a>>;

fn gate(as_json: bool) -> Option<Result<(), String>> {
    let value = tunnel::waitlist(local_studio_core::CHANNEL, tunnel::platform())?;
    show(&value, as_json);
    Some(Ok(()))
}

fn status(as_json: bool) -> Result<(), String> {
    if let Some(value) = tunnel::waitlist(local_studio_core::CHANNEL, tunnel::platform()) {
        let tunnel = Tunnel::open(None).map_err(|error| failed(&error.into(), as_json))?;
        if tunnel.is_unset() {
            show(&value, as_json);
            return Ok(());
        }
    }
    run_tunnel(|t| Box::pin(t.status()), as_json)
}

fn show(value: &Value, as_json: bool) {
    if as_json {
        println!("{value}");
        return;
    }
    let text = |key: &str| value.get(key).and_then(Value::as_str);
    println!("{}: {}", text("state").unwrap_or("error"), text("message").unwrap_or(""));
    for step in value.get("plan").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
        println!("  - {step}");
    }
    for (key, label) in [("command", "run"), ("url", "open"), ("mcpUrl", "MCP address"), ("wsUrl", "phone address")] {
        if let Some(found) = text(key) {
            println!("  {label}: {found}");
        }
    }
}

fn run_state(call: impl for<'a> FnOnce(&'a Tunnel) -> StateCall<'a>, as_json: bool) -> Result<(), String> {
    let tunnel = Tunnel::open(None).map_err(|error| failed(&error.into(), as_json))?;
    let runtime =
        tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|error| error.to_string())?;
    let value = runtime.block_on(call(&tunnel)).map_err(|failure| failed(&failure, as_json))?;
    show(&value, as_json);
    Ok(())
}

fn port_value(value: Option<&&str>, usage: &str) -> Result<u16, String> {
    value.and_then(|value| value.parse().ok()).ok_or_else(|| usage.to_string())
}

fn setup(flags: &[&str], json: bool) -> Result<(), String> {
    let usage = "usage: local-studio tunnel setup (--cloudflare <hostname> [--alias <name>]... | --tailnet) [--serve <port>] [--mcp <port>] [--yes] [--json]";
    let (mut cloudflare, mut tailnet, mut yes) = (None, false, false);
    let (mut aliases, mut serve, mut mcp) = (Vec::new(), None, None);
    let mut rest = flags.iter();
    while let Some(flag) = rest.next() {
        match *flag {
            "--cloudflare" => cloudflare = Some(rest.next().ok_or(usage)?.to_string()),
            "--alias" => aliases.push(rest.next().ok_or(usage)?.to_string()),
            "--tailnet" => tailnet = true,
            "--yes" => yes = true,
            "--serve" => serve = Some(port_value(rest.next(), usage)?),
            "--mcp" => mcp = Some(port_value(rest.next(), usage)?),
            _ => return Err(usage.to_string()),
        }
    }
    match (cloudflare, tailnet) {
        (Some(host), false) => {
            run_state(move |t| Box::pin(t.setup_cloudflare(host, aliases, serve, mcp, consent(yes, json))), json)
        }
        (None, true) if aliases.is_empty() => run_state(move |t| Box::pin(t.setup_tailnet(serve, mcp)), json),
        _ => Err(usage.to_string()),
    }
}

fn adopt(flags: &[&str], json: bool) -> Result<(), String> {
    let usage = "usage: local-studio tunnel adopt [--hostname <name>] [--yes] [--json]";
    let (mut hostname, mut yes) = (None, false);
    let mut rest = flags.iter();
    while let Some(flag) = rest.next() {
        match *flag {
            "--hostname" => hostname = Some(rest.next().ok_or(usage)?.to_string()),
            "--yes" => yes = true,
            _ => return Err(usage.to_string()),
        }
    }
    let go = move |plan: &Value| {
        if yes {
            return true;
        }
        if json || !std::io::stdin().is_terminal() {
            return false;
        }
        for step in plan.get("plan").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
            println!("  - {step}");
        }
        print!("Adopt Tychicus's tunnel this way? [y/N] ");
        let _ = std::io::stdout().flush();
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer).is_ok() && matches!(answer.trim(), "y" | "Y" | "yes" | "Yes")
    };
    run_state(move |t| Box::pin(t.adopt(hostname, go, consent(yes, json))), json)
}

fn request_ports(flags: &[&str], mut ports: Ports) -> Result<Ports, String> {
    let usage = "usage: local-studio tunnel request | ports [--serve <port>] [--mcp <port>]";
    let mut rest = flags.iter();
    while let Some(flag) = rest.next() {
        let value: u16 = rest.next().and_then(|value| value.parse().ok()).ok_or(usage)?;
        match *flag {
            "--serve" => ports.serve = value,
            "--mcp" => ports.mcp = value,
            _ => return Err(usage.to_string()),
        }
    }
    Ok(ports)
}

fn open_browser(url: &str) {
    if std::env::var_os("LS_TUNNEL_NO_BROWSER").is_some() {
        return;
    }
    let _ = opener()
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

#[cfg(unix)]
fn opener() -> std::process::Command {
    std::process::Command::new(if cfg!(target_os = "macos") { "open" } else { "xdg-open" })
}

#[cfg(windows)]
fn opener() -> std::process::Command {
    let mut command = std::process::Command::new("rundll32.exe");
    command.arg("url.dll,FileProtocolHandler");
    command
}

fn consent(yes: bool, as_json: bool) -> impl FnOnce(&Path, &[String]) -> bool {
    move |target, notes| {
        if yes {
            return true;
        }
        if as_json || !std::io::stdin().is_terminal() {
            return false;
        }
        for note in notes {
            println!("{note}");
        }
        print!(
            "Download cloudflared {} from github.com/cloudflare/cloudflared, check its pinned SHA-256, and install it at {}? [y/N] ",
            tunnel::CLOUDFLARED_VERSION,
            target.display()
        );
        let _ = std::io::stdout().flush();
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer).is_ok() && matches!(answer.trim(), "y" | "Y" | "yes" | "Yes")
    }
}

fn cloudflared(yes: bool, as_json: bool) -> Result<(), String> {
    let tunnel = Tunnel::open(None).map_err(|error| failed(&error.into(), as_json))?;
    let runtime =
        tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|error| error.to_string())?;
    let found = runtime
        .block_on(tunnel.cloudflared(consent(yes, as_json), "local-studio tunnel cloudflared --yes"))
        .map_err(|failure| failed(&failure, as_json))?;
    if as_json {
        let pinned = matches!(found, Cloudflared::Pinned(_));
        let path = found.path().display().to_string();
        let version = tunnel::CLOUDFLARED_VERSION;
        println!(
            "{}",
            serde_json::json!({ "state": "ready", "next": "ready", "path": path, "pinned": pinned, "version": version })
        );
        return Ok(());
    }
    match found {
        Cloudflared::Dev(path) => println!("LS_CLOUDFLARED: {} (a dev override, not checked)", path.display()),
        Cloudflared::Pinned(path) => {
            println!("cloudflared {} at {} (pinned digest checked)", tunnel::CLOUDFLARED_VERSION, path.display());
        }
    }
    Ok(())
}

fn byo() -> Result<(), String> {
    match std::process::Command::new("tychicus").args(["remote", "setup"]).status() {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(format!("tychicus remote setup exited with {status}")),
        Err(_) => Err("Install the Tychicus plugin, then run: tychicus remote setup".to_string()),
    }
}
