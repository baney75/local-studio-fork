use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::Context;
use local_studio_core::Studio;
use serde_json::json;
use tokio::net::TcpListener;
use tokio::sync::{oneshot, watch, Notify};

use crate::devices::{self, Devices};
use crate::hub::{self, Feed, Hub};
use crate::limits::Gate;
use crate::server::{self, Hosts, Route, Server};
use crate::settings::{self, Effective, Settings};
use crate::{CHANNEL, VERSION};

const BIND_WAIT: Duration = Duration::from_secs(10);
const BIND_RETRY: Duration = Duration::from_millis(200);
const RELOAD_EVERY: Duration = Duration::from_secs(2);
const CLOSE_GRACE: Duration = Duration::from_millis(300);
const STOP_WITHIN: Duration = Duration::from_secs(2);
const GOING_AWAY: u16 = 1001;
const RESTARTING: u16 = 1012;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stop {
    Quit,
    HandOver,
}

pub(crate) struct Launch {
    pub studio: Arc<Studio>,
    pub feed: Feed,
    pub notify: Arc<Notify>,
    pub data_dir: PathBuf,
    pub flags: Settings,
    pub reload: Option<Arc<Notify>>,
}

pub(crate) fn host_name() -> String {
    std::process::Command::new("hostname")
        .arg("-s")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "local-studio".to_string())
}

pub(crate) fn configured(data_dir: &Path, flags: &Settings, default_name: &str) -> anyhow::Result<Effective> {
    let file = settings::load(data_dir)?.unwrap_or_default();
    let config = file.merged(flags).effective(default_name);
    anyhow::ensure!(
        config.tunnel_port != Some(config.port),
        "the tunnel port must differ from the local port {}: one listener per route",
        config.port
    );
    Ok(config)
}

async fn bind(port: u16, wait: Duration) -> anyhow::Result<TcpListener> {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        match TcpListener::bind(("127.0.0.1", port)).await {
            Ok(listener) => return Ok(listener),
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse && tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(BIND_RETRY).await;
            }
            Err(error) => return Err(error).with_context(|| format!("listen on 127.0.0.1:{port}")),
        }
    }
}

fn hosts_of(config: &Effective) -> Hosts {
    Hosts { allowed: config.allow_hosts.clone(), tunnels: config.allow_tunnel_hosts.clone() }
}

pub(crate) async fn run(launch: Launch, until: impl Future<Output = Stop>) -> anyhow::Result<Stop> {
    let Launch { studio, feed, notify, data_dir, flags, reload } = launch;
    let default_name = host_name();
    let config = configured(&data_dir, &flags, &default_name)?;
    let devices = Devices::new(&data_dir);
    let key = devices.static_key()?;
    let machine = json!({
        "id": devices.machine_id()?,
        "name": config.name,
        "os": std::env::consts::OS,
        "version": VERSION,
        "channel": CHANNEL,
        "urls": config.urls,
    });
    let local = bind(config.port, BIND_WAIT).await?;
    let tunnel = match config.tunnel_port {
        Some(port) => Some((bind(port, BIND_WAIT).await?, port)),
        None => None,
    };
    let hub = Hub::new(Arc::clone(&studio), machine, config.hooks.clone())?;
    tokio::spawn(Arc::clone(&hub).pump(notify, feed));
    tokio::spawn(Arc::clone(&hub).reap());
    eprintln!(
        "local-studio-serve: serving {} on http://127.0.0.1:{}{} (pid {}, machine key {})",
        data_dir.display(),
        config.port,
        tunnel.as_ref().map(|(_, port)| format!(", tunnel on http://127.0.0.1:{port}")).unwrap_or_default(),
        std::process::id(),
        &devices::hex(&key.public)[..16]
    );
    let server = Arc::new(Server {
        hub: Arc::clone(&hub),
        devices,
        key,
        port: config.port,
        hosts: RwLock::new(hosts_of(&config)),
        gate: Gate::default(),
    });
    tokio::spawn(server::revocations(Arc::clone(&server)));
    let (stopping, stopped) = watch::channel(false);
    tokio::spawn(server::listen(local, Route::Local, Arc::clone(&server), stopped.clone()));
    let tunnel_port = tunnel.as_ref().map(|(_, port)| *port);
    if let Some((listener, _)) = tunnel {
        tokio::spawn(server::listen(listener, Route::Tunnel, Arc::clone(&server), stopped.clone()));
    }
    let watcher = Watcher { data_dir, flags, default_name, server: Arc::clone(&server), stopped, tunnel: tunnel_port };
    let watching = tokio::spawn(watcher.run(config, reload));
    let stop = until.await;
    watching.abort();
    stopping.send_replace(true);
    let (code, reason) = match stop {
        Stop::Quit => (GOING_AWAY, "serve stopped"),
        Stop::HandOver => (RESTARTING, "service restart"),
    };
    hub.close_all(code, reason);
    tokio::time::sleep(CLOSE_GRACE).await;
    eprintln!("local-studio-serve: closed every remote socket with {code} ({reason})");
    Ok(stop)
}

struct Watcher {
    data_dir: PathBuf,
    flags: Settings,
    default_name: String,
    server: Arc<Server>,
    stopped: watch::Receiver<bool>,
    tunnel: Option<u16>,
}

impl Watcher {
    async fn run(mut self, mut current: Effective, reload: Option<Arc<Notify>>) {
        let mut broken = false;
        loop {
            match &reload {
                Some(reload) => {
                    tokio::select! {
                        () = tokio::time::sleep(RELOAD_EVERY) => {}
                        () = reload.notified() => eprintln!("local-studio-serve: SIGHUP: rereading serve.json"),
                    }
                }
                None => tokio::time::sleep(RELOAD_EVERY).await,
            }
            let next = match configured(&self.data_dir, &self.flags, &self.default_name) {
                Ok(next) => {
                    broken = false;
                    next
                }
                Err(error) => {
                    if !broken {
                        eprintln!("local-studio-serve: kept the running settings: {error:#}");
                    }
                    broken = true;
                    continue;
                }
            };
            self.start_tunnel(&next).await;
            if next == current {
                continue;
            }
            if next.port != current.port {
                eprintln!(
                    "local-studio-serve: serve.json asks for port {}; the local listener stays on {} until serve restarts",
                    next.port, current.port
                );
            }
            self.server.set_hosts(hosts_of(&next));
            self.server.hub.set_machine(&next.name, &next.urls);
            self.server.hub.set_hooks(next.hooks.clone());
            eprintln!(
                "local-studio-serve: reloaded serve.json: hosts {:?}, tunnel hosts {:?}, urls {:?}, {} notify hooks",
                next.allow_hosts,
                next.allow_tunnel_hosts,
                next.urls,
                next.hooks.len()
            );
            current = next;
        }
    }

    async fn start_tunnel(&mut self, config: &Effective) {
        if self.tunnel.is_some() || config.allow_tunnel_hosts.is_empty() {
            return;
        }
        let Some(port) = config.tunnel_port else { return };
        match bind(port, Duration::ZERO).await {
            Ok(listener) => {
                eprintln!("local-studio-serve: a tunnel host was added: tunnel on http://127.0.0.1:{port}");
                self.tunnel = Some(port);
                tokio::spawn(server::listen(listener, Route::Tunnel, Arc::clone(&self.server), self.stopped.clone()));
            }
            Err(error) => eprintln!("local-studio-serve: could not start the tunnel listener: {error:#}"),
        }
    }
}

pub struct Handle {
    stop: Option<oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Handle {
    pub fn stop(mut self) {
        self.halt();
    }

    fn halt(&mut self) {
        if let Some(stop) = self.stop.take() {
            stop.send(()).ok();
        }
        if let Some(thread) = self.thread.take() {
            thread.join().ok();
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.halt();
    }
}

pub fn start(studio: Arc<Studio>, data_dir: Option<&Path>) -> anyhow::Result<Option<Handle>> {
    let explicit = data_dir.map(|dir| dir.to_string_lossy().into_owned());
    let data_dir = local_studio_core::data_dir(explicit.as_deref()).map_err(|message| anyhow::anyhow!(message))?;
    if !settings::exists(&data_dir) {
        return Ok(None);
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("local-studio-remote")
        .enable_all()
        .build()?;
    let (stop, stopped) = oneshot::channel::<()>();
    let thread = std::thread::Builder::new().name("local-studio-remote".to_string()).spawn(move || {
        let (waker, notify) = hub::signal();
        let feed = Feed::Subscription(studio.subscribe(waker));
        let launch = Launch { studio, feed, notify, data_dir, flags: Settings::default(), reload: None };
        let served = runtime.block_on(run(launch, async {
            stopped.await.ok();
            Stop::HandOver
        }));
        if let Err(error) = served {
            eprintln!("local-studio-serve: the in-process server stopped: {error:#}");
        }
        runtime.shutdown_timeout(STOP_WITHIN);
    })?;
    Ok(Some(Handle { stop: Some(stop), thread: Some(thread) }))
}
