use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{bail, ensure, Context};
use eukhe_client::daemon_client::DaemonClient;
use eukhe_types::daemon::DaemonCommand;
use local_studio_core::{
    AccessMode, Approval, ApprovalAnswer, Block, Connection, DaemonLaunch, Delivery, Harness, Item, ItemKind,
    ModelOption, NoticeLevel, PromptDraft, Studio, StudioConfig, ThreadRow, ThreadSettings, ThreadStatus, TranscriptOp,
    Update, UsagePeriod, UsageReport, Waker, WorktreeChoice,
};
use serde_json::Map;

const STEP_TIMEOUT: Duration = Duration::from_secs(240);
const CHECK_TIMEOUT: Duration = Duration::from_secs(45);
const SLASH_TIMEOUT: Duration = Duration::from_secs(20);
const WORD: &str = "zeppelin";
const QUEUED: &str = "marmalade";
const PROOF: &str = "echo ls-proof-4711 > proof.txt";
const STOPPED: &str = "echo ls-stop-4711 > stopped.txt";
const TOUCH: &str = "touch touched.txt";
const LONG: &str = "Write a 3000 word short story about a lighthouse keeper on a northern island, in plain prose.";

pub struct Target {
    pub name: &'static str,
    pub harness: Harness,
    pub provider: &'static str,
    pub model: fn(&[ModelOption]) -> Option<String>,
    pub thinking: Option<&'static str>,
}

#[derive(Default)]
pub struct Signal {
    pending: Mutex<bool>,
    ready: Condvar,
}

impl Waker for Signal {
    fn wake(&self) {
        *self.pending.lock().unwrap_or_else(PoisonError::into_inner) = true;
        self.ready.notify_one();
    }
}

impl Signal {
    pub fn wait(&self, timeout: Duration) {
        let pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        let (mut pending, _) = self
            .ready
            .wait_timeout_while(pending, timeout, |pending| !*pending)
            .unwrap_or_else(PoisonError::into_inner);
        *pending = false;
    }
}

#[derive(Default)]
pub struct Watch {
    connection: Option<Connection>,
    projects: Vec<String>,
    threads: Vec<ThreadRow>,
    models: Vec<ModelOption>,
    pub thread: Option<String>,
    busy: bool,
    finished: u32,
    approvals: Vec<Approval>,
    pub denied: Vec<(Approval, Instant)>,
    answers: BTreeMap<u64, String>,
    users: BTreeMap<u64, String>,
    latest: Option<String>,
    streamed: bool,
    failure: Option<String>,
    resets: u32,
    errors: Vec<String>,
    usage: Option<UsageReport>,
    resources: u32,
    misses: Vec<String>,
}

impl Watch {
    fn row(&self) -> Option<&ThreadRow> {
        let thread = self.thread.as_deref()?;
        self.threads.iter().find(|row| row.id == thread)
    }

    fn begin(&mut self) -> u32 {
        self.latest = None;
        self.streamed = false;
        self.failure = None;
        self.finished
    }

    fn miss(&mut self, check: String) {
        println!("MISS {check}");
        self.misses.push(check);
    }

    fn reply(&self, step: &str) -> anyhow::Result<String> {
        if let Some(failure) = &self.failure {
            bail!("{step}: the reply ended with an error: {failure}");
        }
        let reply = self.latest.clone().unwrap_or_default();
        println!("     {step} reply: {}", reply.trim());
        Ok(reply)
    }
}

fn observe(watch: &mut Watch, item: &Item, live: bool) {
    match &item.kind {
        ItemKind::Assistant { blocks, streaming, error, .. } => {
            let text: String = blocks
                .iter()
                .filter_map(|block| match block {
                    Block::Text { text } => Some(text.as_str()),
                    Block::Thinking { .. } => None,
                })
                .collect();
            let said = !text.trim().is_empty();
            if *streaming {
                watch.streamed |= live && said;
                return;
            }
            if let (true, Some(error)) = (live, error) {
                println!("     assistant error: {error}");
                watch.failure = Some(error.clone());
            }
            if said {
                watch.answers.insert(item.id, text.clone());
                if live {
                    watch.latest = Some(text);
                }
            }
        }
        ItemKind::User { text, .. } => {
            watch.users.insert(item.id, text.clone());
        }
        ItemKind::Tool { .. }
        | ItemKind::TurnSummary { .. }
        | ItemKind::Notice { .. }
        | ItemKind::Compaction { .. } => {}
    }
}

fn apply(watch: &mut Watch, update: Update) {
    match update {
        Update::Transcript { thread, op } if watch.thread.as_deref() == Some(thread.as_str()) => match op {
            TranscriptOp::Reset { items } => {
                watch.resets += 1;
                watch.answers.clear();
                watch.users.clear();
                for item in &items {
                    observe(watch, item, false);
                }
            }
            TranscriptOp::Insert { item, .. } | TranscriptOp::Update { item, .. } => observe(watch, &item, true),
        },
        Update::Thread { thread, info } if watch.thread.as_deref() == Some(thread.as_str()) => {
            if watch.busy && !info.busy {
                watch.finished += 1;
            }
            watch.busy = info.busy;
        }
        Update::Approvals { thread, approvals } if watch.thread.as_deref() == Some(thread.as_str()) => {
            watch.approvals = approvals;
        }
        Update::Usage { report, .. } => watch.usage = Some(report),
        Update::Commands { .. } | Update::Skills { .. } => watch.resources += 1,
        Update::Connection { state } => watch.connection = Some(state),
        Update::Projects { projects } => watch.projects = projects.into_iter().map(|project| project.id).collect(),
        Update::Threads { threads } => watch.threads = threads,
        Update::Models { models, .. } => watch.models = models,
        Update::ThreadStarted { thread, .. } => watch.thread = Some(thread),
        Update::Notice { level, text } => {
            println!("     notice {level:?}: {text}");
            if level == NoticeLevel::Error {
                watch.errors.push(text);
            }
        }
        Update::RequestFailed { message, .. } => {
            println!("     request failed: {message}");
            watch.errors.push(message);
        }
        Update::Transcript { .. }
        | Update::Thread { .. }
        | Update::Approvals { .. }
        | Update::ArchivedThreads { .. }
        | Update::ThreadSearch { .. }
        | Update::Queue { .. }
        | Update::ComposerText { .. }
        | Update::DaemonDefault { .. }
        | Update::Subagents { .. }
        | Update::MessageImage { .. }
        | Update::Notification { .. }
        | Update::GitStatus { .. }
        | Update::Branches { .. }
        | Update::Diff { .. }
        | Update::Files { .. }
        | Update::FileContent { .. }
        | Update::SearchResults { .. }
        | Update::PullRequests { .. }
        | Update::PullRequestDetail { .. }
        | Update::Devices { .. }
        | Update::TerminalOpened { .. }
        | Update::TerminalFrame { .. }
        | Update::TerminalText { .. }
        | Update::TerminalExited { .. }
        | Update::Local { .. }
        | Update::LocalDashboard { .. }
        | Update::Providers { .. }
        | Update::ProviderModels { .. }
        | Update::Settings { .. }
        | Update::Keybindings { .. }
        | Update::ProjectScripts { .. }
        | Update::ProjectSettings { .. }
        | Update::WorktreeSetup { .. }
        | Update::Daemon { .. }
        | Update::Kernel { .. }
        | Update::RequestDone { .. } => {}
    }
}

fn pump(
    studio: &Studio,
    signal: &Signal,
    watch: &mut Watch,
    step: &str,
    done: impl Fn(&Watch) -> bool,
) -> anyhow::Result<()> {
    pump_within(studio, signal, watch, step, STEP_TIMEOUT, done)
}

fn pump_within(
    studio: &Studio,
    signal: &Signal,
    watch: &mut Watch,
    step: &str,
    timeout: Duration,
    done: impl Fn(&Watch) -> bool,
) -> anyhow::Result<()> {
    settle(studio, signal, watch, (step, timeout), true, done)
}

fn expecting(
    studio: &Studio,
    signal: &Signal,
    watch: &mut Watch,
    (step, timeout): (&str, Duration),
    done: impl Fn(&Watch) -> bool,
) -> anyhow::Result<()> {
    settle(studio, signal, watch, (step, timeout), false, done)
}

fn settle(
    studio: &Studio,
    signal: &Signal,
    watch: &mut Watch,
    (step, timeout): (&str, Duration),
    deny: bool,
    done: impl Fn(&Watch) -> bool,
) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    let errors = watch.errors.len();
    loop {
        for update in studio.tick() {
            apply(watch, update);
        }
        if deny {
            deny_unexpected(studio, watch, step)?;
        }
        if done(watch) {
            println!("ok   {step}");
            return Ok(());
        }
        if let Some(error) = watch.errors.get(errors) {
            bail!("{step}: {error}{}", pending(watch));
        }
        let Some(left) = deadline.checked_duration_since(Instant::now()) else {
            bail!("timed out waiting for: {step}{}", pending(watch));
        };
        signal.wait(left.min(Duration::from_millis(100)));
    }
}

fn pending(watch: &Watch) -> String {
    if watch.approvals.is_empty() {
        return String::new();
    }
    let cards: Vec<String> = watch.approvals.iter().map(|approval| format!("`{}`", approval.command)).collect();
    format!(" (approval pending: {})", cards.join(", "))
}

fn deny_unexpected(studio: &Studio, watch: &mut Watch, step: &str) -> anyhow::Result<()> {
    let Some(thread) = watch.thread.clone() else { return Ok(()) };
    for approval in watch.approvals.clone() {
        match watch.denied.iter().find(|(known, _)| known.request_id == approval.request_id) {
            Some((_, at)) if at.elapsed() > CHECK_TIMEOUT => bail!(
                "{step}: the approval `{}` is still pending {} s after a deny",
                approval.command,
                CHECK_TIMEOUT.as_secs()
            ),
            Some(_) => {}
            None => {
                println!("     {step}: denied an approval this step does not expect: {}", approval.command);
                studio.answer_approval(thread.clone(), approval.request_id.clone(), ApprovalAnswer::Deny);
                watch.denied.push((approval, Instant::now()));
            }
        }
    }
    Ok(())
}

fn prompt(text: &str) -> PromptDraft {
    PromptDraft { text: text.to_string(), images: Vec::new(), files: Vec::new() }
}

pub fn turn(
    studio: &Studio,
    signal: &Signal,
    watch: &mut Watch,
    thread: &str,
    text: &str,
    step: &str,
) -> anyhow::Result<String> {
    let finished = watch.begin();
    studio.send(format!("send-{finished}"), thread.to_string(), prompt(text), Delivery::Queue);
    pump(studio, signal, watch, &format!("{step} finished"), |watch| watch.finished > finished)?;
    watch.reply(step)
}

fn write_prompt(file: &str) -> String {
    format!(
        "Create a file named {file} in the current directory containing the single line hello. Use your built-in file editing tool (Write for Claude Code, apply_patch for Codex), not a shell command or an MCP tool, and do not ask me first."
    )
}

fn shell_prompt(command: &str) -> String {
    format!("Run exactly this shell command with your shell tool, unchanged, and do not ask me first: {command}")
}

fn approval_turn(
    studio: &Studio,
    signal: &Signal,
    watch: &mut Watch,
    thread: &str,
    (label, text): (&str, &str),
    answer: ApprovalAnswer,
) -> anyhow::Result<Vec<Approval>> {
    let finished = watch.begin();
    studio.send(format!("send-{label}"), thread.to_string(), prompt(text), Delivery::Queue);
    let mut asked = Vec::new();
    loop {
        expecting(studio, signal, watch, (&format!("{label}: approval asked or turn ended"), STEP_TIMEOUT), |watch| {
            !watch.approvals.is_empty() || watch.finished > finished
        })?;
        let Some(approval) = watch.approvals.first().cloned() else { break };
        println!("     approval: {} {:?}", approval.command, approval.effects);
        studio.answer_approval(thread.to_string(), approval.request_id.clone(), answer);
        let request = approval.request_id.clone();
        asked.push(approval);
        expecting(
            studio,
            signal,
            watch,
            (&format!("{label}: approval {} settled", asked.len()), STEP_TIMEOUT),
            |watch| watch.approvals.iter().all(|known| known.request_id != request),
        )?;
    }
    Ok(asked)
}

fn card_shows_command(
    studio: &Studio,
    signal: &Signal,
    watch: &mut Watch,
    thread: &str,
    work: &Path,
) -> anyhow::Result<()> {
    let asked = approval_turn(studio, signal, watch, thread, ("proof", &shell_prompt(PROOF)), ApprovalAnswer::Deny)?;
    ensure!(!asked.is_empty(), "Supervised did not ask before running a shell command");
    ensure!(!work.join("proof.txt").exists(), "proof.txt was written after a deny");
    let probe = PROOF.split(" > ").next().unwrap_or(PROOF);
    match asked.iter().find(|approval| approval.command.contains("ls-proof-4711")) {
        Some(card) if card.command.contains(probe) => {
            println!("ok   the shell approval card shows the command: {}", card.command.replace('\n', " | "));
        }
        Some(card) => watch.miss(format!("the shell approval card hides the command `{probe}`: {:?}", card.command)),
        None => watch.miss(format!(
            "no shell approval card shows the command `{probe}`: {:?}",
            asked.iter().map(|approval| approval.command.clone()).collect::<Vec<_>>()
        )),
    }
    Ok(())
}

fn codex_supervised_holds(
    studio: &Studio,
    signal: &Signal,
    watch: &mut Watch,
    (target, thread): (&Target, &str),
    work: &Path,
) -> anyhow::Result<()> {
    if target.harness != Harness::Codex {
        return Ok(());
    }
    untrusted_command_asks(studio, signal, watch, thread, work)?;
    outside_write_held(studio, signal, watch, thread, work)
}

fn untrusted_command_asks(
    studio: &Studio,
    signal: &Signal,
    watch: &mut Watch,
    thread: &str,
    work: &Path,
) -> anyhow::Result<()> {
    let asked = approval_turn(studio, signal, watch, thread, ("touch", &shell_prompt(TOUCH)), ApprovalAnswer::Deny)?;
    ensure!(!asked.is_empty(), "Supervised did not ask before running `{TOUCH}`");
    ensure!(!work.join("touched.txt").exists(), "touched.txt was written after a deny");
    match asked.iter().find(|approval| approval.command.contains("touched.txt")) {
        Some(card) => {
            println!("ok   `{TOUCH}` asked first and deny left no file: {}", card.command.replace('\n', " | "));
        }
        None => watch.miss(format!(
            "no approval card for `{TOUCH}` names touched.txt: {:?}",
            asked.iter().map(|approval| approval.command.clone()).collect::<Vec<_>>()
        )),
    }
    Ok(())
}

fn outside_write_held(
    studio: &Studio,
    signal: &Signal,
    watch: &mut Watch,
    thread: &str,
    work: &Path,
) -> anyhow::Result<()> {
    let outside = work.parent().context("scratch root")?.join("outside");
    std::fs::create_dir_all(&outside)?;
    let target = outside.join("outside.txt");
    let text = format!(
        "Create a file at the absolute path {} containing the single line hello. Use your built-in file editing tool (Write for Claude Code, apply_patch for Codex), not a shell command or an MCP tool, and do not ask me first.",
        target.display()
    );
    let asked = approval_turn(studio, signal, watch, thread, ("outside.txt", &text), ApprovalAnswer::Deny)?;
    ensure!(
        !target.exists(),
        "a write outside the thread's cwd happened {}",
        if asked.is_empty() { "silently, with no approval asked" } else { "after a deny" }
    );
    if asked.is_empty() {
        println!("ok   a write outside the cwd was refused without asking: {}", watch.reply("outside.txt")?.trim());
    } else {
        println!("ok   a write outside the cwd asked first and deny left no file ({} approvals denied)", asked.len());
    }
    Ok(())
}

fn stop_settles_approval(studio: &Studio, signal: &Signal, watch: &mut Watch, thread: &str) -> anyhow::Result<()> {
    let finished = watch.begin();
    studio.send("send-stop-approval".to_string(), thread.to_string(), prompt(&shell_prompt(STOPPED)), Delivery::Queue);
    expecting(studio, signal, watch, ("stop-approval: approval asked", STEP_TIMEOUT), |watch| {
        !watch.approvals.is_empty() || watch.finished > finished
    })?;
    ensure!(!watch.approvals.is_empty(), "Supervised did not ask before running {STOPPED}");
    println!("     approval left unanswered: {}", watch.approvals[0].command);
    studio.abort(thread.to_string());
    let stopped = Instant::now();
    let settled = expecting(studio, signal, watch, ("Stop settled the unanswered approval", CHECK_TIMEOUT), |watch| {
        watch.approvals.is_empty() && !watch.busy && watch.row().is_some_and(|row| row.status != ThreadStatus::Working)
    });
    if let Err(error) = settled {
        watch.miss(format!("{error} (approvals {}, busy {})", watch.approvals.len(), watch.busy));
        for approval in watch.approvals.clone() {
            studio.answer_approval(thread.to_string(), approval.request_id, ApprovalAnswer::Deny);
        }
        expecting(studio, signal, watch, ("stale approvals cleared after the miss", STEP_TIMEOUT), |watch| {
            watch.approvals.is_empty()
        })?;
        if watch.busy {
            studio.abort(thread.to_string());
            pump(studio, signal, watch, "the thread left Working after the miss", |watch| !watch.busy)?;
        }
    } else {
        println!("     settled in {} ms", stopped.elapsed().as_millis());
    }
    Ok(())
}

fn slash_command_settles(studio: &Studio, signal: &Signal, watch: &mut Watch, thread: &str) -> anyhow::Result<()> {
    let finished = watch.begin();
    studio.send("send-/clear".to_string(), thread.to_string(), prompt("/clear"), Delivery::Queue);
    let sent = Instant::now();
    let left = pump_within(studio, signal, watch, "/clear left Working", SLASH_TIMEOUT, |watch| {
        watch.finished > finished && watch.row().is_some_and(|row| row.status != ThreadStatus::Working)
    });
    if let Err(error) = left {
        watch.miss(format!("{error} (busy {}, row {:?})", watch.busy, watch.row().map(|row| row.status)));
        if watch.busy {
            studio.abort(thread.to_string());
            pump(studio, signal, watch, "/clear: Stop took the thread out of Working", |watch| !watch.busy)?;
        }
    } else {
        println!("     /clear settled in {} ms", sent.elapsed().as_millis());
    }
    Ok(())
}

fn queued_follow_up(studio: &Studio, signal: &Signal, watch: &mut Watch, thread: &str) -> anyhow::Result<()> {
    watch.begin();
    let first = "Count from one to forty in words, one number per line, and nothing else.";
    studio.send("send-count".to_string(), thread.to_string(), prompt(first), Delivery::Queue);
    pump(studio, signal, watch, "count turn is working", |watch| watch.busy)?;
    let follow = format!("Reply with only the word {QUEUED}.");
    studio.send("send-queued".to_string(), thread.to_string(), prompt(&follow), Delivery::Queue);
    let ran = pump(studio, signal, watch, "queued follow-up ran after the turn", |watch| {
        !watch.busy && watch.latest.as_deref().is_some_and(|text| text.to_ascii_lowercase().contains(QUEUED))
    });
    match ran {
        Ok(()) => {
            let users = watch.users.values().filter(|text| text.contains(QUEUED)).count();
            let counted = watch.answers.values().any(|text| text.to_ascii_lowercase().contains("forty"));
            if users != 1 {
                watch.miss(format!("the queued follow-up shows {users} times in the transcript"));
            } else if !counted {
                watch.miss(format!(
                    "the running turn never finished counting to forty before {QUEUED} (latest {:?})",
                    watch.latest
                ));
            } else {
                println!("ok   the queued follow-up ran once after the running turn finished");
            }
        }
        Err(error) => {
            watch.miss(format!("{error} (busy {}, latest {:?})", watch.busy, watch.latest));
            if watch.busy {
                studio.abort(thread.to_string());
                pump(studio, signal, watch, "the thread left Working after the miss", |watch| !watch.busy)?;
            }
        }
    }
    Ok(())
}

fn efforts_running() -> Vec<String> {
    let parent = std::process::id().to_string();
    let Ok(listing) = Command::new("ps").args(["-A", "-o", "ppid=,command="]).output() else { return Vec::new() };
    String::from_utf8_lossy(&listing.stdout)
        .lines()
        .filter_map(|line| {
            let (ppid, command) = line.trim().split_once(' ')?;
            (ppid == parent && command.contains("stream-json")).then(|| {
                command
                    .split_once("--effort ")
                    .and_then(|(_, rest)| rest.split_whitespace().next())
                    .unwrap_or("none")
                    .to_string()
            })
        })
        .collect()
}

fn effort_restarts(
    studio: &Studio,
    signal: &Signal,
    watch: &mut Watch,
    thread: &str,
    (old, new): (&str, &str),
) -> anyhow::Result<()> {
    let deadline = Instant::now() + CHECK_TIMEOUT;
    loop {
        for update in studio.tick() {
            apply(watch, update);
        }
        deny_unexpected(studio, watch, "effort restart")?;
        if !efforts_running().iter().any(|effort| effort == old) {
            break;
        }
        if Instant::now() >= deadline {
            watch.miss(format!(
                "the Claude Code process still runs at --effort {old} {} s after the turn that changed it (running: {:?})",
                CHECK_TIMEOUT.as_secs(),
                efforts_running()
            ));
            return Ok(());
        }
        signal.wait(Duration::from_millis(500));
    }
    let mut running = efforts_running();
    if running.is_empty() {
        println!("     no Claude Code process runs after the restart; the next send must open one at --effort {new}");
        turn(studio, signal, watch, thread, "Reply with only the word ready.", "effort check turn")?;
        running = efforts_running();
    }
    if running.iter().any(|effort| effort == new) && !running.iter().any(|effort| effort == old) {
        println!("ok   the Claude Code process runs at the new --effort {new} (running: {running:?})");
    } else {
        watch.miss(format!(
            "after the effort change to {new} no Claude Code process runs at --effort {new} (running: {running:?})"
        ));
    }
    Ok(())
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
    std::fs::write(work.join("README.md"), "Scratch repo for the harness smoke.\n")?;
    git(work, &["add", "README.md"])?;
    git(work, &["-c", "user.name=smoke", "-c", "user.email=smoke@localhost", "commit", "-q", "-m", "init"])
}

fn long_turn_aborts(
    studio: &Studio,
    signal: &Signal,
    watch: &mut Watch,
    (target, thread): (&Target, &str),
) -> anyhow::Result<()> {
    let finished = watch.begin();
    studio.send("send-long".to_string(), thread.to_string(), prompt(LONG), Delivery::Queue);
    pump(studio, signal, watch, "long turn is working", |watch| watch.busy)?;
    pump(studio, signal, watch, "long turn is streaming text", |watch| watch.streamed || watch.finished > finished)?;
    ensure!(
        watch.finished == finished,
        "the long turn ended before the abort: {:?} (approvals denied: {:?})",
        watch.failure,
        watch.denied.iter().map(|(approval, _)| approval.command.clone()).collect::<Vec<_>>()
    );
    let old = target.thinking.unwrap_or("none");
    let new = if old == "medium" { "low" } else { "medium" };
    studio.set_thinking_level(thread.to_string(), new.to_string());
    println!("     thinking level {old} -> {new} during the running turn");
    studio.abort(thread.to_string());
    let aborted = Instant::now();
    pump(studio, signal, watch, "abort ended the turn", |watch| watch.finished > finished && !watch.busy)?;
    pump(studio, signal, watch, "abort took the row out of Working", |watch| {
        watch.row().is_some_and(|row| row.status != ThreadStatus::Working)
    })?;
    let status = watch.row().map(|row| row.status);
    println!("ok   aborted turn left Working in {} ms (row {status:?})", aborted.elapsed().as_millis());
    if target.harness == Harness::ClaudeCode {
        effort_restarts(studio, signal, watch, thread, (old, new))?;
    }
    Ok(())
}

fn clear_outputs(root: &Path, work: &Path) -> anyhow::Result<()> {
    let files = ["denied.txt", "approved.txt", "touched.txt", "proof.txt", "stopped.txt"].map(|name| work.join(name));
    for path in files.iter().chain([root.join("outside").join("outside.txt")].iter()) {
        match std::fs::remove_file(path) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                return Err(error).with_context(|| format!("remove {}", path.display()));
            }
            _ => {}
        }
    }
    Ok(())
}

fn open(config: &StudioConfig) -> anyhow::Result<(Arc<Studio>, Arc<Signal>, Watch)> {
    let signal = Arc::new(Signal::default());
    let studio = Studio::new(config.clone(), Arc::clone(&signal) as Arc<dyn Waker>)?;
    let mut watch = Watch::default();
    pump(&studio, &signal, &mut watch, "daemon connected", |watch| {
        matches!(watch.connection, Some(Connection::Ready))
    })?;
    Ok((studio, signal, watch))
}

fn first_run(config: &StudioConfig, target: &Target, work: &Path) -> anyhow::Result<(String, Vec<String>)> {
    let (studio, signal, mut watch) = open(config)?;
    let outcome = (|| {
        let watch = &mut watch;
        pump(&studio, &signal, watch, "models listed", |watch| {
            watch.models.iter().any(|model| model.provider == target.provider)
        })?;
        let models: Vec<ModelOption> =
            watch.models.iter().filter(|model| model.provider == target.provider).cloned().collect();
        let model = (target.model)(&models).context("no model to run")?;
        println!(
            "     {} models: {}",
            target.name,
            models.iter().map(|model| model.id.as_str()).collect::<Vec<_>>().join(", ")
        );
        println!("     running {}/{model}", target.provider);
        studio.add_project(work.to_string_lossy().into_owned());
        pump(&studio, &signal, watch, "project added", |watch| watch.projects.len() == 1)?;
        let settings = ThreadSettings {
            provider: Some(target.provider.to_string()),
            model: Some(model),
            thinking_level: target.thinking.map(str::to_string),
            access: AccessMode::Supervised,
            worktree: WorktreeChoice::Local,
        };
        let first =
            format!("<div class=\"note\">Remember the word {WORD}. Reply with only that word, nothing else.</div>");
        let finished = watch.begin();
        let project = watch.projects.first().cloned();
        studio.start_thread("draft-harness".to_string(), project, prompt(&first), settings);
        pump(&studio, &signal, watch, "thread started", |watch| watch.thread.is_some())?;
        let thread = watch.thread.clone().context("thread id")?;
        studio.select_thread(Some(thread.clone()));
        pump(&studio, &signal, watch, "turn 1 finished", |watch| watch.finished > finished)?;
        let reply = watch.reply("turn 1")?;
        ensure!(reply.to_ascii_lowercase().contains(WORD), "turn 1 did not answer with {WORD}");
        println!("ok   reply contains the asked word");
        let row = watch.row().context("thread row")?;
        ensure!(row.harness == target.harness, "the row shows harness {:?}", row.harness);
        println!("ok   row lists harness {:?}", row.harness);
        studio.list_commands("harness-commands".to_string(), watch.projects.first().cloned());
        pump(&studio, &signal, watch, "commands listed with the CLI thread focused", |watch| watch.resources == 1)?;
        studio.list_skills("harness-skills".to_string(), watch.projects.first().cloned());
        pump(&studio, &signal, watch, "skills listed with the CLI thread focused", |watch| watch.resources == 2)?;

        let denied = approval_turn(
            &studio,
            &signal,
            watch,
            &thread,
            ("denied.txt", &write_prompt("denied.txt")),
            ApprovalAnswer::Deny,
        )?
        .len();
        ensure!(denied > 0, "Supervised did not ask before writing denied.txt");
        ensure!(!work.join("denied.txt").exists(), "denied.txt was written after a deny");
        println!("ok   deny left denied.txt unwritten ({denied} approvals denied)");
        let approved = approval_turn(
            &studio,
            &signal,
            watch,
            &thread,
            ("approved.txt", &write_prompt("approved.txt")),
            ApprovalAnswer::Approve,
        )?
        .len();
        ensure!(approved > 0, "Supervised did not ask before writing approved.txt");
        ensure!(work.join("approved.txt").is_file(), "approved.txt is missing after an approve");
        println!("ok   approve wrote approved.txt ({approved} approvals)");
        codex_supervised_holds(&studio, &signal, watch, (target, &thread), work)?;

        card_shows_command(&studio, &signal, watch, &thread, work)?;
        stop_settles_approval(&studio, &signal, watch, &thread)?;
        ensure!(!work.join("stopped.txt").exists(), "stopped.txt was written although its approval was never given");
        queued_follow_up(&studio, &signal, watch, &thread)?;
        long_turn_aborts(&studio, &signal, watch, (target, &thread))?;
        Ok(thread)
    })();
    let misses = std::mem::take(&mut watch.misses);
    studio.shutdown();
    outcome.map(|thread| (thread, misses))
}

fn second_run(config: &StudioConfig, target: &Target, thread: &str) -> anyhow::Result<Vec<String>> {
    let (studio, signal, mut watch) = open(config)?;
    let outcome = (|| {
        let watch = &mut watch;
        watch.thread = Some(thread.to_string());
        pump(&studio, &signal, watch, "thread listed after the restart", |watch| watch.row().is_some())?;
        let row = watch.row().context("thread row")?;
        ensure!(row.harness == target.harness, "after the restart the row shows harness {:?}", row.harness);
        println!("ok   listed after restart: \"{}\" harness {:?}", row.title, row.harness);
        studio.select_thread(Some(thread.to_string()));
        pump(&studio, &signal, watch, "earlier turns shown", |watch| watch.users.len() >= 2)?;
        println!("ok   earlier turns shown ({} user messages, {} replies)", watch.users.len(), watch.answers.len());
        if watch.users.values().any(|text| text.starts_with("<div") && text.contains(WORD)) {
            println!("ok   the user message starting with <div survived the restart");
        } else {
            let first = watch.users.values().next().cloned().unwrap_or_default();
            watch.miss(format!(
                "the user message starting with <div is missing after the restart (first user message: {first:?})"
            ));
        }
        let reply = turn(
            &studio,
            &signal,
            watch,
            thread,
            "What word did I ask you to remember in my first message? Reply with only that word.",
            "resumed turn",
        )?;
        ensure!(reply.to_ascii_lowercase().contains(WORD), "the resumed session did not remember {WORD}");
        println!("ok   the session resumed: turn 1 word recalled");
        if target.harness == Harness::ClaudeCode {
            slash_command_settles(&studio, &signal, watch, thread)?;
        }
        studio.load_usage("usage-1".to_string(), UsagePeriod::Day);
        pump(&studio, &signal, watch, "usage report loaded", |watch| watch.usage.is_some())?;
        let report = watch.usage.as_ref().context("usage")?;
        let rows: Vec<_> = report.by_model.iter().filter(|model| model.provider == target.provider).collect();
        for row in &rows {
            println!(
                "     usage {}/{}: {} tokens, {} requests, ${:.4}",
                row.provider, row.model, row.tokens, row.requests, row.cost_usd
            );
        }
        println!(
            "     usage total: {} tokens (in {} cache read {} out {}), ${:.4}, {} sessions",
            report.total_tokens,
            report.input_tokens,
            report.cache_read_tokens,
            report.output_tokens,
            report.cost_usd,
            report.sessions
        );
        ensure!(rows.iter().any(|row| row.tokens > 0), "the usage report has no {} tokens", target.provider);
        ensure!(report.output_tokens > 0, "the usage report counts no output tokens");
        println!("ok   usage report counts real tokens");
        Ok(())
    })();
    let misses = std::mem::take(&mut watch.misses);
    studio.shutdown();
    outcome.map(|()| misses)
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

pub fn run(target: &Target) -> anyhow::Result<()> {
    let usage = format!("usage: {} <eukhe> <scratch dir>", target.name);
    let mut args = std::env::args_os().skip(1);
    let daemon = PathBuf::from(args.next().context(usage.clone())?);
    let root = PathBuf::from(args.next().context(usage)?);
    let agent_dir = root.join("agent");
    let work = root.join("work");
    std::fs::create_dir_all(&agent_dir)?;
    scratch_repo(&work)?;
    clear_outputs(&root, &work)?;
    std::env::set_var("LOCAL_STUDIO_DATA_DIR", root.join("data"));
    std::env::set_var("EUKHE_CODING_AGENT_DIR", &agent_dir);
    let socket = root.join("d.sock");
    let config = StudioConfig {
        socket_path: Some(socket.to_string_lossy().into_owned()),
        session_dir: Some(agent_dir.join("sessions").to_string_lossy().into_owned()),
        data_dir: None,
        controller_main: None,
        take_over_serve: false,
        launch: Some(DaemonLaunch { program: daemon.to_string_lossy().into_owned() }),
    };
    let outcome = first_run(&config, target, &work).and_then(|(thread, mut misses)| {
        println!("ok   first Studio shut down; starting a second one on the same data dir");
        misses.extend(second_run(&config, target, &thread)?);
        Ok(misses)
    });
    let stopped = stop_daemon(&socket);
    let misses = outcome?;
    stopped?;
    if !misses.is_empty() {
        bail!("{} checks failed:\n- {}", misses.len(), misses.join("\n- "));
    }
    println!("PASS {}", target.name);
    Ok(())
}
