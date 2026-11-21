use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use base64::Engine;
use local_studio_core::{DaemonLaunch, Studio, StudioConfig};
use serde_json::{json, Value};
use tokio::sync::{mpsc, Notify};

use crate::devices::{now_ms, Devices};
use crate::host::{self, host_name, Launch, Stop};
use crate::hub::{self, Feed};
use crate::settings::{self, Hook, Settings};
use crate::{lock, service, CHANNEL};

const LOCK_RETRY: Duration = Duration::from_secs(2);
const HAND_OVER_WAIT: Duration = Duration::from_secs(20);
const HAND_OVER_POLL: Duration = Duration::from_millis(200);
const STOP_WITHIN: Duration = Duration::from_secs(2);
const PICK_UP_WITHIN: Duration = Duration::from_secs(5);

const USAGE: &str = "usage:
  local-studio-serve serve [--port 18093] [--tunnel-port 18094] [--allow-host <name>]... [--allow-tunnel-host <name>]... [--url <wss url>]... [--notify <url>]... [--eukhe <path>] [--name <name>] [--data-dir <dir>]
  local-studio-serve pair [--url <wss url>] [--port 18093] [--name <name>] [--data-dir <dir>]
  local-studio-serve devices [--data-dir <dir>]
  local-studio-serve revoke (<device id or name> | --all) [--data-dir <dir>]
  local-studio-serve allow-tunnel-host [--remove] [--json] <name> [--data-dir <dir>]
  local-studio-serve install [--tailscale] [--https-port 443] [--bin <path>] [--eukhe <path>] [--no-load] [--json] [serve flags]
  local-studio-serve uninstall [--no-load] [--json] [--data-dir <dir>]
  local-studio-serve status [--no-load] [--json] [--data-dir <dir>]

--json prints one object with a \"state\" field, and nothing else on stdout. No command ever prompts.
serve reads <data dir>/remote/serve.json and rereads it on SIGHUP and when it changes; flags add to its lists and replace its other fields for that run.";

#[must_use]
pub fn cli() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            if std::env::args().skip(1).any(|arg| arg == "--json") {
                println!("{}", json!({ "state": "error", "error": format!("{error:#}") }));
            } else {
                eprintln!("local-studio-serve: {error:#}");
            }
            ExitCode::FAILURE
        }
    }
}

pub(crate) struct Out {
    json: bool,
}

impl Out {
    pub fn new(args: &mut Args) -> Self {
        Self { json: args.switch("--json") }
    }

    pub fn say(&self, line: impl std::fmt::Display) {
        if !self.json {
            println!("{line}");
        }
    }

    pub fn object(&self, value: &Value) {
        if self.json {
            println!("{value}");
        }
    }
}

pub(crate) struct Args(pub Vec<String>);

impl Args {
    pub fn take(&mut self, flag: &str) -> anyhow::Result<Option<String>> {
        let Some(at) = self.0.iter().position(|arg| arg == flag) else { return Ok(None) };
        if at + 1 >= self.0.len() {
            bail!("{flag} needs a value\n{USAGE}");
        }
        let value = self.0.remove(at + 1);
        self.0.remove(at);
        Ok(Some(value))
    }

    pub fn take_all(&mut self, flag: &str) -> anyhow::Result<Vec<String>> {
        let mut values = Vec::new();
        while let Some(value) = self.take(flag)? {
            values.push(value);
        }
        Ok(values)
    }

    pub fn switch(&mut self, flag: &str) -> bool {
        let before = self.0.len();
        self.0.retain(|arg| arg != flag);
        self.0.len() != before
    }

    pub fn number(&mut self, flag: &str) -> anyhow::Result<Option<u16>> {
        self.take(flag)?.map(|port| port.parse::<u16>().with_context(|| format!("{flag} {port}"))).transpose()
    }

    fn port(&mut self) -> anyhow::Result<u16> {
        Ok(self.number("--port")?.unwrap_or(settings::DEFAULT_PORT))
    }

    pub fn data_dir(&mut self) -> anyhow::Result<PathBuf> {
        let explicit = self.take("--data-dir")?;
        local_studio_core::data_dir(explicit.as_deref()).map_err(|message| anyhow::anyhow!(message))
    }

    pub fn serve_flags(&mut self) -> anyhow::Result<Settings> {
        let flags = Settings {
            name: self.take("--name")?,
            port: self.number("--port")?,
            tunnel_port: self.number("--tunnel-port")?,
            allow_hosts: self.take_all("--allow-host")?,
            allow_tunnel_hosts: self.take_all("--allow-tunnel-host")?,
            urls: self.take_all("--url")?,
            notify: self.take_all("--notify")?.iter().map(|url| Hook::webhook(url)).collect(),
            ..Settings::default()
        };
        for host in flags.allow_hosts.iter().chain(&flags.allow_tunnel_hosts) {
            settings::check_host(host)?;
        }
        if flags.port.is_some() && flags.tunnel_port == flags.port {
            bail!("--tunnel-port must differ from --port: one listener per route");
        }
        Ok(flags)
    }

    pub fn done(self) -> anyhow::Result<()> {
        match self.0.first() {
            Some(extra) => bail!("unexpected argument {extra}\n{USAGE}"),
            None => Ok(()),
        }
    }
}

fn run() -> anyhow::Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let command = if args.first().is_some_and(|arg| !arg.starts_with('-')) { args.remove(0) } else { "serve".into() };
    let args = Args(args);
    match command.as_str() {
        "serve" => serve(args),
        "pair" => pair(args),
        "devices" => list(args),
        "revoke" => revoke(args),
        "allow-tunnel-host" => allow_tunnel_host(args),
        "install" => service::install(args),
        "uninstall" => service::uninstall(args),
        "status" => service::status(args),
        "help" => {
            println!("{USAGE}");
            Ok(())
        }
        other => bail!("unknown command {other}\n{USAGE}"),
    }
}

enum Event {
    Stop,
    HandOver,
    Reload,
}

fn signals() -> anyhow::Result<mpsc::UnboundedReceiver<Event>> {
    use tokio::signal::unix::{signal, SignalKind};
    let (events, received) = mpsc::unbounded_channel();
    let (ready, installed) = std::sync::mpsc::channel::<Result<(), String>>();
    std::thread::Builder::new().name("local-studio-signals".to_string()).spawn(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(runtime) => runtime,
            Err(error) => {
                ready.send(Err(error.to_string())).ok();
                return;
            }
        };
        runtime.block_on(async move {
            let streams = (
                signal(SignalKind::terminate()),
                signal(SignalKind::interrupt()),
                signal(SignalKind::user_defined1()),
                signal(SignalKind::hangup()),
            );
            let (Ok(mut terminate), Ok(mut interrupt), Ok(mut hand_over), Ok(mut hang_up)) = streams else {
                ready.send(Err("could not install the signal handlers".to_string())).ok();
                return;
            };
            ready.send(Ok(())).ok();
            loop {
                let event = tokio::select! {
                    _ = terminate.recv() => Event::Stop,
                    _ = interrupt.recv() => Event::Stop,
                    _ = hand_over.recv() => Event::HandOver,
                    _ = hang_up.recv() => Event::Reload,
                };
                if events.send(event).is_err() {
                    return;
                }
            }
        });
    })?;
    installed.recv().context("the signal thread stopped")?.map_err(|error| anyhow::anyhow!(error))?;
    Ok(received)
}

struct Waiter {
    runtime: tokio::runtime::Runtime,
    events: mpsc::UnboundedReceiver<Event>,
}

impl Waiter {
    fn stopped_within(&mut self, wait: Duration) -> bool {
        let events = &mut self.events;
        self.runtime.block_on(async {
            let deadline = tokio::time::Instant::now() + wait;
            loop {
                match tokio::time::timeout_at(deadline, events.recv()).await {
                    Err(_) => return false,
                    Ok(Some(Event::Stop) | None) => return true,
                    Ok(Some(Event::HandOver | Event::Reload)) => {}
                }
            }
        })
    }

    fn stopped_until_free(&mut self, data_dir: &Path) -> bool {
        let since = Instant::now();
        while since.elapsed() < LOCK_RETRY {
            if self.stopped_within(HAND_OVER_POLL) {
                return true;
            }
            if matches!(lock::holder(data_dir), lock::Owner::Free) {
                return false;
            }
        }
        false
    }

    fn take_lock(
        &mut self,
        data_dir: &Path,
        config: &StudioConfig,
    ) -> anyhow::Result<Option<(Arc<Studio>, Arc<Notify>)>> {
        let mut noted = false;
        let mut free = 0;
        loop {
            let (waker, notify) = hub::signal();
            let error = match Studio::new(config.clone(), waker) {
                Ok(studio) => {
                    if noted {
                        eprintln!("local-studio-serve: the lock on {} is free; serving", data_dir.display());
                    }
                    return Ok(Some((studio, notify)));
                }
                Err(error) => error,
            };
            match lock::holder(data_dir) {
                lock::Owner::Held(pid) => {
                    free = 0;
                    if !noted {
                        eprintln!(
                            "local-studio-serve: {} owns {}; serve waits and tries again every {} s",
                            lock::describe(pid),
                            data_dir.display(),
                            LOCK_RETRY.as_secs()
                        );
                        noted = true;
                    }
                    if self.stopped_until_free(data_dir) {
                        return Ok(None);
                    }
                }
                lock::Owner::Free => {
                    free += 1;
                    if free >= 3 {
                        bail!("{error}");
                    }
                    if self.stopped_within(HAND_OVER_POLL) {
                        return Ok(None);
                    }
                }
            }
        }
    }

    fn step_aside(&mut self, data_dir: &Path) -> bool {
        let me = std::process::id();
        let since = Instant::now();
        loop {
            if let Some(owner) = lock::recorded(data_dir).filter(|pid| *pid != me) {
                eprintln!(
                    "local-studio-serve: {} took {} over; serve waits for it to finish",
                    lock::describe(Some(owner)),
                    data_dir.display()
                );
                return true;
            }
            if since.elapsed() >= HAND_OVER_WAIT {
                eprintln!(
                    "local-studio-serve: nothing took {} over within {} s; serve tries the lock again",
                    data_dir.display(),
                    HAND_OVER_WAIT.as_secs()
                );
                return true;
            }
            if self.stopped_within(HAND_OVER_POLL) {
                return false;
            }
        }
    }
}

async fn next_stop(events: &mut mpsc::UnboundedReceiver<Event>, reload: &Notify) -> Stop {
    loop {
        match events.recv().await {
            Some(Event::Stop) | None => return Stop::Quit,
            Some(Event::HandOver) => return Stop::HandOver,
            Some(Event::Reload) => reload.notify_one(),
        }
    }
}

fn serve(mut args: Args) -> anyhow::Result<()> {
    let flags = args.serve_flags()?;
    let launch = args
        .take("--eukhe")?
        .or_else(|| std::env::var("EUKHE_BIN").ok().filter(|value| !value.is_empty()))
        .map(|program| DaemonLaunch { program });
    let data_dir = args.data_dir()?;
    args.done()?;
    host::configured(&data_dir, &flags, &host_name())?;
    let config = StudioConfig {
        socket_path: None,
        session_dir: None,
        data_dir: Some(data_dir.to_string_lossy().into_owned()),
        launch,
        controller_main: None,
        take_over_serve: false,
    };
    let mut waiter =
        Waiter { runtime: tokio::runtime::Builder::new_current_thread().enable_all().build()?, events: signals()? };
    loop {
        let Some((studio, notify)) = waiter.take_lock(&data_dir, &config)? else { return Ok(()) };
        let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
        let reload = Arc::new(Notify::new());
        let launch = Launch {
            studio: Arc::clone(&studio),
            feed: Feed::Tick,
            notify,
            data_dir: data_dir.clone(),
            flags: flags.clone(),
            reload: Some(Arc::clone(&reload)),
        };
        let events = &mut waiter.events;
        let served = runtime.block_on(host::run(launch, next_stop(events, &reload)));
        runtime.shutdown_timeout(STOP_WITHIN);
        studio.shutdown();
        drop(studio);
        match served? {
            Stop::Quit => return Ok(()),
            Stop::HandOver => {
                eprintln!("local-studio-serve: SIGUSR1: released {} for the desktop app", data_dir.display());
                if !waiter.step_aside(&data_dir) {
                    return Ok(());
                }
            }
        }
    }
}

fn allow_tunnel_host(mut args: Args) -> anyhow::Result<()> {
    let out = Out::new(&mut args);
    let remove = args.switch("--remove");
    let data_dir = args.data_dir()?;
    let Some(name) = args.0.first().cloned() else { bail!("name the tunnel host\n{USAGE}") };
    args.0.remove(0);
    args.done()?;
    settings::check_host(&name)?;
    let mut settings = settings::load(&data_dir)?.unwrap_or_default();
    let present = settings.allow_tunnel_hosts.contains(&name);
    if remove {
        settings.allow_tunnel_hosts.retain(|host| host != &name);
    } else if !present {
        settings.allow_tunnel_hosts.push(name.clone());
    }
    if !remove && settings.tunnel_port.is_none() {
        settings.tunnel_port = Some(settings::DEFAULT_TUNNEL_PORT);
    }
    settings::save(&data_dir, &settings)?;
    let path = settings::path(&data_dir);
    let tunnel_port = settings.tunnel_port.unwrap_or(settings::DEFAULT_TUNNEL_PORT);
    let state = match (remove, present) {
        (true, true) => "removed",
        (false, false) => "added",
        _ => "unchanged",
    };
    match (remove, present) {
        (true, true) => out.say(format_args!("Removed {name} from allowTunnelHosts in {}.", path.display())),
        (true, false) => out.say(format_args!("{name} was not in allowTunnelHosts in {}.", path.display())),
        (false, true) => out.say(format_args!("{name} is already in allowTunnelHosts in {}.", path.display())),
        (false, false) => out.say(format_args!(
            "Added {name} to allowTunnelHosts in {}; the tunnel listener is 127.0.0.1:{tunnel_port}.",
            path.display()
        )),
    }
    let owner = match lock::holder(&data_dir) {
        lock::Owner::Held(pid) => Some(pid),
        lock::Owner::Free => None,
    };
    let process = owner.flatten().and_then(local_studio_core::process_name);
    if let (Some(Some(pid)), Some("local-studio-serve")) = (owner, process.as_deref()) {
        std::process::Command::new("kill").args(["-HUP", &pid.to_string()]).status().ok();
    }
    let live = owner.is_some() && picked_up(tunnel_port, &name, !remove);
    if live {
        out.say(format_args!(
            "{} picked it up: 127.0.0.1:{tunnel_port} answers accordingly.",
            lock::describe(owner.flatten())
        ));
    } else if owner.is_some() {
        out.say(format_args!(
            "{} did not pick it up within {} s; check its log.",
            lock::describe(owner.flatten()),
            PICK_UP_WITHIN.as_secs()
        ));
    } else {
        out.say(format_args!(
            "Nothing serves {} now; serve or the desktop app reads it when it starts.",
            data_dir.display()
        ));
    }
    out.object(&json!({
        "state": state,
        "host": name,
        "allowTunnelHosts": settings.allow_tunnel_hosts,
        "tunnelPort": tunnel_port,
        "live": live,
        "owner": owner.map(|pid| json!({ "pid": pid, "kind": process.as_deref().map_or("unknown", lock::kind_id) })),
        "settingsPath": path,
    }));
    Ok(())
}

fn picked_up(port: u16, host: &str, allowed: bool) -> bool {
    let since = Instant::now();
    loop {
        if (service::probe(port, host) == Some(200)) == allowed {
            return true;
        }
        if since.elapsed() >= PICK_UP_WITHIN {
            return false;
        }
        std::thread::sleep(HAND_OVER_POLL);
    }
}

fn tailscale_url() -> Option<String> {
    let output = std::process::Command::new("tailscale").args(["status", "--json"]).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let status: Value = serde_json::from_slice(&output.stdout).ok()?;
    let name = status.pointer("/Self/DNSName")?.as_str()?.trim_end_matches('.');
    (!name.is_empty()).then(|| format!("wss://{name}/v1/ws"))
}

fn encode(text: &str) -> String {
    let mut encoded = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            write!(encoded, "%{byte:02X}").ok();
        }
    }
    encoded
}

fn scheme(channel: &str) -> String {
    match channel {
        "stable" | "" => "localstudio".to_string(),
        other => format!("localstudio-{other}"),
    }
}

fn pair(mut args: Args) -> anyhow::Result<()> {
    let port = args.port()?;
    let url = args.take("--url")?;
    let name = args.take("--name")?.unwrap_or_else(host_name);
    let data_dir = args.data_dir()?;
    args.done()?;
    let url = url.or_else(tailscale_url).unwrap_or_else(|| format!("ws://127.0.0.1:{port}/v1/ws"));
    let devices = Devices::new(&data_dir);
    let (id, secret, _) = devices.open_pairing()?;
    let secret = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(secret.0);
    let link = format!("{}://pair?url={}&name={}&id={id}#{secret}", scheme(CHANNEL), encode(&url), encode(&name));
    let code = qrcode::QrCode::new(link.as_bytes())?;
    let picture = code
        .render::<qrcode::render::unicode::Dense1x2>()
        .dark_color(qrcode::render::unicode::Dense1x2::Light)
        .light_color(qrcode::render::unicode::Dense1x2::Dark)
        .quiet_zone(true)
        .build();
    println!("{picture}");
    println!("{link}");
    eprintln!(
        "Scan this with Local Studio on the phone within 10 minutes. It works once; a new code cancels this one."
    );
    if url.starts_with("wss://") {
        if let Some(host) = url.trim_start_matches("wss://").split('/').next() {
            eprintln!("Run serve with --allow-host {host} so the phone's connection is accepted.");
        }
    }
    Ok(())
}

fn ago(then: i64) -> String {
    let minutes = (now_ms() - then).max(0) / 60_000;
    match minutes {
        0 => "just now".to_string(),
        1..=59 => format!("{minutes} min ago"),
        60..=2879 => format!("{} h ago", minutes / 60),
        _ => format!("{} days ago", minutes / 1440),
    }
}

fn list(mut args: Args) -> anyhow::Result<()> {
    let data_dir = args.data_dir()?;
    args.done()?;
    let devices = Devices::new(&data_dir).list()?;
    if devices.is_empty() {
        println!("No paired devices. Run `local-studio-serve pair` to pair one.");
    }
    for device in devices {
        println!(
            "{}  {}  key {}  {} {} {}  paired {}, last seen {}",
            device.info.id,
            device.info.name,
            device.key.get(..16).unwrap_or_default(),
            device.info.platform,
            device.info.app,
            device.info.channel,
            ago(device.first_seen),
            ago(device.last_seen)
        );
    }
    Ok(())
}

fn revoke(mut args: Args) -> anyhow::Result<()> {
    let data_dir = args.data_dir()?;
    let devices = Devices::new(&data_dir);
    let gone = if args.0.iter().any(|arg| arg == "--all") {
        args.0.retain(|arg| arg != "--all");
        args.done()?;
        devices.revoke_all()?
    } else {
        let Some(device) = args.0.first().cloned() else { bail!("name the device to revoke, or --all\n{USAGE}") };
        args.0.remove(0);
        args.done()?;
        let gone = devices.revoke(&device)?;
        if gone.is_empty() {
            bail!("no paired device has the id or name {device}");
        }
        gone
    };
    if gone.is_empty() {
        println!("No paired devices to revoke.");
    }
    for device in gone {
        println!(
            "Revoked {} ({}). A running serve closes its socket within a second.",
            device.info.name, device.info.id
        );
    }
    Ok(())
}
