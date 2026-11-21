use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, RwLock};
use std::time::{Duration, Instant};

use local_studio_core::{
    FileContent, ImageAttachment, Item, PromptDraft, Studio, Subscription, TranscriptOp, Update, Waker,
};
use serde_json::{json, Value};
use tokio::sync::{mpsc, Notify};

use crate::devices::{hex, now_ms, random, unhex};
use crate::noise::BLOB_ID;
use crate::ops::{self, OpError};

pub const PROTOCOL: u64 = 2;
pub const MIN_PROTOCOL: u64 = 2;
const WINDOW: usize = 200;
const PAGE_LIMIT: usize = 500;
const REMEMBERED: usize = 1024;
const MAX_BACKLOG: usize = 64;
const MAX_UPLOADS: usize = 16;
const MAX_MIME: usize = 255;
const MAX_ACK: usize = 64;
const MAX_ID: usize = 128;
const MAX_OUTSTANDING: usize = 64;
const MAX_COMMANDS: usize = 512;
const MAX_CONTROL: usize = 64;
const MAX_OUTCOME: usize = 16 << 10;
const MAX_MESSAGE: usize = 1024;
const MAX_ACCEPTED: usize = 16_384;
const SEND_LOG: Duration = Duration::from_secs(60);
const MAX_SEND_LOG: usize = 256;
const BUFFERED: usize = 2000;
const BUFFERED_BYTES: usize = 64 << 20;
const MAX_WATCH: usize = 64;
const MAX_BLOB: usize = 25 << 20;
const MAX_PENDING: usize = 100 << 20;
const MAX_DOWNLOADS: usize = 64 << 20;
const NOTIFY_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_QUEUED_ITEMS: usize = 2 * BUFFERED;
const MAX_QUEUED_BYTES: usize = 256 << 20;
const FRAME_BUDGET: usize = crate::noise::MAX_JSON - 4096;
const REPLACED: u16 = 4409;
const SNAPSHOT: [&str; 5] = ["connection", "projects", "threads", "models", "daemon_default"];

fn kept() -> Duration {
    static KEPT: OnceLock<Duration> = OnceLock::new();
    *KEPT.get_or_init(|| {
        std::env::var("LOCAL_STUDIO_SERVE_KEPT_SECS")
            .ok()
            .and_then(|secs| secs.parse::<u64>().ok())
            .filter(|secs| *secs > 0)
            .map_or(Duration::from_secs(600), Duration::from_secs)
    })
}

pub struct Blob {
    pub id: [u8; BLOB_ID],
    pub data: Vec<u8>,
}

pub enum Out {
    Frame(String),
    Update { seq: u64, text: Arc<str>, blob: Option<Arc<Blob>> },
    Close(u16, String),
}

impl Out {
    fn blob(&self) -> usize {
        match self {
            Self::Update { blob: Some(blob), .. } => blob.data.len(),
            _ => 0,
        }
    }

    fn size(&self) -> usize {
        match self {
            Self::Frame(text) => text.len(),
            Self::Update { text, blob, .. } => text.len() + blob.as_ref().map_or(0, |blob| blob.data.len()),
            Self::Close(..) => 0,
        }
    }
}

pub type ConnId = u64;

#[derive(Default)]
struct Queued {
    items: AtomicUsize,
    bytes: AtomicUsize,
    blobs: AtomicUsize,
}

#[derive(Clone)]
pub struct Tx {
    out: mpsc::UnboundedSender<Out>,
    queued: Arc<Queued>,
    cut: Arc<Notify>,
    stop: Arc<Notify>,
}

pub struct Rx {
    out: mpsc::UnboundedReceiver<Out>,
    queued: Arc<Queued>,
}

pub fn outbound() -> (Tx, Rx, Arc<Notify>, Arc<Notify>) {
    let (out, rx) = mpsc::unbounded_channel();
    let queued = Arc::new(Queued::default());
    let cut = Arc::new(Notify::new());
    let stop = Arc::new(Notify::new());
    (
        Tx { out, queued: Arc::clone(&queued), cut: Arc::clone(&cut), stop: Arc::clone(&stop) },
        Rx { out: rx, queued },
        cut,
        stop,
    )
}

impl Tx {
    pub fn send(&self, out: Out) -> bool {
        let (size, blob) = (out.size(), out.blob());
        let items = self.queued.items.fetch_add(1, Ordering::SeqCst);
        let bytes = self.queued.bytes.fetch_add(size, Ordering::SeqCst);
        self.queued.blobs.fetch_add(blob, Ordering::SeqCst);
        let full = items >= MAX_QUEUED_ITEMS || (items > 0 && bytes + size > MAX_QUEUED_BYTES);
        if full || self.out.send(out).is_err() {
            self.queued.items.fetch_sub(1, Ordering::SeqCst);
            self.queued.bytes.fetch_sub(size, Ordering::SeqCst);
            self.queued.blobs.fetch_sub(blob, Ordering::SeqCst);
            if full {
                self.cut.notify_one();
            }
            return false;
        }
        true
    }
}

impl Rx {
    pub async fn recv(&mut self) -> Option<Out> {
        let out = self.out.recv().await?;
        self.queued.items.fetch_sub(1, Ordering::SeqCst);
        self.queued.bytes.fetch_sub(out.size(), Ordering::SeqCst);
        self.queued.blobs.fetch_sub(out.blob(), Ordering::SeqCst);
        Some(out)
    }
}

struct Entry {
    seq: u64,
    at: Instant,
    text: Arc<str>,
    blob: Option<Arc<Blob>>,
}

fn weight(text: &str, blob: Option<&Blob>) -> usize {
    text.len() + blob.map_or(0, |blob| blob.data.len())
}

struct Upload {
    mime: String,
    size: usize,
    data: Vec<u8>,
    ack: Option<Value>,
    at: Instant,
}

struct Session {
    key: String,
    conn: Option<(ConnId, Tx)>,
    seq: u64,
    buffer: VecDeque<Entry>,
    buffered: usize,
    watching: HashSet<String>,
    left_at: Option<Instant>,
    uploads: HashMap<[u8; BLOB_ID], Upload>,
    outstanding: VecDeque<(String, Instant)>,
}

impl Session {
    fn new(key: String) -> Self {
        Self {
            key,
            conn: None,
            seq: 0,
            buffer: VecDeque::new(),
            buffered: 0,
            watching: HashSet::new(),
            left_at: None,
            uploads: HashMap::new(),
            outstanding: VecDeque::new(),
        }
    }

    fn push(&mut self, text: Arc<str>, blob: Option<Arc<Blob>>) {
        self.seq += 1;
        let now = Instant::now();
        self.buffer.push_back(Entry { seq: self.seq, at: now, text: Arc::clone(&text), blob: blob.clone() });
        self.buffered += weight(&text, blob.as_deref());
        while self.buffer.len() > BUFFERED
            || self.buffered > BUFFERED_BYTES
            || self.buffer.front().is_some_and(|entry| now.duration_since(entry.at) > kept())
        {
            let Some(entry) = self.buffer.pop_front() else { break };
            self.buffered -= weight(&entry.text, entry.blob.as_deref());
        }
        let gone = self.conn.as_ref().is_some_and(|(_, tx)| !tx.send(Out::Update { seq: self.seq, text, blob }));
        if gone {
            self.conn = None;
            self.left_at = Some(now);
        }
    }

    fn pending(&self) -> usize {
        self.uploads.values().map(|upload| upload.size).sum()
    }

    fn downloads(&self) -> usize {
        self.conn.as_ref().map_or(0, |(_, tx)| tx.queued.blobs.load(Ordering::SeqCst))
    }

    fn drop_partial(&mut self) {
        self.uploads.retain(|_, upload| upload.ack.is_none());
    }

    fn close(&mut self, code: u16, reason: &str) {
        if let Some((_, tx)) = self.conn.take() {
            tx.send(Out::Close(code, reason.to_string()));
            tx.stop.notify_one();
        }
    }
}

#[derive(Default)]
struct Mirror {
    items: Option<Vec<Item>>,
    info: Option<Arc<str>>,
    approvals: Option<Arc<str>>,
    queue: Option<Arc<str>>,
    subagents: Option<Arc<str>>,
    setup: Option<Arc<str>>,
}

struct Remembered<T> {
    order: VecDeque<String>,
    values: HashMap<String, T>,
}

impl<T> Default for Remembered<T> {
    fn default() -> Self {
        Self { order: VecDeque::new(), values: HashMap::new() }
    }
}

impl<T> Remembered<T> {
    fn insert(&mut self, key: String, value: T) {
        if self.values.insert(key.clone(), value).is_none() {
            self.order.push_back(key);
        }
        while self.order.len() > REMEMBERED {
            if let Some(old) = self.order.pop_front() {
                self.values.remove(&old);
            }
        }
    }
}

struct Tag {
    device: String,
    given: String,
    accepted: Option<String>,
}

struct Ledger {
    key: String,
    ids: HashSet<String>,
    outcomes: Remembered<Arc<str>>,
    tags: VecDeque<String>,
}

impl Ledger {
    fn new(key: String) -> Self {
        Self { key, ids: HashSet::new(), outcomes: Remembered::default(), tags: VecDeque::new() }
    }

    fn full(&self) -> bool {
        self.ids.len() >= MAX_ACCEPTED
    }

    fn outcome(&mut self, id: &str, text: &Arc<str>) {
        if self.ids.contains(id) && !self.outcomes.values.contains_key(id) {
            self.outcomes.insert(id.to_string(), Arc::clone(text));
        }
    }
}

#[derive(Default)]
struct State {
    next: ConnId,
    sessions: HashMap<String, Session>,
    watchers: HashMap<String, HashSet<String>>,
    mirrors: HashMap<String, Mirror>,
    snapshot: [Option<Arc<str>>; SNAPSHOT.len()],
    tags: HashMap<String, Tag>,
    tagged: u64,
    ledgers: HashMap<String, Ledger>,
    git: HashMap<String, HashSet<String>>,
    sends: HashMap<String, (String, Instant)>,
    setups: HashMap<String, Arc<str>>,
    drafts: HashMap<String, String>,
}

impl State {
    fn deliver(&mut self, device: &str, text: Arc<str>, blob: Option<Arc<Blob>>) {
        if let Some(session) = self.sessions.get_mut(device) {
            session.push(text, blob);
        }
    }

    fn send_watchers(&mut self, thread: &str, text: &Arc<str>) {
        if let Some((request, at)) = self.sends.remove(thread) {
            let kind = serde_json::from_str::<Value>(text)
                .ok()
                .and_then(|value| value.get("type").and_then(Value::as_str).map(str::to_string))
                .unwrap_or_default();
            eprintln!(
                "local-studio-serve: send {request} first update ({kind}) out at {} ms, {} ms after accept",
                now_ms(),
                at.elapsed().as_millis()
            );
        }
        let devices: Vec<String> = self.watchers.get(thread).into_iter().flatten().cloned().collect();
        for device in devices {
            self.deliver(&device, Arc::clone(text), None);
        }
    }

    fn broadcast(&mut self, text: &Arc<str>) {
        for session in self.sessions.values_mut() {
            session.push(Arc::clone(text), None);
        }
    }

    fn replay(&mut self, device: &str, thread: &str) {
        let Some(mirror) = self.mirrors.get(thread) else { return };
        let mut texts: Vec<Arc<str>> = mirror.info.iter().cloned().collect();
        if let Some(items) = &mirror.items {
            texts.push(windowed_reset(thread, items));
        }
        texts.extend(
            [&mirror.approvals, &mirror.queue, &mirror.subagents, &mirror.setup].into_iter().flatten().cloned(),
        );
        for text in texts {
            self.deliver(device, text, None);
        }
    }

    fn tagged(&self, tag: &str) -> Option<(String, String)> {
        self.tags.get(tag).map(|tag| (tag.device.clone(), tag.given.clone()))
    }

    fn finish(&mut self, tag: &str) {
        let Some(device) = self.tags.get(tag).map(|tag| tag.device.clone()) else { return };
        let Some(session) = self.sessions.get_mut(&device) else { return };
        let before = session.outstanding.len();
        session.outstanding.retain(|(known, _)| known != tag);
        if session.outstanding.len() < before {
            self.tags.remove(tag);
        }
    }

    fn routed(&self, value: &Value) -> Option<(String, Value)> {
        let field = ["request", "draft"].into_iter().find(|field| value.get(*field).is_some_and(Value::is_string))?;
        let (device, given) = self.tagged(value.get(field).and_then(Value::as_str)?)?;
        let mut value = value.clone();
        value[field] = Value::String(given);
        Some((device, value))
    }

    fn remember_outcome(&mut self, tag: &str, routed: &Value, text: &Arc<str>) {
        let Some(Tag { device, accepted: Some(key), .. }) = self.tags.get(tag) else { return };
        let key = key.clone();
        let kept = if text.len() <= MAX_OUTCOME {
            Some(Arc::clone(text))
        } else {
            let mut short = routed.clone();
            if let Some(message) = short.get("message").and_then(Value::as_str) {
                let mut end = MAX_MESSAGE.min(message.len());
                while !message.is_char_boundary(end) {
                    end -= 1;
                }
                short["message"] = Value::String(format!("{} (cut at {MAX_MESSAGE} bytes)", &message[..end]));
            }
            Some(text_of(&short)).filter(|short| short.len() <= MAX_OUTCOME)
        };
        if let (Some(ledger), Some(kept)) = (self.ledgers.get_mut(device.as_str()), kept) {
            ledger.outcome(&key, &kept);
        }
    }

    fn over_downloads(&mut self, owner: &str, request: &str, size: usize) -> bool {
        let pending = self.sessions.get(owner).map_or(0, Session::downloads);
        if pending + size <= MAX_DOWNLOADS {
            return false;
        }
        let failed = json!({
            "type": "request_failed",
            "request": request,
            "code": "downloads_pending",
            "message": format!(
                "{pending} bytes of earlier downloads are still on their way to this device; a {size} byte answer would pass the {MAX_DOWNLOADS} byte budget. Ask again once they arrive"
            ),
        });
        self.deliver(owner, text_of(&failed), None);
        true
    }

    fn local_setup(&mut self, thread: &str, done: bool, value: &Value) {
        let text = text_of(value);
        if done && value.get("error").is_none_or(Value::is_null) {
            self.setups.remove(thread);
        } else {
            self.setups.insert(thread.to_string(), Arc::clone(&text));
        }
        if let Some(mirror) = self.mirrors.get_mut(thread) {
            mirror.setup = Some(Arc::clone(&text));
        }
        self.send_watchers(thread, &text);
    }

    fn started(&mut self, tag: &str, thread: &str) {
        let Some(text) = self.setups.remove(tag) else { return };
        let Ok(mut value) = serde_json::from_str::<Value>(&text) else { return };
        value["thread"] = Value::String(thread.to_string());
        let text = text_of(&value);
        self.drafts.insert(tag.to_string(), thread.to_string());
        self.setups.insert(thread.to_string(), Arc::clone(&text));
        if let Some(mirror) = self.mirrors.get_mut(thread) {
            mirror.setup = Some(Arc::clone(&text));
        }
        self.send_watchers(thread, &text);
    }

    fn live(&mut self, device: &str, conn: ConnId) -> Result<&mut Session, OpError> {
        self.sessions
            .get_mut(device)
            .filter(|session| session.conn.as_ref().is_some_and(|(id, _)| *id == conn))
            .ok_or_else(|| OpError::new("closed", "this connection was replaced or closed"))
    }

    fn release(&mut self, studio: &Studio, device: &str, thread: &str) {
        let Some(set) = self.watchers.get_mut(thread) else { return };
        set.remove(device);
        if set.is_empty() {
            self.watchers.remove(thread);
            self.mirrors.remove(thread);
            studio.unwatch_thread(thread.to_string());
        }
    }

    fn watch(&mut self, studio: &Studio, device: &str, thread: &str) {
        let Some(session) = self.sessions.get_mut(device) else { return };
        if !session.watching.insert(thread.to_string()) {
            return;
        }
        let set = self.watchers.entry(thread.to_string()).or_default();
        let first = set.is_empty();
        set.insert(device.to_string());
        if first {
            let setup = self.setups.get(thread).cloned();
            self.mirrors.insert(thread.to_string(), Mirror { setup, ..Mirror::default() });
            studio.watch_thread(thread.to_string());
        } else {
            self.replay(device, thread);
        }
    }

    fn drop_ledger(&mut self, device: &str) {
        if let Some(ledger) = self.ledgers.remove(device) {
            for tag in ledger.tags {
                self.tags.remove(&tag);
            }
        }
    }

    fn drop_session(&mut self, studio: &Studio, device: &str, code: u16, reason: &str) {
        let Some(mut session) = self.sessions.remove(device) else { return };
        session.close(code, reason);
        for (tag, _) in &session.outstanding {
            self.tags.remove(tag);
        }
        for thread in session.watching {
            self.release(studio, device, &thread);
        }
        for set in self.git.values_mut() {
            set.remove(device);
        }
    }
}

fn accepted_key(op: &str, given: &str) -> String {
    format!("{op}:{given}")
}

fn text_of(value: &Value) -> Arc<str> {
    Arc::from(value.to_string())
}

fn windowed_reset(thread: &str, items: &[Item]) -> Arc<str> {
    let mut shown = items.len().min(WINDOW);
    loop {
        let earlier = items.len() - shown;
        let mut update = json!({
            "type": "transcript",
            "thread": thread,
            "op": { "type": "reset", "items": &items[earlier..], "earlier": earlier },
        });
        if shown <= 1 {
            fit(&mut update, "/op/items/0");
            return text_of(&update);
        }
        let text = update.to_string();
        if text.len() <= FRAME_BUDGET {
            return Arc::from(text);
        }
        shown /= 2;
    }
}

fn escaped(byte: u8) -> usize {
    match byte {
        b'"' | b'\\' | b'\n' | b'\r' | b'\t' | 0x08 | 0x0c => 2,
        0..=0x1f => 6,
        _ => 1,
    }
}

fn quoted(text: &str) -> usize {
    2 + text.bytes().map(escaped).sum::<usize>()
}

fn size(value: &Value) -> usize {
    match value {
        Value::Null | Value::Bool(true) => 4,
        Value::Bool(false) => 5,
        Value::Number(number) => number.to_string().len(),
        Value::String(text) => quoted(text),
        Value::Array(items) => 2 + items.iter().map(size).sum::<usize>() + items.len().saturating_sub(1),
        Value::Object(fields) => {
            2 + fields.iter().map(|(name, field)| quoted(name) + 1 + size(field)).sum::<usize>()
                + fields.len().saturating_sub(1)
        }
    }
}

fn shrink(value: &mut Value, budget: usize) {
    let mut total = size(value);
    if total <= budget {
        return;
    }
    match value {
        Value::String(text) => {
            let room = budget.saturating_sub(2);
            let (mut used, mut end) = (0, 0);
            for (at, byte) in text.bytes().enumerate() {
                used += escaped(byte);
                if used > room {
                    break;
                }
                end = at + 1;
            }
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
        }
        Value::Array(items) => {
            let mut used = 2;
            let mut keep = 0;
            for item in items.iter_mut() {
                let need = size(item) + 1;
                if used + need <= budget {
                    used += need;
                    keep += 1;
                    continue;
                }
                let room = budget.saturating_sub(used + 1);
                shrink(item, room);
                if size(item) <= room {
                    keep += 1;
                }
                break;
            }
            items.truncate(keep);
        }
        Value::Object(fields) => {
            let mut order: Vec<(usize, String)> =
                fields.iter().map(|(name, field)| (size(field), name.clone())).collect();
            order.sort_unstable_by_key(|entry| std::cmp::Reverse(entry.0));
            for (own, name) in order {
                let Some(field) = fields.get_mut(&name) else { continue };
                shrink(field, own.saturating_sub(total - budget));
                total = total - own + size(field);
                if total <= budget {
                    break;
                }
            }
        }
        _ => {}
    }
}

fn fit(value: &mut Value, pointer: &str) -> bool {
    let total = size(value);
    if total <= FRAME_BUDGET {
        return false;
    }
    let Some(part) = value.pointer_mut(pointer) else { return false };
    let own = size(part);
    shrink(part, FRAME_BUDGET.saturating_sub(total - own + 32));
    let cut = own - size(part);
    if let Some(fields) = part.as_object_mut() {
        fields.insert("cut".to_string(), json!(cut));
    }
    true
}

pub struct Signal(Arc<Notify>);

impl Waker for Signal {
    fn wake(&self) {
        self.0.notify_one();
    }
}

pub fn signal() -> (Arc<Signal>, Arc<Notify>) {
    let notify = Arc::new(Notify::new());
    (Arc::new(Signal(Arc::clone(&notify))), notify)
}

pub struct Resume {
    pub seq: Option<u64>,
    pub epoch: Option<String>,
    pub watch: Vec<String>,
}

pub enum Feed {
    Tick,
    Subscription(Subscription),
}

impl Feed {
    fn drain(&self, studio: &Studio) -> Vec<Update> {
        match self {
            Self::Tick => studio.tick(),
            Self::Subscription(subscription) => subscription.tick(),
        }
    }
}

pub struct Hub {
    studio: Arc<Studio>,
    state: Mutex<State>,
    machine: RwLock<Value>,
    pub epoch: String,
    hooks: RwLock<Vec<String>>,
    http: reqwest::Client,
}

impl Hub {
    pub fn new(studio: Arc<Studio>, machine: Value, hooks: Vec<String>) -> anyhow::Result<Arc<Self>> {
        let http = reqwest::Client::builder().timeout(NOTIFY_TIMEOUT).build()?;
        let epoch = hex(&random::<8>()?);
        Ok(Arc::new(Self {
            studio,
            state: Mutex::new(State::default()),
            machine: RwLock::new(machine),
            epoch,
            hooks: RwLock::new(hooks),
            http,
        }))
    }

    fn machine(&self) -> Value {
        self.machine.read().unwrap_or_else(PoisonError::into_inner).clone()
    }

    pub fn set_machine(&self, name: &str, urls: &[String]) {
        let mut machine = self.machine.write().unwrap_or_else(PoisonError::into_inner);
        machine["name"] = json!(name);
        machine["urls"] = json!(urls);
    }

    pub fn set_hooks(&self, hooks: Vec<String>) {
        *self.hooks.write().unwrap_or_else(PoisonError::into_inner) = hooks;
    }

    pub fn close_all(&self, code: u16, reason: &str) {
        let mut state = self.state();
        for session in state.sessions.values_mut() {
            session.close(code, reason);
            session.left_at = Some(Instant::now());
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub async fn pump(self: Arc<Self>, notify: Arc<Notify>, feed: Feed) {
        self.studio.load_models();
        loop {
            notify.notified().await;
            for update in feed.drain(&self.studio) {
                self.on_update(update);
            }
        }
    }

    pub async fn reap(self: Arc<Self>) {
        let mut every = tokio::time::interval(Duration::from_secs(15).min(kept() / 2));
        loop {
            every.tick().await;
            let mut state = self.state();
            let now = Instant::now();
            let gone: Vec<String> = state
                .sessions
                .iter()
                .filter(|(_, session)| session.left_at.is_some_and(|left| now.duration_since(left) > kept()))
                .map(|(device, _)| device.clone())
                .collect();
            for device in gone {
                eprintln!("local-studio-serve: released device {device}'s session after {} s away", kept().as_secs());
                state.drop_session(&self.studio, &device, 1000, "gone");
            }
            for session in state.sessions.values_mut() {
                session.uploads.retain(|_, upload| now.duration_since(upload.at) <= kept());
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    fn on_update(&self, update: Update) {
        let value = match serde_json::to_value(&update) {
            Ok(value) => value,
            Err(error) => {
                eprintln!("local-studio-serve: could not encode an update: {error}");
                return;
            }
        };
        let kind = value.get("type").and_then(Value::as_str).unwrap_or_default().to_string();
        let mut state = self.state();
        match update {
            Update::Transcript { thread, op } => {
                let Some(mirror) = state.mirrors.get_mut(&thread) else { return };
                match op {
                    TranscriptOp::Reset { items } => {
                        let text = windowed_reset(&thread, &items);
                        let first = mirror.items.is_none();
                        mirror.items = Some(items);
                        let setup = mirror.setup.clone().filter(|_| first);
                        state.send_watchers(&thread, &text);
                        if let Some(setup) = setup {
                            state.send_watchers(&thread, &setup);
                        }
                        return;
                    }
                    TranscriptOp::Insert { index, item } => {
                        if let Some(items) = mirror.items.as_mut() {
                            let index = usize::try_from(index).unwrap_or(usize::MAX);
                            if index <= items.len() {
                                items.insert(index, item);
                            } else {
                                mirror.items = None;
                            }
                        }
                    }
                    TranscriptOp::Update { index, item } => {
                        if let Some(items) = mirror.items.as_mut() {
                            match usize::try_from(index).ok().and_then(|index| items.get_mut(index)) {
                                Some(slot) => *slot = item,
                                None => mirror.items = None,
                            }
                        }
                    }
                }
                let mut value = value;
                fit(&mut value, "/op/item");
                state.send_watchers(&thread, &text_of(&value));
            }
            Update::Thread { thread, .. }
            | Update::Approvals { thread, .. }
            | Update::Queue { thread, .. }
            | Update::Subagents { thread, .. } => {
                let text = text_of(&value);
                let Some(mirror) = state.mirrors.get_mut(&thread) else { return };
                let slot = match kind.as_str() {
                    "thread" => &mut mirror.info,
                    "approvals" => &mut mirror.approvals,
                    "queue" => &mut mirror.queue,
                    _ => &mut mirror.subagents,
                };
                *slot = Some(Arc::clone(&text));
                state.send_watchers(&thread, &text);
            }
            Update::WorktreeSetup { thread: draft, done, .. } if !state.drafts.contains_key(&draft) => {
                let Some((owner, given)) = state.tagged(&draft) else {
                    state.local_setup(&draft, done, &value);
                    return;
                };
                if done {
                    state.setups.remove(&draft);
                } else {
                    state.setups.insert(draft, text_of(&value));
                }
                let mut routed = value;
                routed["thread"] = Value::String(given);
                state.deliver(&owner, text_of(&routed), None);
            }
            Update::WorktreeSetup { thread: draft, done, .. } => {
                let Some(thread) = state.drafts.get(&draft).cloned() else { return };
                if done {
                    state.drafts.remove(&draft);
                }
                let mut value = value;
                value["thread"] = Value::String(thread.clone());
                state.local_setup(&thread, done, &value);
            }
            Update::Notification { thread, kind: notification, .. } => {
                state.broadcast(&text_of(&value));
                drop(state);
                self.notify_hooks(&thread, &value.get("kind").cloned().unwrap_or(json!(format!("{notification:?}"))));
            }
            Update::GitStatus { project, .. } => {
                let text = text_of(&value);
                for device in state.git.remove(&project).unwrap_or_default() {
                    state.deliver(&device, Arc::clone(&text), None);
                }
            }
            Update::MessageImage { request: tag, mime_type, data } => {
                let Some((owner, request)) = state.tagged(&tag) else { return };
                state.finish(&tag);
                if state.over_downloads(&owner, &request, data.len()) {
                    return;
                }
                let Ok(id) = random::<BLOB_ID>() else { return };
                let header = json!({ "type": "blob", "request": request, "blob": hex(&id), "size": data.len(), "mime": mime_type });
                state.deliver(&owner, text_of(&header), Some(Arc::new(Blob { id, data })));
            }
            Update::FileContent { request: tag, path, content } => {
                let Some((owner, request)) = state.tagged(&tag) else { return };
                state.finish(&tag);
                let (mime, data, language) = match content {
                    FileContent::Text { text, language } => {
                        ("text/plain; charset=utf-8".to_string(), text.into_bytes(), language)
                    }
                    FileContent::Image { mime_type, data } => (mime_type, data, None),
                    FileContent::Binary { size } | FileContent::TooLarge { size } => {
                        let what = if kind_of_file(&value) == "binary" { "is binary" } else { "is too large to send" };
                        let failed = json!({ "type": "request_failed", "request": request, "message": format!("{path} {what} ({size} bytes)") });
                        state.deliver(&owner, text_of(&failed), None);
                        return;
                    }
                };
                if state.over_downloads(&owner, &request, data.len()) {
                    return;
                }
                let Ok(id) = random::<BLOB_ID>() else { return };
                let header = json!({ "type": "blob", "request": request, "blob": hex(&id), "size": data.len(), "mime": mime, "path": path, "language": language });
                state.deliver(&owner, text_of(&header), Some(Arc::new(Blob { id, data })));
            }
            _ => match kind.as_str() {
                "connection" | "projects" | "threads" | "models" | "daemon_default" => {
                    let text = text_of(&value);
                    if let Some(index) = SNAPSHOT.iter().position(|name| *name == kind) {
                        state.snapshot[index] = Some(Arc::clone(&text));
                    }
                    state.broadcast(&text);
                }
                "thread_started" | "request_done" | "request_failed" => match state.routed(&value) {
                    Some((owner, routed)) => {
                        let text = text_of(&routed);
                        let tag =
                            ["request", "draft"].into_iter().find_map(|field| value.get(field).and_then(Value::as_str));
                        if let (Some(tag), Some(thread)) =
                            (value.get("draft").and_then(Value::as_str), value.get("thread").and_then(Value::as_str))
                        {
                            state.started(tag, thread);
                        }
                        if let Some(tag) = tag {
                            state.remember_outcome(tag, &routed, &text);
                        }
                        state.deliver(&owner, text, None);
                        if let Some(tag) = tag {
                            state.finish(tag);
                        }
                    }
                    None if kind == "thread_started" => state.broadcast(&text_of(&value)),
                    None => eprintln!("local-studio-serve: dropped a {kind} for a request no phone owns"),
                },
                "notice" => state.broadcast(&text_of(&value)),
                "thread_search"
                | "archived_threads"
                | "commands"
                | "skills"
                | "diff"
                | "pull_requests"
                | "pull_request_detail" => {
                    if let Some((owner, mut routed)) = state.routed(&value) {
                        fit(&mut routed, "");
                        state.deliver(&owner, text_of(&routed), None);
                        if let Some(tag) = value.get("request").and_then(Value::as_str) {
                            state.finish(tag);
                        }
                    }
                }
                _ => {}
            },
        }
    }

    fn notify_hooks(&self, thread: &str, kind: &Value) {
        let machine = self.machine();
        let body = json!({
            "machine": { "id": machine.get("id"), "name": machine.get("name") },
            "kind": kind,
            "thread": thread,
        });
        let hooks = self.hooks.read().unwrap_or_else(PoisonError::into_inner).clone();
        for (index, url) in hooks.iter().enumerate() {
            let request = self.http.post(url).json(&body);
            tokio::spawn(async move {
                match request.send().await {
                    Ok(response) if response.status().is_success() => {}
                    Ok(response) => eprintln!("local-studio-serve: notify hook {index} answered {}", response.status()),
                    Err(error) => eprintln!("local-studio-serve: notify hook {index} failed: {}", error.without_url()),
                }
            });
        }
    }

    pub fn attach(
        &self,
        device: &str,
        name: &str,
        key: &str,
        protocol: u64,
        tx: Tx,
        resume: &Resume,
    ) -> Option<ConnId> {
        let mut state = self.state();
        let asked: HashSet<String> = resume.watch.iter().take(MAX_WATCH).cloned().collect();
        let changes = state.sessions.get(device).map_or(asked.len(), |session| {
            if session.key == key {
                session.watching.symmetric_difference(&asked).count()
            } else {
                session.watching.len() + asked.len()
            }
        });
        if changes > 0 && self.studio.queued() + changes > MAX_COMMANDS {
            return None;
        }
        state.next += 1;
        let conn = state.next;
        let fresh = state.sessions.get(device).is_none_or(|session| session.key != key);
        if fresh {
            state.drop_session(&self.studio, device, REPLACED, "replaced");
            state.sessions.insert(device.to_string(), Session::new(key.to_string()));
        }
        if state.ledgers.get(device).is_none_or(|ledger| ledger.key != key) {
            state.drop_ledger(device);
            state.ledgers.insert(device.to_string(), Ledger::new(key.to_string()));
        }
        let snapshot: Vec<Value> =
            state.snapshot.iter().flatten().filter_map(|text| serde_json::from_str(text).ok()).collect();
        let Some(session) = state.sessions.get_mut(device) else { return Some(conn) };
        session.close(REPLACED, "replaced by a newer socket");
        session.drop_partial();
        let delta = !fresh
            && resume.epoch.as_deref() == Some(self.epoch.as_str())
            && resume.seq.is_some_and(|seq| {
                seq <= session.seq
                    && (seq == session.seq || session.buffer.front().is_some_and(|entry| entry.seq <= seq + 1))
            });
        let base = if delta { resume.seq.unwrap_or_default() } else { session.seq };
        let hello = json!({
            "op": "hello",
            "protocol": protocol,
            "min_protocol": MIN_PROTOCOL,
            "epoch": self.epoch,
            "machine": self.machine(),
            "device": { "id": device, "name": name },
            "resume": if delta { "delta" } else { "snapshot" },
            "seq": base,
            "snapshot": if delta { Vec::new() } else { snapshot },
        });
        tx.send(Out::Frame(hello.to_string()));
        if delta {
            for entry in session.buffer.iter().filter(|entry| entry.seq > base) {
                tx.send(Out::Update { seq: entry.seq, text: Arc::clone(&entry.text), blob: entry.blob.clone() });
            }
        }
        session.conn = Some((conn, tx));
        session.left_at = None;
        let wanted: Vec<String> = resume.watch.iter().take(MAX_WATCH).cloned().collect();
        let held: Vec<String> = session.watching.iter().cloned().collect();
        for thread in held.iter().filter(|thread| !wanted.contains(thread)) {
            if let Some(session) = state.sessions.get_mut(device) {
                session.watching.remove(thread);
            }
            state.release(&self.studio, device, thread);
        }
        for thread in &wanted {
            if held.contains(thread) {
                if !delta {
                    state.replay(device, thread);
                }
            } else {
                state.watch(&self.studio, device, thread);
            }
        }
        Some(conn)
    }

    pub fn detach(&self, device: &str, conn: ConnId) {
        let mut state = self.state();
        if let Some(session) = state.sessions.get_mut(device) {
            if session.conn.as_ref().is_some_and(|(id, _)| *id == conn) {
                session.conn = None;
                session.left_at = Some(Instant::now());
                session.drop_partial();
            }
        }
    }

    pub fn retain(&self, known: &HashMap<String, String>) {
        let mut state = self.state();
        let gone: Vec<String> = state
            .sessions
            .iter()
            .filter(|(device, session)| known.get(*device) != Some(&session.key))
            .map(|(device, _)| device.clone())
            .collect();
        for device in gone {
            eprintln!("local-studio-serve: device {device} was revoked; closing its socket");
            state.drop_session(&self.studio, &device, 4401, "revoked");
        }
        let revoked: Vec<String> = state
            .ledgers
            .iter()
            .filter(|(device, ledger)| known.get(*device) != Some(&ledger.key))
            .map(|(device, _)| device.clone())
            .collect();
        for device in revoked {
            state.drop_ledger(&device);
        }
    }

    pub fn watch(&self, device: &str, conn: ConnId, thread: &str) -> Result<(), OpError> {
        let mut state = self.state();
        let session = state.live(device, conn)?;
        if session.watching.contains(thread) {
            return Ok(());
        }
        if session.watching.len() >= MAX_WATCH {
            return Err(OpError::new("too_many", format!("watch at most {MAX_WATCH} threads at once")));
        }
        if self.studio.queued() >= MAX_COMMANDS {
            return Err(OpError::new(
                "busy",
                "the machine is still working through earlier requests; try again shortly",
            ));
        }
        state.watch(&self.studio, device, thread);
        Ok(())
    }

    pub fn unwatch(&self, device: &str, conn: ConnId, thread: &str) -> Result<(), OpError> {
        let mut state = self.state();
        if state.live(device, conn)?.watching.remove(thread) {
            state.release(&self.studio, device, thread);
        }
        Ok(())
    }

    pub fn load_earlier(&self, device: &str, conn: ConnId, args: &Value) -> Result<Value, OpError> {
        let thread: String = ops::arg(args, "thread")?;
        let before: Option<u64> = ops::arg(args, "before")?;
        let limit: Option<u64> = ops::arg(args, "limit")?;
        let mut state = self.state();
        if !state.live(device, conn)?.watching.contains(&thread) {
            return Err(OpError::new("not_watched", format!("watch thread {thread} before loading its transcript")));
        }
        let items =
            state.mirrors.get(&thread).and_then(|mirror| mirror.items.as_ref()).ok_or_else(|| {
                OpError::new("not_loaded", format!("thread {thread}'s transcript has not loaded yet"))
            })?;
        let before = before.and_then(|before| usize::try_from(before).ok()).unwrap_or(items.len()).min(items.len());
        let limit = limit.and_then(|limit| usize::try_from(limit).ok()).unwrap_or(WINDOW).min(PAGE_LIMIT);
        let mut shown = limit.min(before);
        loop {
            let start = before - shown;
            let mut page = json!({ "thread": thread, "start": start, "items": &items[start..before] });
            if shown <= 1 {
                fit(&mut page, "/items/0");
                return Ok(page);
            }
            if page.to_string().len() <= FRAME_BUDGET {
                return Ok(page);
            }
            shown /= 2;
        }
    }

    pub fn request(&self, device: &str, conn: ConnId, op: &str, args: &Value) -> Result<bool, OpError> {
        let mut state = self.state();
        state.live(device, conn)?;
        if args.get("thread").and_then(Value::as_str).is_some_and(|thread| thread.len() > MAX_ID) {
            return Err(OpError::new("bad_args", format!("args.thread may be at most {MAX_ID} bytes")));
        }
        let prompt_op = ops::idempotent(op);
        let field = ops::correlation(op)
            .filter(|_| prompt_op || ops::answered(op))
            .filter(|field| args.get(*field).is_some_and(Value::is_string));
        let given = field.and_then(|field| args.get(field)).and_then(Value::as_str).map(str::to_string);
        if let (Some(field), Some(given)) = (field, &given) {
            if given.len() > MAX_ACK {
                return Err(OpError::new("bad_args", format!("args.{field} may be at most {MAX_ACK} bytes")));
            }
        }
        let asked = match args.get("request") {
            Some(Value::String(asked)) if op == "start_thread" => Some(asked.clone()),
            Some(_) if op == "start_thread" => {
                return Err(OpError::new("bad_args", "args.request must be a string"));
            }
            _ => None,
        };
        if asked.as_ref().is_some_and(|asked| asked.len() > MAX_ACK) {
            return Err(OpError::new("bad_args", format!("args.request may be at most {MAX_ACK} bytes")));
        }
        let key = match asked {
            Some(asked) => Some(accepted_key("start_thread#request", &asked)),
            None => given.as_ref().filter(|_| prompt_op).map(|given| accepted_key(op, given)),
        };
        if let Some(key) = &key {
            let ledger = state.ledgers.get_mut(device);
            if let Some(ledger) = ledger.as_ref().filter(|ledger| ledger.ids.contains(key)) {
                let outcome = ledger.outcomes.values.get(key).cloned();
                if let Some(outcome) = outcome {
                    state.live(device, conn)?.push(outcome, None);
                }
                return Ok(true);
            }
            if ledger.is_some_and(|ledger| ledger.full()) {
                return Err(OpError::new(
                    "too_many",
                    format!(
                        "this device has sent {MAX_ACCEPTED} prompts since serve started; restart serve to send more"
                    ),
                ));
            }
        }
        self.admit(&mut state, device, conn, op, field.is_some() && !prompt_op)?;
        let mut args = args.clone();
        let tag = match (field, &given) {
            (Some(field), Some(given)) => {
                state.tagged += 1;
                let tag = format!("m{}", state.tagged);
                args[field] = Value::String(tag.clone());
                Some((tag, given.clone()))
            }
            _ => None,
        };
        let session = state.live(device, conn)?;
        let mut prompt = |value: &Value| resolve_prompt(session, value);
        ops::dispatch(&self.studio, op, &args, &mut prompt)?;
        if let Some((tag, given)) = tag {
            let accepted = key.clone().filter(|_| prompt_op);
            state.tags.insert(tag.clone(), Tag { device: device.to_string(), given, accepted });
            let mut evicted = Vec::new();
            if prompt_op {
                if let Some(ledger) = state.ledgers.get_mut(device) {
                    ledger.tags.push_back(tag);
                    while ledger.tags.len() > REMEMBERED {
                        evicted.extend(ledger.tags.pop_front());
                    }
                }
            } else {
                state.live(device, conn)?.outstanding.push_back((tag, Instant::now()));
            }
            for old in evicted {
                state.tags.remove(&old);
            }
        }
        if op == "git_status" {
            if let Some(project) = args.get("project").and_then(Value::as_str) {
                state.git.entry(project.to_string()).or_default().insert(device.to_string());
            }
        }
        if let Some(key) = key {
            if let Some(ledger) = state.ledgers.get_mut(device) {
                ledger.ids.insert(key);
            }
        }
        if op == "send" {
            if let (Some(request), Some(thread)) = (given.as_deref(), args.get("thread").and_then(Value::as_str)) {
                eprintln!("local-studio-serve: send {request} accepted for thread {thread} at {} ms", now_ms());
                state.sends.retain(|_, (_, at)| at.elapsed() < SEND_LOG);
                if state.sends.len() < MAX_SEND_LOG {
                    state.sends.insert(thread.to_string(), (request.to_string(), Instant::now()));
                }
            }
        }
        Ok(false)
    }

    fn admit(&self, state: &mut State, device: &str, conn: ConnId, op: &str, answered: bool) -> Result<(), OpError> {
        let prompt_op = ops::idempotent(op);
        let room = if matches!(op, "abort" | "answer_approval") { MAX_COMMANDS + MAX_CONTROL } else { MAX_COMMANDS };
        if self.studio.queued() >= room {
            return Err(OpError::new(
                "busy",
                "the machine is still working through earlier requests; try again shortly",
            ));
        }
        if prompt_op {
            let (prompts, bytes) = self.studio.backlog();
            if prompts >= MAX_BACKLOG || bytes >= MAX_PENDING {
                return Err(OpError::new("busy", "the machine is still taking earlier prompts; try again shortly"));
            }
        }
        if answered {
            let now = Instant::now();
            let session = state.live(device, conn)?;
            let mut expired = Vec::new();
            while session.outstanding.front().is_some_and(|(_, at)| now.duration_since(*at) > kept()) {
                expired.extend(session.outstanding.pop_front().map(|(tag, _)| tag));
            }
            let waiting = session.outstanding.len();
            for tag in expired {
                state.tags.remove(&tag);
            }
            if waiting >= MAX_OUTSTANDING {
                return Err(OpError::new(
                    "busy",
                    format!(
                        "{MAX_OUTSTANDING} requests from this device are still waiting for answers; try again shortly"
                    ),
                ));
            }
        }
        Ok(())
    }

    pub fn blob_put(&self, device: &str, conn: ConnId, id: Value, args: &Value) -> Result<(), OpError> {
        let blob: String = ops::arg(args, "blob")?;
        let size: usize = ops::arg(args, "size")?;
        let mime: String = ops::arg(args, "mime")?;
        let blob = unhex(&blob)
            .and_then(|bytes| <[u8; BLOB_ID]>::try_from(bytes).ok())
            .ok_or_else(|| OpError::new("bad_args", "args.blob must be 16 bytes in hex"))?;
        if size > MAX_BLOB {
            return Err(OpError::new("too_large", format!("a blob may be at most {MAX_BLOB} bytes")));
        }
        if mime.len() > MAX_MIME {
            return Err(OpError::new("bad_args", format!("args.mime may be at most {MAX_MIME} bytes")));
        }
        if id.to_string().len() > MAX_ACK {
            return Err(OpError::new("bad_args", format!("the request id may be at most {MAX_ACK} bytes")));
        }
        let (_, queued) = self.studio.backlog();
        let mut state = self.state();
        let session = state.live(device, conn)?;
        if session.uploads.contains_key(&blob) {
            return Err(OpError::new("bad_args", "this blob id is already in use"));
        }
        if session.uploads.len() >= MAX_UPLOADS {
            return Err(OpError::new("too_many", format!("at most {MAX_UPLOADS} uploads may be pending per device")));
        }
        if session.pending() + queued + size > MAX_PENDING {
            return Err(OpError::new("too_large", format!("at most {MAX_PENDING} bytes may be pending per device")));
        }
        let upload =
            Upload { mime, size, data: Vec::with_capacity(size.min(1 << 20)), ack: Some(id), at: Instant::now() };
        session.uploads.insert(blob, upload);
        Ok(())
    }

    pub fn blob_chunk(
        &self,
        device: &str,
        conn: ConnId,
        blob: [u8; BLOB_ID],
        last: bool,
        data: &[u8],
    ) -> Result<Option<Value>, String> {
        let mut state = self.state();
        let session = state.live(device, conn).map_err(|error| error.message)?;
        let Some(upload) = session.uploads.get_mut(&blob) else { return Ok(None) };
        let Some(ack) = upload.ack.clone() else {
            return Err(format!("a chunk after blob {} was complete", hex(&blob)));
        };
        if upload.data.len() + data.len() > upload.size {
            return Err(format!("blob {} is longer than its declared {} bytes", hex(&blob), upload.size));
        }
        upload.data.extend_from_slice(data);
        if !last {
            return Ok(None);
        }
        if upload.data.len() != upload.size {
            let short = upload.data.len();
            session.uploads.remove(&blob);
            let error = OpError::new("bad_blob", format!("the blob ended at {short} bytes, not the declared size"));
            return Ok(Some(json!({ "re": ack, "error": error.to_value() })));
        }
        upload.ack = None;
        upload.at = Instant::now();
        Ok(Some(json!({ "re": ack, "ok": true })))
    }
}

fn kind_of_file(value: &Value) -> &str {
    value.pointer("/content/type").and_then(Value::as_str).unwrap_or_default()
}

fn resolve_prompt(session: &mut Session, value: &Value) -> Result<PromptDraft, OpError> {
    let text: String = ops::arg(value, "text")?;
    let files: Option<Vec<String>> = ops::arg(value, "files")?;
    let ids: Option<Vec<String>> = ops::arg(value, "images")?;
    let mut images = Vec::new();
    for id in ids.unwrap_or_default() {
        let blob = unhex(&id)
            .and_then(|bytes| <[u8; BLOB_ID]>::try_from(bytes).ok())
            .ok_or_else(|| OpError::new("bad_args", format!("image {id:?} is not a blob id")))?;
        match session.uploads.get(&blob) {
            Some(upload) if upload.ack.is_none() => {}
            Some(_) => return Err(OpError::new("bad_blob", format!("blob {id} has not finished uploading"))),
            None => return Err(OpError::new("bad_blob", format!("no uploaded blob {id}"))),
        }
        if let Some(upload) = session.uploads.remove(&blob) {
            images.push(ImageAttachment { mime_type: upload.mime, data: upload.data });
        }
    }
    Ok(PromptDraft { text, images, files: files.unwrap_or_default() })
}
