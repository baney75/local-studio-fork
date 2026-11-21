use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context};
use eukhe_client::daemon_client::DaemonClient;
use eukhe_types::daemon::DaemonCommand;
use local_studio_core::{
    AccessMode, Connection, DaemonLaunch, Delivery, ItemKind, NoticeLevel, NotificationKind, PromptDraft, Studio,
    StudioConfig, ThreadSettings, TranscriptOp, Update, Waker, WorktreeChoice,
};
use serde_json::Map;

#[allow(dead_code)]
#[path = "support/harness.rs"]
mod harness;

use harness::Signal;

const SETUP_TIMEOUT: Duration = Duration::from_secs(60);
const CHECK_TIMEOUT: Duration = Duration::from_secs(12);

#[derive(Default)]
struct Watch {
    connection: Option<Connection>,
    projects: Vec<String>,
    started: BTreeMap<String, String>,
    thread: Option<String>,
    busy: bool,
    replies: BTreeMap<u64, String>,
    notices: BTreeMap<u64, String>,
    errors: Vec<String>,
    failed: Vec<(String, String)>,
    refused: Vec<String>,
    misses: Vec<String>,
}

impl Watch {
    fn replied(&self, text: &str) -> Option<&String> {
        self.replies.values().find(|reply| reply.contains(text))
    }

    fn noticed(&self, text: &str) -> bool {
        self.notices.values().any(|notice| notice.contains(text))
    }

    fn miss(&mut self, check: String) {
        println!("MISS {check}");
        self.misses.push(check);
    }
}

fn observe(watch: &mut Watch, id: u64, kind: &ItemKind) {
    match kind {
        ItemKind::Assistant { blocks, streaming: false, .. } => {
            let text: String = blocks
                .iter()
                .filter_map(|block| match block {
                    local_studio_core::Block::Text { text } => Some(text.as_str()),
                    local_studio_core::Block::Thinking { .. } => None,
                })
                .collect();
            if !text.trim().is_empty() {
                watch.replies.insert(id, text);
            }
        }
        ItemKind::Notice { text, .. } => {
            watch.notices.insert(id, text.clone());
        }
        _ => {}
    }
}

fn apply(watch: &mut Watch, update: Update) {
    match update {
        Update::Transcript { thread, op } if watch.thread.as_deref() == Some(thread.as_str()) => match op {
            TranscriptOp::Reset { items } => {
                watch.replies.clear();
                watch.notices.clear();
                for item in &items {
                    observe(watch, item.id, &item.kind);
                }
            }
            TranscriptOp::Insert { item, .. } | TranscriptOp::Update { item, .. } => {
                observe(watch, item.id, &item.kind);
            }
        },
        Update::Thread { thread, info } if watch.thread.as_deref() == Some(thread.as_str()) => watch.busy = info.busy,
        Update::Connection { state } => watch.connection = Some(state),
        Update::Projects { projects } => watch.projects = projects.into_iter().map(|project| project.id).collect(),
        Update::ThreadStarted { draft, thread } => {
            watch.started.insert(draft, thread);
        }
        Update::Notice { level, text } => {
            println!("     notice {level:?}: {text}");
            if level == NoticeLevel::Error {
                watch.errors.push(text);
            }
        }
        Update::Notification { thread, kind: NotificationKind::Failed, body, .. } => watch.failed.push((thread, body)),
        Update::RequestFailed { message, .. } => {
            println!("     request failed: {message}");
            watch.refused.push(message);
        }
        _ => {}
    }
}

struct Run {
    studio: Arc<Studio>,
    signal: Arc<Signal>,
    watch: Watch,
    sent: u32,
}

impl Run {
    fn until(&mut self, timeout: Duration, done: impl Fn(&Watch) -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            for update in self.studio.tick() {
                apply(&mut self.watch, update);
            }
            if done(&self.watch) {
                return true;
            }
            let Some(left) = deadline.checked_duration_since(Instant::now()) else { return false };
            self.signal.wait(left.min(Duration::from_millis(50)));
        }
    }

    fn setup(&mut self, step: &str, done: impl Fn(&Watch) -> bool) -> anyhow::Result<()> {
        if self.until(SETUP_TIMEOUT, done) {
            println!("ok   {step}");
            Ok(())
        } else {
            bail!("timed out waiting for: {step}")
        }
    }

    fn check(&mut self, step: &str, done: impl Fn(&Watch) -> bool) -> bool {
        if self.until(CHECK_TIMEOUT, done) {
            println!("ok   {step}");
            true
        } else {
            self.watch.miss(format!("{step} (not within {} s)", CHECK_TIMEOUT.as_secs()));
            false
        }
    }

    fn thread(&self) -> String {
        self.watch.thread.clone().unwrap_or_default()
    }

    fn send(&mut self, text: &str) {
        self.sent += 1;
        println!("     send {text:?}");
        self.studio.send(format!("send-{}", self.sent), self.thread(), prompt(text), Delivery::Queue);
    }

    fn start(&mut self, label: &str, (provider, model): (&str, &str), thinking: Option<&str>) -> anyhow::Result<()> {
        self.start_with(label, (provider, model), thinking, AccessMode::FullAccess)
    }

    fn start_with(
        &mut self,
        label: &str,
        (provider, model): (&str, &str),
        thinking: Option<&str>,
        access: AccessMode,
    ) -> anyhow::Result<()> {
        self.watch.thread = None;
        self.watch.busy = false;
        self.watch.replies.clear();
        self.watch.notices.clear();
        let settings = ThreadSettings {
            provider: Some(provider.to_string()),
            model: Some(model.to_string()),
            thinking_level: thinking.map(str::to_string),
            access,
            worktree: WorktreeChoice::Local,
        };
        let draft = format!("draft-{label}");
        self.studio.start_thread(draft.clone(), self.watch.projects.first().cloned(), prompt("hello"), settings);
        self.setup(&format!("{label}: thread started"), |watch| watch.started.contains_key(&draft))?;
        let thread = self.watch.started.get(&draft).cloned().context("thread id")?;
        self.watch.thread = Some(thread.clone());
        self.studio.select_thread(Some(thread));
        self.setup(&format!("{label}: first reply"), |watch| watch.replied("reply to hello").is_some() && !watch.busy)
    }

    fn slow(&mut self, text: &str) -> anyhow::Result<()> {
        self.send(text);
        self.setup(&format!("{text:?} is working"), |watch| watch.busy)
    }
}

fn prompt(text: &str) -> PromptDraft {
    PromptDraft { text: text.to_string(), images: Vec::new(), files: Vec::new() }
}

fn ran_at(run: &mut Run, text: &str, effort: &str) {
    run.send(text);
    let reply = format!("reply to {text}");
    if run.check(&format!("{text:?} is answered"), |watch| watch.replied(&reply).is_some()) {
        let reply = run.watch.replied(&reply).cloned().unwrap_or_default();
        if reply.contains(&format!("at effort {effort}")) {
            println!("ok   {text:?} ran on the new effort: {reply}");
        } else {
            run.watch.miss(format!("{text:?} did not run on the new effort: {reply}"));
        }
    }
}

fn claude_restart_keeps_message(run: &mut Run) -> anyhow::Result<()> {
    run.start("claude-restart", ("claude-code", "haiku"), Some("low"))?;
    run.slow("slow alpha")?;
    run.studio.set_thinking_level(run.thread(), "medium".to_string());
    run.send("queued bravo");
    if !run.check("a message sent during the turn that changed the effort is answered", |watch| {
        watch.replied("reply to queued bravo").is_some()
    }) {
        return Ok(());
    }
    run.setup("the thread is idle after the held message", |watch| !watch.busy)?;
    ran_at(run, "charlie", "medium");
    Ok(())
}

fn claude_stop_drops_held(run: &mut Run) -> anyhow::Result<()> {
    run.start("claude-discard", ("claude-code", "haiku"), Some("low"))?;
    run.slow("slow discard tango")?;
    run.studio.set_thinking_level(run.thread(), "medium".to_string());
    run.send("uniform");
    run.studio.abort(run.thread());
    run.setup("the stopped turn ended", |watch| !watch.busy)?;
    run.until(Duration::from_secs(5), |_| false);
    ran_at(run, "victor", "medium");
    Ok(())
}

fn claude_stop_after_effort_change(run: &mut Run) -> anyhow::Result<()> {
    run.start("claude-stop", ("claude-code", "haiku"), Some("low"))?;
    let thread = run.thread();
    let errors = run.watch.errors.len();
    let failed = run.watch.failed.len();
    run.slow("slow quota delta")?;
    run.studio.set_thinking_level(thread.clone(), "high".to_string());
    run.check("an account stop after an effort change shows the account error", |watch| {
        watch.errors[errors..].iter().any(|error| error.contains("account problem"))
    });
    run.check("an account stop after an effort change raises a Failed notification", |watch| {
        watch.failed[failed..].iter().any(|(id, body)| *id == thread && body.contains("account problem"))
    });
    Ok(())
}

fn codex_start_race(run: &mut Run) -> anyhow::Result<()> {
    run.start("codex-race", ("codex", "fake-mini"), None)?;
    let refused = run.watch.refused.len();
    run.slow("slow echo")?;
    run.send("lag foxtrot");
    run.setup("the running turn ended and the held message is starting", |watch| {
        watch.replied("reply to slow echo").is_some() && !watch.busy
    })?;
    run.send("golf");
    let both = run.check("a send while a held message is starting waits for it", |watch| {
        watch.replied("reply to lag foxtrot").is_some() && watch.replied("reply to golf").is_some()
    });
    if both {
        let order: Vec<&String> = run.watch.replies.values().collect();
        let foxtrot = order.iter().position(|reply| reply.contains("foxtrot"));
        let golf = order.iter().position(|reply| reply.contains("golf"));
        if foxtrot < golf {
            println!("ok   the held message ran before the later send");
        } else {
            run.watch.miss(format!("the later send ran before the held message: {order:?}"));
        }
    }
    if run.watch.refused.len() > refused {
        let refused = run.watch.refused[refused..].join("; ");
        run.watch.miss(format!("a send was refused while a held message was starting: {refused}"));
    }
    Ok(())
}

fn codex_failed_start(run: &mut Run) -> anyhow::Result<()> {
    run.start("codex-reject", ("codex", "fake-mini"), None)?;
    run.slow("slow hotel")?;
    run.send("reject india");
    run.send("juliet");
    run.check("a held message after one Codex refused to start still runs", |watch| {
        watch.replied("reply to juliet").is_some()
    });
    Ok(())
}

fn codex_drop(run: &mut Run, label: &str, first: &str, held: &str) -> anyhow::Result<()> {
    run.start(label, ("codex", "fake-mini"), None)?;
    run.slow(first)?;
    run.send(held);
    run.check(&format!("{label}: the dropped held message is shown in the transcript"), |watch| watch.noticed(held));
    Ok(())
}

fn codex_unexpected_approval(run: &mut Run) -> anyhow::Result<()> {
    run.start_with("codex-skill", ("codex", "fake-mini"), None, AccessMode::Supervised)?;
    let thread = run.thread();
    let mut watch = harness::Watch::default();
    watch.thread = Some(thread.clone());
    let step = "a Supervised turn that asks an approval no step expects";
    let outcome = harness::turn(&run.studio, &run.signal, &mut watch, &thread, "skill quebec", step);
    let denied: Vec<String> = watch.denied.iter().map(|(approval, _)| approval.command.clone()).collect();
    match outcome {
        Ok(reply) if reply.contains("after decline") && denied.iter().any(|command| command.contains("SKILL.md")) => {
            println!("ok   the unexpected approval {denied:?} was denied and the turn went on: {reply}");
        }
        Ok(reply) => run.watch.miss(format!("the turn ended without a deny of its approval: {reply}")),
        Err(error) => run.watch.miss(format!("{error:#}")),
    }
    Ok(())
}

fn scenarios(run: &mut Run) -> anyhow::Result<()> {
    claude_restart_keeps_message(run)?;
    claude_stop_after_effort_change(run)?;
    claude_stop_drops_held(run)?;
    codex_start_race(run)?;
    codex_failed_start(run)?;
    codex_drop(run, "codex-exit", "slow exit kilo", "lima")?;
    codex_drop(run, "codex-start-exit", "slow romeo", "crash sierra")?;
    codex_drop(run, "codex-error-stop", "slow quota-error mike", "november")?;
    codex_drop(run, "codex-failed-stop", "slow quota-fail oscar", "papa")?;
    codex_unexpected_approval(run)
}

fn git(work: &Path, args: &[&str]) -> anyhow::Result<()> {
    let status = Command::new("git").current_dir(work).args(args).status()?;
    ensure!(status.success(), "git {args:?} failed");
    Ok(())
}

fn scratch_repo(work: &Path) -> anyhow::Result<()> {
    if work.join(".git").is_dir() {
        return Ok(());
    }
    std::fs::create_dir_all(work)?;
    git(work, &["init", "-q", "-b", "main"])?;
    std::fs::write(work.join("README.md"), "Scratch repo for the harness queue smoke.\n")?;
    git(work, &["add", "README.md"])?;
    git(work, &["-c", "user.name=smoke", "-c", "user.email=smoke@localhost", "commit", "-q", "-m", "init"])
}

fn stop_daemon(socket: &Path) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    runtime.block_on(async {
        let (client, _events) = DaemonClient::connect(socket).await?;
        client
            .request_ok(DaemonCommand::Shutdown { id: None, force: Some(true), if_idle: None, rest: Map::default() })
            .await?;
        println!("ok   daemon stopped");
        anyhow::Ok(())
    })
}

fn main() -> anyhow::Result<()> {
    let usage = "usage: harness_queue_smoke <eukhe> <scratch dir>";
    let mut args = std::env::args_os().skip(1);
    let daemon = PathBuf::from(args.next().context(usage)?);
    let root = PathBuf::from(args.next().context(usage)?);
    let examples = std::env::current_exe()?;
    let fakes = [("LOCAL_STUDIO_CLAUDE_BIN", "fake_claude"), ("LOCAL_STUDIO_CODEX_BIN", "fake_codex")];
    for (variable, name) in fakes {
        let binary = examples.with_file_name(name);
        ensure!(binary.is_file(), "build the {name} example first ({} is missing)", binary.display());
        std::env::set_var(variable, binary);
    }
    let home = root.join("home");
    let agent_dir = root.join("agent");
    let work = root.join("work");
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(&agent_dir)?;
    scratch_repo(&work)?;
    std::env::set_var("HOME", &home);
    std::env::set_var("CLAUDE_CONFIG_DIR", home.join(".claude"));
    std::env::set_var("CODEX_HOME", home.join(".codex"));
    std::env::set_var("LOCAL_STUDIO_DATA_DIR", root.join("data"));
    std::env::set_var("EUKHE_CODING_AGENT_DIR", &agent_dir);
    let socket = root.join("d.sock");
    let config = StudioConfig {
        socket_path: Some(socket.to_string_lossy().into_owned()),
        session_dir: Some(agent_dir.join("sessions").to_string_lossy().into_owned()),
        data_dir: None,
        controller_main: None,
        launch: Some(DaemonLaunch { program: daemon.to_string_lossy().into_owned() }),
        take_over_serve: false,
    };
    let signal = Arc::new(Signal::default());
    let studio = Studio::new(config, Arc::clone(&signal) as Arc<dyn Waker>)?;
    let mut run = Run { studio: Arc::clone(&studio), signal, watch: Watch::default(), sent: 0 };
    let outcome = (|| {
        run.setup("daemon connected", |watch| matches!(watch.connection, Some(Connection::Ready)))?;
        studio.add_project(work.to_string_lossy().into_owned());
        run.setup("project added", |watch| watch.projects.len() == 1)?;
        scenarios(&mut run)
    })();
    studio.shutdown();
    let stopped = stop_daemon(&socket);
    outcome?;
    stopped?;
    if !run.watch.misses.is_empty() {
        bail!("{} checks failed:\n- {}", run.watch.misses.len(), run.watch.misses.join("\n- "));
    }
    println!("PASS harness_queue_smoke");
    Ok(())
}
