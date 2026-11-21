use std::io::{BufRead, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context};
use base64::Engine;
use eukhe_client::daemon_client::DaemonClient;
use eukhe_types::daemon::DaemonCommand;
use futures_util::{SinkExt, StreamExt};
use local_studio_core::{DaemonLaunch, Studio, StudioConfig, Waker};
use serde_json::{json, Map, Value};
use snow::TransportState;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

const USAGE: &str = "usage: serve_handoff_smoke <eukhe> <scratch dir>";
const CHANNEL: &str = env!("LOCAL_STUDIO_BUILD_CHANNEL");
const XX: &str = "Noise_XXpsk3_25519_ChaChaPoly_SHA256";
const KK: &str = "Noise_KK_25519_ChaChaPoly_SHA256";
const PROLOGUE: &[u8] = b"local-studio-remote/1\n";
const QUICK: Duration = Duration::from_secs(20);
const SETUP: Duration = Duration::from_secs(60);
const TAKEOVER: Duration = Duration::from_secs(15);
const BACK: Duration = Duration::from_secs(2);
const WAITING: &str = "serve waits and tries again";
const TUNNEL: &str = "studio.handoff.test";
const TAILNET: &str = "box.tail1234.ts.net";
const ROUTED: &str = "studio-box.tail1234.ts.net";
const SECOND: &str = "studio-parish1.handoff.test";

struct Quiet;

impl Waker for Quiet {
    fn wake(&self) {}
}

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args_os().skip(1);
    let first = args.next().context(USAGE)?;
    if first == "app" {
        let eukhe = PathBuf::from(args.next().context(USAGE)?);
        let data = PathBuf::from(args.next().context(USAGE)?);
        return app(&eukhe, &data);
    }
    let root = PathBuf::from(args.next().context(USAGE)?);
    tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(smoke(&PathBuf::from(first), &root))
}

fn app(eukhe: &Path, data: &Path) -> anyhow::Result<()> {
    let config = StudioConfig {
        socket_path: None,
        session_dir: None,
        data_dir: Some(data.to_string_lossy().into_owned()),
        launch: Some(DaemonLaunch { program: eukhe.to_string_lossy().into_owned() }),
        controller_main: None,
        take_over_serve: true,
    };
    let started = Instant::now();
    let studio = Studio::new(config, Arc::new(Quiet)).map_err(|error| anyhow::anyhow!("{error}"))?;
    let took = started.elapsed();
    let host = local_studio_remote::host::start(Arc::clone(&studio), Some(data))?
        .context("serve.json exists, so the in-process server must start")?;
    println!("ready {}", took.as_millis());
    std::io::stdout().flush()?;
    let (quit, quitting) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        let mut line = String::new();
        while std::io::stdin().read_line(&mut line).is_ok_and(|read| read > 0) {
            line.clear();
        }
        quit.send(()).ok();
    });
    while matches!(quitting.recv_timeout(Duration::from_millis(200)), Err(RecvTimeoutError::Timeout)) {
        drop(studio.tick());
    }
    host.stop();
    studio.shutdown();
    Ok(())
}

struct App {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: mpsc::UnboundedReceiver<String>,
}

struct Run {
    me: PathBuf,
    bin: PathBuf,
    eukhe: PathBuf,
    root: PathBuf,
    data: PathBuf,
    port: u16,
    tunnel: u16,
    serve: Option<Child>,
    apps: Vec<App>,
    report: Vec<String>,
}

fn free_port() -> anyhow::Result<u16> {
    Ok(std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}

fn alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn owner(data: &Path) -> Option<u32> {
    std::fs::read_to_string(data.join("studio.lock")).ok()?.trim().parse().ok()
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

async fn until(
    step: &str,
    bound: Duration,
    mut done: impl FnMut() -> anyhow::Result<bool>,
) -> anyhow::Result<Duration> {
    let started = Instant::now();
    loop {
        if done()? {
            let took = started.elapsed();
            println!("ok   {step} ({:.2} s)", took.as_secs_f64());
            return Ok(took);
        }
        if started.elapsed() >= bound {
            bail!("timed out after {:.1} s waiting for: {step}", bound.as_secs_f64());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn http_get(port: u16, host: &str, path: &str) -> anyhow::Result<u16> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await?;
    let request = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await?;
    let mut answer = String::new();
    stream.read_to_string(&mut answer).await?;
    answer.split_whitespace().nth(1).and_then(|code| code.parse().ok()).context("no status")
}

async fn health(port: u16, host: &str) -> Option<u16> {
    tokio::time::timeout(Duration::from_secs(2), http_get(port, host, "/v1/health")).await.ok()?.ok()
}

impl Run {
    fn cli(&self, args: &[&str]) -> anyhow::Result<String> {
        let output = Command::new(&self.bin).args(args).arg("--data-dir").arg(&self.data).output()?;
        ensure!(output.status.success(), "{args:?} failed: {}", String::from_utf8_lossy(&output.stderr));
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    fn json(&self, args: &[&str]) -> anyhow::Result<Value> {
        json_of(Command::new(&self.bin).args(args).arg("--json").arg("--data-dir").arg(&self.data), args)
    }

    fn serve_pid(&self) -> anyhow::Result<u32> {
        self.serve.as_ref().map(Child::id).context("serve is not running")
    }

    fn serve_alive(&mut self) -> anyhow::Result<bool> {
        Ok(match self.serve.as_mut() {
            Some(child) => child.try_wait()?.is_none(),
            None => false,
        })
    }

    fn serve_log(&self) -> String {
        read(&self.root.join("serve.log"))
    }

    fn start_serve(&mut self) -> anyhow::Result<u32> {
        let log = std::fs::OpenOptions::new().create(true).append(true).open(self.root.join("serve.log"))?;
        let child = Command::new(&self.bin)
            .arg("serve")
            .arg("--eukhe")
            .arg(&self.eukhe)
            .arg("--data-dir")
            .arg(&self.data)
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .context("start local-studio-serve")?;
        let pid = child.id();
        println!("     serve pid {pid}");
        self.serve = Some(child);
        Ok(pid)
    }

    fn start_app(&mut self, name: &str) -> anyhow::Result<usize> {
        let log = std::fs::OpenOptions::new().create(true).append(true).open(self.root.join("app.log"))?;
        let mut child = Command::new(&self.me)
            .arg("app")
            .arg(&self.eukhe)
            .arg(&self.data)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(log)
            .spawn()
            .with_context(|| format!("start the {name}"))?;
        let stdout = child.stdout.take().context("the app has no stdout")?;
        let (sender, lines) = mpsc::unbounded_channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if sender.send(line).is_err() {
                    return;
                }
            }
        });
        println!("     {name} pid {}", child.id());
        let stdin = child.stdin.take();
        self.apps.push(App { child, stdin, lines });
        Ok(self.apps.len() - 1)
    }

    async fn ready(&mut self, at: usize, name: &str, bound: Duration) -> anyhow::Result<Duration> {
        let started = Instant::now();
        let app = self.apps.get_mut(at).context("no such app")?;
        let line = tokio::time::timeout(bound, app.lines.recv())
            .await
            .with_context(|| {
                format!("the {name} did not start within {} s:\n{}", bound.as_secs(), read(&self.root.join("app.log")))
            })?
            .with_context(|| format!("the {name} exited before it started:\n{}", read(&self.root.join("app.log"))))?;
        let core: u64 = line
            .strip_prefix("ready ")
            .and_then(|ms| ms.parse().ok())
            .with_context(|| format!("the {name} said {line}"))?;
        Ok(started.elapsed().max(Duration::from_millis(core)))
    }

    fn app_pid(&self, at: usize) -> anyhow::Result<u32> {
        self.apps.get(at).map(|app| app.child.id()).context("no such app")
    }

    async fn quit_app(&mut self, at: usize) -> anyhow::Result<Instant> {
        let app = self.apps.get_mut(at).context("no such app")?;
        drop(app.stdin.take());
        let pid = app.child.id();
        let deadline = Instant::now() + QUICK;
        loop {
            if let Some(status) = app.child.try_wait()? {
                ensure!(status.success(), "app {pid} quit with {status}:\n{}", read(&self.root.join("app.log")));
                return Ok(Instant::now());
            }
            if Instant::now() >= deadline {
                Command::new("kill").args(["-KILL", &pid.to_string()]).status()?;
                app.child.wait()?;
                bail!("app {pid} did not quit within {} s of stdin closing and was killed", QUICK.as_secs());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn stop_serve(&mut self) -> anyhow::Result<()> {
        let Some(mut child) = self.serve.take() else { return Ok(()) };
        let pid = child.id().to_string();
        Command::new("kill").args(["-TERM", &pid]).status()?;
        let deadline = Instant::now() + QUICK;
        while child.try_wait()?.is_none() {
            if Instant::now() >= deadline {
                Command::new("kill").args(["-KILL", &pid]).status()?;
                child.wait()?;
                bail!("serve {pid} ignored SIGTERM and was killed");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        println!("ok   serve {pid} stopped by PID");
        Ok(())
    }

    fn cleanup(&mut self) -> anyhow::Result<()> {
        for app in &mut self.apps {
            drop(app.stdin.take());
            if app.child.try_wait()?.is_none() {
                std::thread::sleep(Duration::from_secs(3));
            }
            if app.child.try_wait()?.is_none() {
                Command::new("kill").args(["-KILL", &app.child.id().to_string()]).status()?;
                app.child.wait()?;
            }
        }
        self.stop_serve()
    }
}

fn json_of(command: &mut Command, args: &[&str]) -> anyhow::Result<Value> {
    let output = command.stdin(Stdio::null()).output()?;
    let printed = String::from_utf8_lossy(&output.stdout);
    ensure!(output.status.success(), "{args:?} --json failed: {printed}{}", String::from_utf8_lossy(&output.stderr));
    ensure!(printed.trim().lines().count() == 1, "{args:?} --json printed more than one line:\n{printed}");
    let value: Value =
        serde_json::from_str(printed.trim()).with_context(|| format!("{args:?} --json printed {printed}"))?;
    ensure!(value.get("state").and_then(Value::as_str).is_some(), "{args:?} --json has no state: {value}");
    Ok(value)
}

fn expect(value: &Value, pointer: &str, wanted: &Value, what: &str) -> anyhow::Result<()> {
    ensure!(value.pointer(pointer) == Some(wanted), "{what}: {pointer} is not {wanted} in {value}");
    Ok(())
}

fn header_text(pattern: &str, id_key: &str, id: &str) -> String {
    json!({ "v": 1, "pattern": pattern, id_key: id, "protocol": 2, "min_protocol": 2 }).to_string()
}

fn prologue(header: &str) -> Vec<u8> {
    let mut bytes = PROLOGUE.to_vec();
    bytes.extend_from_slice(header.as_bytes());
    bytes
}

fn noise_write(state: &mut snow::HandshakeState, payload: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut message = vec![0u8; 65_535];
    let length = state.write_message(payload, &mut message)?;
    message.truncate(length);
    Ok(message)
}

struct Phone {
    id: String,
    private: Vec<u8>,
    server: Vec<u8>,
}

enum Frame {
    Json(Value),
    Closed(Option<u16>),
}

struct Link {
    socket: Socket,
    transport: TransportState,
    joined: Vec<u8>,
}

async fn socket(port: u16) -> anyhow::Result<Socket> {
    let request = format!("ws://127.0.0.1:{port}/v1/ws").into_client_request()?;
    Ok(tokio_tungstenite::connect_async(request).await?.0)
}

async fn binary(socket: &mut Socket) -> anyhow::Result<Option<Vec<u8>>> {
    loop {
        let message = tokio::time::timeout(QUICK, socket.next()).await.context("no handshake message in time")?;
        return Ok(match message {
            Some(Ok(Message::Binary(bytes))) => Some(bytes.to_vec()),
            Some(Ok(Message::Text(text))) => bail!("a text message during the handshake: {text}"),
            Some(Ok(Message::Close(_)) | Err(_)) | None => None,
            Some(Ok(_)) => continue,
        });
    }
}

impl Link {
    async fn send(&mut self, frame: &Value) -> anyhow::Result<()> {
        let mut record = vec![1u8];
        record.extend_from_slice(frame.to_string().as_bytes());
        let mut message = vec![0u8; record.len() + 16];
        let length = self.transport.write_message(&record, &mut message)?;
        message.truncate(length);
        self.socket.send(Message::binary(message)).await?;
        Ok(())
    }

    async fn next(&mut self, bound: Duration) -> anyhow::Result<Frame> {
        let deadline = Instant::now() + bound;
        loop {
            let left = deadline.checked_duration_since(Instant::now()).context("no frame from the server in time")?;
            let message =
                tokio::time::timeout(left, self.socket.next()).await.context("no frame from the server in time")?;
            match message {
                Some(Ok(Message::Binary(bytes))) => {
                    let mut record = vec![0u8; bytes.len()];
                    let length = self.transport.read_message(&bytes, &mut record)?;
                    record.truncate(length);
                    match record.first() {
                        Some(1) => {
                            self.joined.extend_from_slice(&record[1..]);
                            let text = std::mem::take(&mut self.joined);
                            return Ok(Frame::Json(serde_json::from_slice(&text)?));
                        }
                        Some(2) => self.joined.extend_from_slice(&record[1..]),
                        _ => {}
                    }
                }
                Some(Ok(Message::Close(frame))) => return Ok(Frame::Closed(frame.map(|frame| u16::from(frame.code)))),
                Some(Err(_)) | None => return Ok(Frame::Closed(None)),
                Some(Ok(_)) => {}
            }
        }
    }

    async fn hello(&mut self) -> anyhow::Result<Value> {
        self.send(&json!({ "op": "hello", "device": { "name": "handoff phone", "app": "0.4.1", "channel": CHANNEL }, "resume": { "watch": [] } })).await?;
        loop {
            match self.next(QUICK).await? {
                Frame::Json(frame) if frame.get("op").and_then(Value::as_str) == Some("hello") => return Ok(frame),
                Frame::Json(_) => {}
                Frame::Closed(code) => bail!("closed with {code:?} before hello"),
            }
        }
    }

    async fn closed(&mut self, bound: Duration) -> anyhow::Result<Option<u16>> {
        loop {
            if let Frame::Closed(code) = self.next(bound).await? {
                return Ok(code);
            }
        }
    }
}

async fn pair(run: &Run) -> anyhow::Result<(Phone, Link, Value)> {
    let url = format!("ws://127.0.0.1:{}/v1/ws", run.port);
    let printed = run.cli(&["pair", "--url", &url, "--name", "handoff-box"])?;
    let link = printed.lines().find(|line| line.contains("://pair?")).context("pair printed no link")?;
    let (query, secret) = link.split_once('#').context("the pairing link has no secret")?;
    let pairing = query.split(['?', '&']).find_map(|part| part.strip_prefix("id=")).context("the link has no id")?;
    let secret = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(secret)?;
    let psk = <[u8; 32]>::try_from(secret).map_err(|_| anyhow::anyhow!("the pairing secret is not 32 bytes"))?;
    let keys = snow::Builder::new(KK.parse()?).generate_keypair()?;
    let id = "phone-handoff".to_string();
    let mut socket = socket(run.port).await?;
    let header = header_text("XXpsk3", "pair", pairing);
    socket.send(Message::text(header.clone())).await?;
    let mut state = snow::Builder::new(XX.parse()?)
        .local_private_key(&keys.private)?
        .psk(3, &psk)?
        .prologue(&prologue(&header))?
        .build_initiator()?;
    socket.send(Message::binary(noise_write(&mut state, &[])?)).await?;
    let second = binary(&mut socket).await?.context("pairing was refused")?;
    let mut scratch = vec![0u8; 65_535];
    state.read_message(&second, &mut scratch)?;
    let server = state.get_remote_static().context("the server sent no static key")?.to_vec();
    let record = json!({ "id": id, "name": "handoff phone", "platform": "ios", "app": "0.4.0", "channel": CHANNEL });
    socket.send(Message::binary(noise_write(&mut state, record.to_string().as_bytes())?)).await?;
    let mut link = Link { socket, transport: state.into_transport_mode()?, joined: Vec::new() };
    let hello = link.hello().await?;
    Ok((Phone { id, private: keys.private, server }, link, hello))
}

async fn kk(port: u16, phone: &Phone) -> anyhow::Result<(Link, Value)> {
    let mut socket = socket(port).await?;
    let header = header_text("KK", "device", &phone.id);
    socket.send(Message::text(header.clone())).await?;
    let mut state = snow::Builder::new(KK.parse()?)
        .local_private_key(&phone.private)?
        .remote_public_key(&phone.server)?
        .prologue(&prologue(&header))?
        .build_initiator()?;
    socket.send(Message::binary(noise_write(&mut state, &[])?)).await?;
    let answer = binary(&mut socket).await?.context("KK was refused")?;
    let mut scratch = vec![0u8; 65_535];
    state.read_message(&answer, &mut scratch)?;
    let mut link = Link { socket, transport: state.into_transport_mode()?, joined: Vec::new() };
    let hello = link.hello().await?;
    Ok((link, hello))
}

async fn reconnect(port: u16, phone: &Phone, step: &str) -> anyhow::Result<(Link, Value)> {
    let started = Instant::now();
    loop {
        match kk(port, phone).await {
            Ok((link, hello)) => {
                ensure!(
                    hello.get("snapshot").and_then(Value::as_array).is_some_and(|snapshot| !snapshot.is_empty()),
                    "{step}: hello carried no snapshot: {hello}"
                );
                println!(
                    "ok   {step}: the phone reconnected and got a {} snapshot ({:.2} s)",
                    hello.get("resume").and_then(Value::as_str).unwrap_or("?"),
                    started.elapsed().as_secs_f64()
                );
                return Ok((link, hello));
            }
            Err(error) if started.elapsed() >= QUICK => bail!("{step}: the phone could not reconnect: {error:#}"),
            Err(_) => tokio::time::sleep(Duration::from_millis(200)).await,
        }
    }
}

fn urls(hello: &Value) -> Vec<String> {
    hello
        .pointer("/machine/urls")
        .and_then(Value::as_array)
        .map(|urls| urls.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default()
}

fn agent_dir(root: &Path) -> anyhow::Result<()> {
    let agent = root.join("agent");
    std::fs::create_dir_all(&agent)?;
    let write = |name: &str, data: &[u8]| -> anyhow::Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(agent.join(name))
            .with_context(|| format!("create {}", agent.join(name).display()))?
            .write_all(data)?;
        Ok(())
    };
    write("auth.json", b"{}")?;
    local_studio_core::check_scratch_agent(&agent).map_err(|error| anyhow::anyhow!(error))?;
    let absent = json!({
        "baseUrl": format!("http://127.0.0.1:{}/v1", free_port()?),
        "api": "openai-completions",
        "apiKey": "absent",
        "models": [{ "id": "absent", "input": ["text"], "contextWindow": 8192 }]
    });
    write("models.json", &serde_json::to_vec(&json!({ "providers": { "handoff-absent": absent } }))?)?;
    write("settings.json", br#"{"onboardingShown":true}"#)?;
    Ok(())
}

async fn stop_daemon(socket: &Path) -> anyhow::Result<()> {
    let Ok((client, _events)) = DaemonClient::connect(socket).await else {
        println!("ok   no daemon was running");
        return Ok(());
    };
    let pid = client.hello().get("supervisorPid").and_then(Value::as_u64).and_then(|pid| u32::try_from(pid).ok());
    let command = DaemonCommand::Shutdown { id: None, force: Some(true), if_idle: None, rest: Map::default() };
    client.request_ok(command).await.ok();
    client.close();
    let Some(pid) = pid else { return Ok(()) };
    until(&format!("daemon {pid} stopped"), QUICK, || Ok(!alive(pid))).await?;
    Ok(())
}

fn write_settings(data: &Path, settings: &Value) -> anyhow::Result<()> {
    std::fs::create_dir_all(data.join("remote"))?;
    std::fs::write(data.join("remote").join("serve.json"), serde_json::to_vec_pretty(settings)?)?;
    Ok(())
}

async fn waits_for_holder(run: &mut Run) -> anyhow::Result<()> {
    let first = run.start_app("first app")?;
    run.ready(first, "first app", SETUP).await?;
    let first_pid = run.app_pid(first)?;
    let host = format!("127.0.0.1:{}", run.port);
    until("the first app serves /v1/health in process", QUICK, || Ok(owner(&run.data) == Some(first_pid))).await?;
    let port = run.port;
    let started = Instant::now();
    while health(port, &host).await != Some(200) {
        ensure!(started.elapsed() < QUICK, "the first app's in-process server never answered /v1/health");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let serve = run.start_serve()?;
    tokio::time::sleep(Duration::from_secs(5)).await;
    ensure!(run.serve_alive()?, "serve exited while the first app held the lock:\n{}", run.serve_log());
    let log = run.serve_log();
    ensure!(log.matches(WAITING).count() == 1, "serve did not log the wait exactly once:\n{log}");
    ensure!(log.contains(&format!("process {first_pid}")), "serve's wait line does not name the holder:\n{log}");
    ensure!(owner(&run.data) == Some(first_pid), "the lock moved while the first app held it");
    println!("ok   serve {serve} waits while app {first_pid} holds the lock, logs it once and keeps running");
    let quit = run.quit_app(first).await?;
    let took = until("serve owns the lock after the first app quits", BACK, || Ok(owner(&run.data) == Some(serve)))
        .await
        .map(|_| quit.elapsed())?;
    run.report.push(format!("first app quit to serve owning the lock: {:.2} s", took.as_secs_f64()));
    let started = Instant::now();
    while health(port, &host).await != Some(200) {
        ensure!(started.elapsed() < QUICK, "serve never answered /v1/health:\n{}", run.serve_log());
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let status = run.json(&["status", "--no-load"])?;
    expect(&status, "/state", &json!("serving"), "status --json")?;
    expect(&status, "/owner/pid", &json!(serve), "status --json")?;
    expect(&status, "/owner/kind", &json!("serve"), "status --json")?;
    expect(&status, "/listeners/0/status", &json!(200), "status --json")?;
    expect(&status, "/serveJson/port", &json!(run.port), "status --json")?;
    expect(&status, "/service/state", &json!("not installed"), "status --json")?;
    println!("ok   status --json: {status}");
    Ok(())
}

async fn tunnel_live(run: &mut Run, phone: &Phone, link: Link) -> anyhow::Result<Link> {
    let tunnel = run.tunnel;
    ensure!(health(tunnel, TUNNEL).await.is_none(), "a tunnel listener ran before any tunnel host was allowed");
    let started = Instant::now();
    let added = tokio::task::block_in_place(|| run.json(&["allow-tunnel-host", TUNNEL]))?;
    let took = started.elapsed();
    expect(&added, "/state", &json!("added"), "allow-tunnel-host --json")?;
    expect(&added, "/allowTunnelHosts", &json!([TUNNEL]), "allow-tunnel-host --json")?;
    expect(&added, "/tunnelPort", &json!(tunnel), "allow-tunnel-host --json")?;
    expect(&added, "/live", &json!(true), "allow-tunnel-host --json")?;
    expect(&added, "/owner/kind", &json!("serve"), "allow-tunnel-host --json")?;
    println!("ok   allow-tunnel-host --json: {added}");
    let health_now = health(tunnel, TUNNEL).await;
    ensure!(health_now == Some(200), "live was true but the tunnel host got {health_now:?}");
    println!("ok   allow-tunnel-host {TUNNEL} started the tunnel listener live: 200 after {:.2} s", took.as_secs_f64());
    run.report.push(format!("allow-tunnel-host to live on the tunnel listener: {:.2} s", took.as_secs_f64()));
    let again = tokio::task::block_in_place(|| run.json(&["allow-tunnel-host", TUNNEL]))?;
    expect(&again, "/state", &json!("unchanged"), "a repeated allow-tunnel-host --json")?;
    expect(&again, "/live", &json!(true), "a repeated allow-tunnel-host --json")?;
    println!("ok   a repeated allow-tunnel-host --json says unchanged");
    let other = health(tunnel, "other.handoff.test").await;
    ensure!(other == Some(403), "another Host got {other:?} on the tunnel listener");
    let crossed = health(run.port, TUNNEL).await;
    ensure!(crossed == Some(403), "the tunnel host got {crossed:?} on the local listener");
    println!(
        "ok   /v1/health on the tunnel listener: 200 for {TUNNEL}, 403 for another Host, 403 on the local listener"
    );
    drop(link);
    let (link, hello) = reconnect(run.port, phone, "after allow-tunnel-host").await?;
    let wanted = format!("wss://{TUNNEL}/v1/ws");
    ensure!(urls(&hello).contains(&wanted), "machine.urls lacks {wanted}: {hello}");
    println!("ok   machine.urls follows the hosts: {:?}", urls(&hello));
    Ok(link)
}

async fn takeover(run: &mut Run, phone: &Phone, mut link: Link) -> anyhow::Result<Link> {
    let serve = run.serve_pid()?;
    let second = run.start_app("second app")?;
    let (took, code) = tokio::join!(run.ready(second, "second app", TAKEOVER), link.closed(QUICK));
    let took = took?;
    ensure!(took <= TAKEOVER, "the takeover took {:.1} s", took.as_secs_f64());
    let app = run.app_pid(second)?;
    ensure!(owner(&run.data) == Some(app), "the second app started without owning the lock");
    println!("ok   a take_over_serve Studio got the lock from serve {serve} in {:.2} s", took.as_secs_f64());
    run.report.push(format!("takeover from serve to the app: {:.2} s", took.as_secs_f64()));
    let code = code?;
    ensure!(code == Some(1012), "the phone was closed with {code:?}, expected 1012");
    println!("ok   the paired phone's socket was closed with 1012");
    ensure!(run.serve_alive()?, "serve exited on SIGUSR1:\n{}", run.serve_log());
    until("serve logs the handover", QUICK, || Ok(run.serve_log().contains("SIGUSR1: released"))).await?;
    let (link, hello) = reconnect(run.port, phone, "to the in-process server").await?;
    ensure!(owner(&run.data) == Some(app), "the lock left the app while the phone reconnected");
    let wanted = format!("wss://{TUNNEL}/v1/ws");
    ensure!(urls(&hello).contains(&wanted), "the in-process server's machine.urls lacks {wanted}: {hello}");
    let tunnel = health(run.tunnel, TUNNEL).await;
    ensure!(tunnel == Some(200), "the in-process server's tunnel listener answered {tunnel:?}");
    println!("ok   the app's in-process server holds the lock, serves the tunnel host and the phone");
    let status = tokio::task::block_in_place(|| run.json(&["status", "--no-load"]))?;
    expect(&status, "/owner/pid", &json!(app), "status --json under the app")?;
    expect(&status, "/state", &json!("serving"), "status --json under the app")?;
    let added = tokio::task::block_in_place(|| run.json(&["allow-tunnel-host", SECOND]))?;
    expect(&added, "/state", &json!("added"), "allow-tunnel-host --json under the app")?;
    expect(&added, "/allowTunnelHosts", &json!([TUNNEL, SECOND]), "allow-tunnel-host --json under the app")?;
    expect(&added, "/live", &json!(true), "allow-tunnel-host --json under the app")?;
    expect(&added, "/owner/pid", &json!(app), "allow-tunnel-host --json under the app")?;
    let second = health(run.tunnel, SECOND).await;
    ensure!(second == Some(200), "{SECOND} got {second:?} from the app's tunnel listener");
    println!("ok   the app's in-process server picked up {SECOND} live (no SIGHUP): {added}");
    allow_host_live(run, "box-app.handoff.test", app, "the app").await?;
    Ok(link)
}

async fn hand_back(run: &mut Run, phone: &Phone, mut link: Link) -> anyhow::Result<Link> {
    let serve = run.serve_pid()?;
    let (quit, code) = tokio::join!(run.quit_app(1), link.closed(QUICK));
    let quit = quit?;
    let code = code?;
    ensure!(code == Some(1012), "the app's quit closed the phone with {code:?}, expected 1012");
    println!("ok   the app's quit closed the phone with 1012");
    let left = BACK.checked_sub(quit.elapsed()).unwrap_or_default();
    until("serve owns the lock again after the app quits", left, || Ok(owner(&run.data) == Some(serve))).await?;
    let took = quit.elapsed();
    ensure!(took <= BACK, "serve took the lock back {:.2} s after the app quit", took.as_secs_f64());
    run.report.push(format!("app quit to serve owning the lock: {:.2} s", took.as_secs_f64()));
    let (link, _) = reconnect(run.port, phone, "back to serve").await?;
    Ok(link)
}

async fn tunnel_removed(run: &Run) -> anyhow::Result<()> {
    let removed = tokio::task::block_in_place(|| run.json(&["allow-tunnel-host", "--remove", TUNNEL]))?;
    expect(&removed, "/state", &json!("removed"), "allow-tunnel-host --remove --json")?;
    expect(&removed, "/allowTunnelHosts", &json!([SECOND]), "allow-tunnel-host --remove --json")?;
    expect(&removed, "/live", &json!(true), "allow-tunnel-host --remove --json")?;
    let gone = health(run.tunnel, TUNNEL).await;
    let kept = health(run.tunnel, SECOND).await;
    ensure!(
        gone == Some(403) && kept == Some(200),
        "after the removal {TUNNEL} got {gone:?} and {SECOND} got {kept:?}"
    );
    println!("ok   allow-tunnel-host --remove --json: {removed}; {TUNNEL} now gets 403, {SECOND} still 200");
    let missing = tokio::task::block_in_place(|| run.json(&["allow-tunnel-host", "--remove", TUNNEL]))?;
    expect(&missing, "/state", &json!("unchanged"), "a repeated --remove --json")?;
    let output = Command::new(&run.bin)
        .args(["allow-tunnel-host", "--json", "not a host!", "--data-dir"])
        .arg(&run.data)
        .stdin(Stdio::null())
        .output()?;
    let failed: Value = serde_json::from_slice(&output.stdout)?;
    ensure!(!output.status.success(), "a bad host name was accepted");
    expect(&failed, "/state", &json!("error"), "a bad host name with --json")?;
    println!("ok   a bad host name with --json exits 1 and prints {failed}");
    Ok(())
}

async fn allow_host_live(run: &Run, host: &str, owner: u32, label: &str) -> anyhow::Result<()> {
    let before = health(run.port, host).await;
    ensure!(before == Some(403), "{host} got {before:?} on the local listener before allow-host");
    let added = tokio::task::block_in_place(|| run.json(&["allow-host", host]))?;
    expect(&added, "/state", &json!("added"), "allow-host --json")?;
    expect(&added, "/port", &json!(run.port), "allow-host --json")?;
    expect(&added, "/live", &json!(true), "allow-host --json")?;
    expect(&added, "/owner/pid", &json!(owner), "allow-host --json")?;
    ensure!(
        added.get("allowHosts").and_then(Value::as_array).is_some_and(|hosts| hosts.contains(&json!(host))),
        "allow-host --json lists no {host}: {added}"
    );
    let now = health(run.port, host).await;
    ensure!(now == Some(200), "live was true but {host} got {now:?} on the local listener");
    let crossed = health(run.tunnel, host).await;
    ensure!(crossed == Some(403), "{host} got {crossed:?} on the tunnel listener");
    let again = tokio::task::block_in_place(|| run.json(&["allow-host", host]))?;
    expect(&again, "/state", &json!("unchanged"), "a repeated allow-host --json")?;
    let removed = tokio::task::block_in_place(|| run.json(&["allow-host", "--remove", host]))?;
    expect(&removed, "/state", &json!("removed"), "allow-host --remove --json")?;
    expect(&removed, "/live", &json!(true), "allow-host --remove --json")?;
    let gone = health(run.port, host).await;
    ensure!(gone == Some(403), "after allow-host --remove {host} got {gone:?}");
    println!("ok   allow-host --json under {label}: {added}; 200 on the local listener, 403 on the tunnel listener, 403 again after --remove");
    Ok(())
}

fn default_tunnel_port(run: &Run) -> anyhow::Result<()> {
    let data = run.root.join("fresh");
    let args = ["allow-tunnel-host", "fresh.handoff.test", "--json"];
    let added = json_of(Command::new(&run.bin).args(args).arg("--data-dir").arg(&data), &args)?;
    expect(&added, "/state", &json!("added"), "allow-tunnel-host --json on a fresh data dir")?;
    expect(&added, "/tunnelPort", &json!(18094), "allow-tunnel-host --json on a fresh data dir")?;
    expect(&added, "/live", &json!(false), "allow-tunnel-host --json on a fresh data dir")?;
    expect(&added, "/owner", &Value::Null, "allow-tunnel-host --json on a fresh data dir")?;
    let written: Value = serde_json::from_str(&read(&data.join("remote").join("serve.json")))?;
    ensure!(
        written.get("tunnelPort") == Some(&json!(18094))
            && written.get("allowTunnelHosts") == Some(&json!(["fresh.handoff.test"])),
        "allow-tunnel-host wrote {written}"
    );
    println!("ok   allow-tunnel-host --json on a fresh data dir: {added} (nothing listens on 18094 for it)");
    let args = ["allow-host", "fresh-box.handoff.test", "--json"];
    let local = json_of(Command::new(&run.bin).args(args).arg("--data-dir").arg(&data), &args)?;
    expect(&local, "/state", &json!("added"), "allow-host --json on a fresh data dir")?;
    expect(&local, "/port", &json!(18093), "allow-host --json on a fresh data dir")?;
    expect(&local, "/allowHosts", &json!(["fresh-box.handoff.test"]), "allow-host --json on a fresh data dir")?;
    expect(&local, "/live", &json!(false), "allow-host --json on a fresh data dir")?;
    expect(&local, "/owner", &Value::Null, "allow-host --json on a fresh data dir")?;
    println!("ok   allow-host --json on a fresh data dir: {local}");
    Ok(())
}

struct Services {
    bin: PathBuf,
    home: PathBuf,
    shim: PathBuf,
    calls: PathBuf,
}

impl Services {
    fn new(run: &Run) -> anyhow::Result<Self> {
        let home = run.root.join("service-home");
        std::fs::create_dir_all(&home)?;
        let calls = run.root.join("tailscale-calls.log");
        let shim = run.root.join("tailscale-shim");
        std::fs::write(
            &shim,
            format!(
                "#!/bin/sh\necho \"$*\" >> '{}'\ncase \"$1\" in status) echo '{{\"Self\":{{\"DNSName\":\"{TAILNET}.\"}}}}' ;; esac\n",
                calls.display()
            ),
        )?;
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755))?;
        }
        Ok(Self { bin: run.bin.clone(), home, shim, calls })
    }

    fn json(&self, dir: &Path, args: &[&str]) -> anyhow::Result<Value> {
        json_of(
            Command::new(&self.bin)
                .args(args)
                .arg("--json")
                .arg("--data-dir")
                .arg(dir)
                .env("HOME", &self.home)
                .env("LS_TAILSCALE", &self.shim),
            args,
        )
    }

    fn commands(&self, tail: impl Fn(&str) -> Vec<String>) -> Value {
        let shim = self.shim.to_string_lossy().into_owned();
        json!(["/v1/health", "/v1/ws"]
            .iter()
            .map(|path| std::iter::once(shim.clone()).chain(tail(path)).collect::<Vec<_>>())
            .collect::<Vec<_>>())
    }

    fn calls(&self) -> String {
        read(&self.calls)
    }
}

fn tailnet_planned(services: &Services, installed: &Value, port: &str) -> anyhow::Result<()> {
    let what = "install --tailscale --no-load --json";
    let mounts = services.commands(|path| {
        [
            "serve",
            "--bg",
            "--yes",
            "--https=443",
            &format!("--set-path={path}"),
            &format!("http://127.0.0.1:{port}{path}"),
        ]
        .map(String::from)
        .to_vec()
    });
    expect(installed, "/tailscale/state", &json!("planned"), what)?;
    expect(installed, "/tailscale/host", &json!(TAILNET), what)?;
    expect(installed, "/tailscale/paths", &json!(["/v1/health", "/v1/ws"]), what)?;
    expect(installed, "/tailscale/commands", &mounts, what)?;
    expect(installed, "/serveJson/tailscale/paths", &json!([]), what)?;
    ensure!(
        services.calls().lines().collect::<Vec<_>>() == ["status --json"],
        "install --no-load ran tailscale beyond status:\n{}",
        services.calls()
    );
    println!("ok   install --tailscale --no-load plans only /v1/health and /v1/ws, never /: {mounts}");
    Ok(())
}

fn tailnet_unmounts(services: &Services, settings: &Path, written: &Value) -> anyhow::Result<Value> {
    let mut mounted = written.clone();
    mounted["tailscale"]["paths"] = json!(["/v1/health", "/v1/ws"]);
    std::fs::write(settings, serde_json::to_vec_pretty(&mounted)?)?;
    let data = settings.parent().and_then(Path::parent).context("serve.json has no data dir")?;
    let removed = services.json(data, &["uninstall", "--no-load"])?;
    let unmounts = services
        .commands(|path| ["serve", "--https=443", &format!("--set-path={path}"), "off"].map(String::from).to_vec());
    expect(&removed, "/tailscale/state", &json!("left"), "uninstall --no-load --json")?;
    expect(&removed, "/tailscale/commands", &unmounts, "uninstall --no-load --json")?;
    ensure!(services.calls().lines().count() == 1, "uninstall --no-load ran tailscale:\n{}", services.calls());
    println!("ok   uninstall --no-load plans removing only the two paths install added: {unmounts}");
    Ok(removed)
}

fn route_install(run: &Run, services: &Services, port: &str) -> anyhow::Result<()> {
    let routed = run.root.join("route-data");
    std::fs::create_dir_all(routed.join("tunnel"))?;
    std::fs::write(
        routed.join("tunnel").join("route.json"),
        json!({"v":1,"owner":"local-studio","route":"tailnet","hostname":ROUTED,"aliases":[],"ports":{"mcp":8787,"serve":18093}}).to_string(),
    )?;
    let before = services.calls();
    let bin = run.bin.to_string_lossy().into_owned();
    let installed = services.json(&routed, &["install", "--no-load", "--tailscale", "--port", port, "--bin", &bin])?;
    let what = "install --tailscale with route.json";
    expect(&installed, "/tailscale/state", &json!("route"), what)?;
    expect(&installed, "/tailscale/host", &json!(ROUTED), what)?;
    expect(&installed, "/tailscale/commands", &json!([]), what)?;
    expect(&installed, "/serveJson/allowHosts", &json!([ROUTED]), what)?;
    ensure!(services.calls() == before, "install with route.json ran tailscale:\n{}", services.calls());
    let removed = services.json(&routed, &["uninstall", "--no-load"])?;
    expect(&removed, "/tailscale/state", &json!("none"), "uninstall after a route.json install")?;
    expect(&removed, "/tailscale/commands", &json!([]), "uninstall after a route.json install")?;
    ensure!(services.calls() == before, "uninstall after a route.json install ran tailscale:\n{}", services.calls());
    println!("ok   with route.json, install --tailscale mounts nothing and allows {ROUTED}; uninstall removes nothing: {installed}");
    Ok(())
}

fn service_files(run: &Run) -> anyhow::Result<()> {
    let services = Services::new(run)?;
    let home = &services.home;
    let data = run.root.join("service-data");
    let port = free_port()?.to_string();
    let service = |args: &[&str]| services.json(&data, args);
    let bin = run.bin.to_string_lossy().into_owned();
    let eukhe = run.eukhe.to_string_lossy().into_owned();
    let installed = service(&[
        "install",
        "--no-load",
        "--tailscale",
        "--port",
        &port,
        "--allow-host",
        "box.handoff.test",
        "--bin",
        &bin,
        "--eukhe",
        &eukhe,
    ])?;
    let (unit, needles): (PathBuf, Vec<&str>) = if cfg!(target_os = "macos") {
        (
            home.join("Library/LaunchAgents/org.local.studio.serve.plist"),
            vec!["<key>RunAtLoad</key>", "<key>KeepAlive</key>", "local-studio-serve.log", "EUKHE_BIN"],
        )
    } else {
        (
            home.join(".config/systemd/user/local-studio-serve.service"),
            vec!["Restart=always", "WantedBy=default.target", "EUKHE_BIN"],
        )
    };
    let settings = data.join("remote").join("serve.json");
    expect(&installed, "/state", &json!("written"), "install --no-load --json")?;
    expect(&installed, "/loaded", &json!(false), "install --no-load --json")?;
    expect(&installed, "/unit", &json!(unit), "install --no-load --json")?;
    expect(&installed, "/settingsPath", &json!(settings), "install --no-load --json")?;
    expect(&installed, "/serveJson/allowHosts", &json!(["box.handoff.test", TAILNET]), "install --no-load --json")?;
    tailnet_planned(&services, &installed, &port)?;
    let text = read(&unit);
    ensure!(!text.is_empty(), "install wrote no {}: {installed}", unit.display());
    for needle in needles.iter().copied().chain([bin.as_str(), eukhe.as_str(), "serve", "--data-dir"]) {
        ensure!(text.contains(needle), "{} lacks {needle}:\n{text}", unit.display());
    }
    if cfg!(target_os = "macos") {
        ensure!(home.join("Library/Logs").is_dir(), "install did not create the log folder");
    }
    let written: Value = serde_json::from_str(&read(&settings))?;
    ensure!(
        written.get("port").and_then(Value::as_u64) == port.parse().ok()
            && written.get("allowHosts") == Some(&json!(["box.handoff.test", TAILNET])),
        "install wrote {written}"
    );
    println!("ok   install --no-load --json: {installed}");
    let status = service(&["status", "--no-load"])?;
    expect(&status, "/state", &json!("stopped"), "status --json with nothing running")?;
    expect(&status, "/owner", &Value::Null, "status --json with nothing running")?;
    expect(&status, "/service/state", &json!("installed"), "status --json with nothing running")?;
    expect(&status, "/listeners/0/listening", &json!(false), "status --json with nothing running")?;
    println!("ok   status --json with the service written and nothing running: {status}");
    let removed = tailnet_unmounts(&services, &settings, &written)?;
    expect(&removed, "/state", &json!("uninstalled"), "uninstall --no-load --json")?;
    expect(&removed, "/removed", &json!([unit, settings]), "uninstall --no-load --json")?;
    ensure!(!unit.exists() && !settings.exists(), "uninstall left {} or {}", unit.display(), settings.display());
    let again = service(&["uninstall", "--no-load"])?;
    expect(&again, "/state", &json!("absent"), "a repeated uninstall --json")?;
    println!("ok   uninstall --no-load --json removed both: {removed}; again: {again}");
    route_install(run, &services, &port)
}

async fn drive(run: &mut Run) -> anyhow::Result<()> {
    waits_for_holder(run).await?;
    let (phone, link, hello) = pair(run).await?;
    ensure!(
        hello.get("snapshot").and_then(Value::as_array).is_some_and(|snapshot| !snapshot.is_empty()),
        "pairing hello had no snapshot"
    );
    println!("ok   a phone paired with serve and got a snapshot");
    let link = tunnel_live(run, &phone, link).await?;
    allow_host_live(run, "box-serve.handoff.test", run.serve_pid()?, "serve").await?;
    let link = takeover(run, &phone, link).await?;
    let mut link = hand_back(run, &phone, link).await?;
    tunnel_removed(run).await?;
    Command::new("kill").args(["-TERM", &run.serve_pid()?.to_string()]).status()?;
    let code = link.closed(QUICK).await?;
    run.stop_serve()?;
    ensure!(code == Some(1001), "serve's stop closed the phone with {code:?}, expected 1001");
    println!("ok   serve's stop closed the phone with 1001");
    default_tunnel_port(run)?;
    service_files(run)?;
    Ok(())
}

async fn smoke(eukhe: &Path, root: &Path) -> anyhow::Result<()> {
    let eukhe = std::fs::canonicalize(eukhe)?;
    std::fs::create_dir_all(root)?;
    let root = std::fs::canonicalize(root)?;
    let me = std::env::current_exe()?;
    let bin = me
        .parent()
        .and_then(Path::parent)
        .map(|dir| dir.join("local-studio-serve"))
        .filter(|bin| bin.is_file())
        .context("build local-studio-serve first: cargo build -p local-studio-remote --bins --examples")?;
    let data = root.join("data");
    std::fs::create_dir_all(root.join("home"))?;
    std::env::set_var("HOME", root.join("home"));
    std::env::set_var("EUKHE_DAEMON_SOCKET", root.join("d.sock"));
    std::env::set_var("EUKHE_CODING_AGENT_DIR", root.join("agent"));
    std::env::set_var("LOCAL_STUDIO_DATA_DIR", &data);
    agent_dir(&root)?;
    let port = free_port()?;
    let tunnel = loop {
        let tunnel = free_port()?;
        if tunnel != port {
            break tunnel;
        }
    };
    write_settings(&data, &json!({ "name": "handoff-box", "port": port, "tunnelPort": tunnel }))?;
    let mut run = Run {
        me,
        bin,
        eukhe,
        root: root.clone(),
        data,
        port,
        tunnel,
        serve: None,
        apps: Vec::new(),
        report: Vec::new(),
    };
    let outcome = drive(&mut run).await;
    if outcome.is_err() {
        let log = run.serve_log();
        let lines: Vec<&str> = log.lines().collect();
        println!("     serve log tail:\n{}", lines[lines.len().saturating_sub(30)..].join("\n"));
        println!("     app log:\n{}", read(&root.join("app.log")));
    }
    let cleaned = run.cleanup();
    let stopped = stop_daemon(&root.join("d.sock")).await;
    for line in &run.report {
        println!("     {line}");
    }
    outcome?;
    cleaned?;
    stopped?;
    println!("PASS serve handoff smoke");
    Ok(())
}
