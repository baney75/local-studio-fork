# Local Studio Remote: the phone protocol

Status: accepted. The core lead accepted this design on 2026-11-17 on PR #4, including blobs as records inside the Noise channel, for building the phone and `local-studio serve`. This file merges through a tier 3 review that the core lead runs; round 1's fixes are in this text.

## Goal

A phone (first the native iPhone app) sees every thread on every one of the user's machines, reads transcripts live, sends and queues messages, answers approvals and questions, starts threads, and hears when a thread needs the user. It reaches each machine over Tailscale (`tailscale serve`) and over a Cloudflare tunnel when one is set up. One wire for all of it, owned by the core, so the Mac, Linux and Windows apps can later attach to `serve` over it too.

## What the code already gives us

- `Studio` (core/src/studio.rs) and its `Workspace` (core/src/workspace) are command shaped: `send`, `abort`, `answer_approval`, `start_thread`, `load_diff` and the rest.
- `Update` (core/src/model.rs) is the event stream every GUI already renders.
- Eukhe's dashboard (eukhe-dashboard, protocol 3) solved loopback serving behind `tailscale serve`: a `Host` allowlist against DNS rebinding and an `Origin` check against cross-site sockets. This design keeps those and adds end to end encryption, because TLS ends at Cloudflare's edge on the tunnel route.

So: **serve `Studio` over a WebSocket**, encrypted end to end. Requests are `Studio` method names, events are `Update`s, both JSON. No second model.

## Core changes

1. **Watched threads, not one focus.** `Studio::select` keeps one `focused` thread and only that thread streams `Transcript`, `Thread`, `Approvals`, `Questions` and `Queue`. Add `watch_thread(thread)` and `unwatch_thread(thread)` with a reference count per thread. `select_thread` stays for GUI focus (it also marks read) and counts as one watch; `select_thread(new)` releases the watch it held on the previous focus. Every `emit_*` that checks `focused == id` checks `watched.contains(id)` instead. Studio then emits thread-scoped updates for every watched thread, so each desktop GUI (macos/, linux/, the Windows app) filters them by its own focus. The server holds one watch per device per thread (see Session: watches belong to the device, not the socket), routes thread-scoped updates only to devices watching that thread, and sends global updates to all.
2. **One Studio owner per data dir, on all three OSes.** `state.json` is written by atomic rename with no lock, so two Studios on one data dir lose each other's flags. Take an advisory lock on `<data>/studio.lock` with `fd-lock` (flock on macOS and Linux, `LockFileEx` on Windows). Whoever holds it serves remote clients: the desktop app when it runs (it starts the server module in process), or `local-studio serve`, run as a user service where no GUI runs: a launchd agent on macOS, a systemd user unit on Linux, a Task Scheduler logon task on Windows.
3. **`serde` behind a `wire` feature** on `Update` and everything it carries plus the request argument types: `#[serde(tag = "type", rename_all = "snake_case")]` on enums, `camelCase` fields. No binary data in JSON; images and files travel as blobs (below).

## Transport

- **One listener per route.** `local-studio serve [--port 18093] [--tunnel-port 18094] [--allow-host <name>]... [--allow-tunnel-host <name>]...` binds `127.0.0.1` only, on two ports. The server knows a client's route from the listener it arrived on, never from `Host` or any other header. On both: `GET /v1/ws` upgrades to the WebSocket, and `GET /v1/health` answers `{"ok":true,"protocol":N,"min_protocol":M}` without auth.
- **Local listener (18093)**: loopback clients and `tailscale serve`. `tailscale serve --bg --yes --https=443 --set-path=/v1/health http://127.0.0.1:18093/v1/health` and the same for `/v1/ws` (never `/`), `--allow-host <machine>.<tailnet>.ts.net`, and the phone connects to `wss://<machine>.<tailnet>.ts.net/v1/ws`. The client address for rate limits comes from the rightmost `X-Forwarded-For` hop, the one `tailscale serve` appends; earlier hops are ignored, because the client writes them. With `Tailscale-User-Login` present the address is that login plus that hop, so a ban on one login at one address doesn't refuse the same login elsewhere. A request is the exempt plain loopback client only when it carries no forwarding header at all: no `X-Forwarded-For` and no `Tailscale-*` header. That client is held only by the pending socket caps; the server never bans `127.0.0.1` as a whole. A request with a forwarding header but neither a login nor `X-Forwarded-For` is keyed on its peer address and can be banned. This listener ignores `CF-Connecting-IP`. `tailscale funnel` on this listener isn't supported: funneled requests come from the public internet with no `Tailscale-User-Login`, and the tunnel listener is the public route.
- **Tunnel listener (18094)**: only the tunnels lead's hosted tunnel (cloudflared) forwards here, and `--allow-tunnel-host` names its hostname. The client address is `CF-Connecting-IP` (Cloudflare overwrites it, so a client can't set it); a request without it gets 400. This listener ignores `Tailscale-User-Login` and `X-Forwarded-For`, and nothing on it is exempt from bans.
- **Host check.** Each listener checks the `Host` header, never `X-Forwarded-Host`. The local listener allows `127.0.0.1:<port>`, `localhost:<port>` and each `--allow-host` name; the tunnel listener allows each `--allow-tunnel-host` name. Any other `Host` gets 403 on every path, `/v1/health` included.
- **Origin.** Native clients send no `Origin`. A request with an `Origin` must match its `Host` (browsers); a request without one is allowed.
- Binary WebSocket messages only after the opening header; no `permessage-deflate` (the payload is ciphertext). One socket per machine per phone.

## Security: end to end on every route

Every route, Tailscale and loopback included, runs the same encrypted channel inside the WebSocket, so there is one code path and nothing depends on which route a phone took. No reusable credential, token or signature crosses Cloudflare.

**Primitives.** Noise protocol framework, `X25519`, `ChaChaPoly`, `SHA256`: `Noise_XXpsk3_25519_ChaChaPoly_SHA256` for pairing and `Noise_KK_25519_ChaChaPoly_SHA256` for every later connection. The server uses the `snow` crate. The phone uses CryptoKit (`Curve25519.KeyAgreement`, `ChaChaPoly`, `SHA256`, `HMAC`) with the Noise state machine written to the spec, checked against `snow` end to end.

**Keys.**
- Each machine has one static X25519 key, created on first `serve` start, in `<data>/remote/static.key` (0600; on Windows an ACL for the user only).
- The phone makes a new static X25519 key for each machine it pairs with, so machines can't link one phone across them. It keeps that key, the machine's static public key, the machine id and its URLs in the Keychain as `kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly`, never synced. The Secure Enclave holds only P-256 keys, so it isn't used.
- The server stores each paired device in `<data>/remote/devices.json` (0600): device id, name, platform, app version, channel, the device's static public key, first and last seen. Nothing secret.

**Opening header.** The client's first WebSocket message is one text frame, also used as the Noise prologue so it can't be changed in flight:

```json
{"v": 1, "pattern": "KK", "device": "<uuid>", "protocol": 2, "min_protocol": 2}
{"v": 1, "pattern": "XXpsk3", "pair": "<pairing id>", "protocol": 2, "min_protocol": 2}
```

The prologue is the bytes `local-studio-remote/1\n` followed by that frame's exact UTF-8 bytes. If the protocol windows don't overlap, the server answers with a text frame `{"op":"incompatible","protocol":N,"min_protocol":M}` and closes before any handshake, so the phone can say which side to update.

**Pairing (`XXpsk3`).**
- `local-studio pair` (and "Pair a phone" in each desktop app) mints a pairing id (8 random bytes, hex) and a pairing secret (32 random bytes), valid 10 minutes and for one use. Minting a new one cancels any unused one. The desktop shows a QR code of `localstudio://pair?url=wss%3A%2F%2Fstudio-mac.tail1234.ts.net%2Fv1%2Fws&name=studio-mac&id=<pairing id>#<secret, base64url>`. The secret sits in the fragment, so it never reaches a server log. The phone app scans the code in its own Pair sheet. The desktop shows no tappable link, because iOS doesn't make a custom URL scheme exclusive and another installed app could receive the secret.
- The secret is the `psk`. Messages: `-> e`, `<- e, ee, s, es`, `-> s, se, psk`. The third message's payload is the device record: `{"id":"<uuid>","name":"My iPhone","platform":"ios","app":"0.4.0","channel":"nightly"}`. The phone makes a new device id for each pairing, like its static key, so the id can't link one phone across machines either. The server refuses a pairing whose `id` it already has.
- On success the server burns the secret, stores the device with its static key, and the channel continues as a normal session (`hello` below). A wrong secret fails the handshake at the third message and counts as a failed attempt.
- **The phone doesn't trust the pairing until the server proves the psk.** In `XXpsk3` the psk is first mixed into the phone's own third message, so an intermediary that answers message two with its own static key can't be told apart until then. Sending message three proves nothing about the responder. The phone stores the machine's static key and shows the pairing as done only after it decrypts the server's first transport message (its `hello`), whose keys depend on the psk. If that never arrives or fails to decrypt, the phone discards the key and the pairing.

**Every later connection (`KK`).** Both static keys are known. Messages: `-> e, es, ss`, `<- e, ee, se`. Handshake payloads are empty; the first transport message is the phone's `hello`. A `KK` header naming an unknown or revoked device is closed at once with code 4401, before any handshake, and counts as a failure; the phone then shows "Pair again" for that machine. A `KK` socket is authenticated only once the phone's first transport message (`hello`) decrypts, which proves the phone holds the ephemeral key; a recorded `-> e, es, ss` replayed by a relay completes the handshake but can never produce that message. Until `hello` decrypts the socket stays under the pending caps and the 10 s handshake timeout, and a completed handshake with no valid `hello` counts as a failure. Only then, if the device still has another live socket (a half open one after a Wi-Fi to cellular switch), does the server close the older socket with code 4409 before it answers `hello`. Watches and the resume buffer belong to the device, so nothing moves with the socket.

**Records.** After the handshake every WebSocket message is one binary Noise transport message, at most 65,535 bytes. Its plaintext is a record:

| First byte | Meaning | Rest |
| --- | --- | --- |
| `0x01` | JSON frame, final chunk | UTF-8 JSON |
| `0x02` | JSON frame, more chunks follow | UTF-8 JSON |
| `0x03` | blob chunk | 16-byte blob id, 1 byte flags (bit 0: final), data |

A JSON frame is at most 8 MiB once joined; its chunks are sent back to back, never interleaved with another JSON frame. Blob chunks may interleave between JSON frames, so a photo upload never delays a ping. A record that fails to decrypt, or breaks these rules, closes the socket.

**Server limits** (the hosted route has no edge rate limit, since the big assistant sites share egress IPs):
- A socket whose opening header hasn't arrived within 5 s is closed; so is one whose handshake isn't complete within 10 s. Both timeouts count as failures.
- Each listener has its own pending cap: at most 8 sockets at a time on it that haven't sent a valid `hello`, and at most 2 from one client address. Tunnel abuse therefore can't starve the Tailscale and loopback listener, and that listener keeps 2 of its 8 slots for the plain loopback client. An IPv6 client address is bucketed by its /64 for the per-address pending cap and the failure count, so rotating addresses inside one /64 counts as one address.
- Failures are bad headers, failed handshakes, timeouts, sockets closed after the header but before the handshake completed, and `KK` headers naming an unknown or revoked device. 5 from one client address within a minute refuse that address for 10 minutes (close code 4429). Client addresses are per listener, as in Transport; the exempt loopback client is never refused.
- Each failure is logged with the address, the pattern and the reason, never a secret, key or payload.

**Revocation.** Desktop settings list paired devices and revoke one, which deletes its static key and closes its live sockets. "Revoke all devices" deletes every device; Local Studio calls it before bringing a tunnel back after a suspension for a second connector, and the phones pair again.

## Session

The phone's first transport frame:

```json
{"op": "hello", "device": {"name": "My iPhone", "app": "0.4.0", "channel": "nightly"}, "resume": {"seq": 412, "epoch": "<server epoch>", "watch": ["t1"]}}
```

The server answers:

```json
{"op": "hello", "protocol": 2, "epoch": "<id>", "machine": {"id": "…", "name": "studio-mac", "os": "macos", "version": "0.4.0", "channel": "nightly", "urls": ["wss://studio-mac.tail1234.ts.net/v1/ws", "wss://studio-mac.example-tunnel.net/v1/ws"]}, "resume": "delta", "snapshot": []}
```

- `protocol` is the highest version inside both windows; the socket speaks it until it closes.
- `machine.urls` lists every route the machine is reachable on now. The phone replaces its stored list with it, so a hostname that changed after 30 days down is learned on any route that still works.
- **Watches belong to the device, not the socket.** A device's watch set lives as long as its resume buffer. While the device has no socket, every update it would have received if connected keeps going into its buffer: global updates (`threads`, `projects`, `thread_started`, `notification` and the rest) and updates for the threads it watches. So a `delta` resume replays an approval raised on an unwatched thread during the gap just as a live socket would have shown it. The buffer, and with it the watch set, is released 10 minutes after the device's last socket closes, and on every new epoch. `LOCAL_STUDIO_SERVE_KEPT_SECS` shortens that hold for `remote_smoke`; service files leave it unset.
- **Full thread state.** On `watch {thread}`, and for each watched thread on every `snapshot` resume, the server sends that thread's current state in this order: `thread`, a `transcript` `reset`, `approvals`, `questions`, `queue`, `subagents`, `worktree_setup`. An approval or question that was pending before the watch, or raised while the phone was away, always reaches the phone this way.
- **Resume by `seq`.** `seq` counts updates per device and server `epoch` (a new epoch on every `serve` start). The buffer holds the device's last 2,000 updates, 64 MiB of updates and their blobs, or 10 minutes, whichever is least, evicting the oldest first; a resume whose `seq` was evicted gets a `snapshot`. When `resume.epoch` matches and `resume.seq` is still in that buffer, the server answers `"resume": "delta"`: it sets the device's watch set to `resume.watch`, sends the missed updates in order, then the full thread state for each thread in `resume.watch` the device didn't already watch, then live updates. Otherwise it answers `"resume": "snapshot"`: it sets the watch set to `resume.watch`, sends the global snapshot (`connection`, `projects`, `threads`, `models`, `daemon_default`, `theme`), then the full thread state for each watched thread.
- **Reconnect.** After a drop the phone waits 1 s, doubling to 30 s, with full jitter, and tries its URLs in order of last success. It resets the wait after a socket stays up 60 s and reconnects at once when the app comes to the foreground or the network path changes.
- `ping`/`pong` every 20 s from the phone; the server closes a socket silent for 60 s.

## Frames

Client to server (requests run concurrently; `id` is the client's counter):

```json
{"id": 7, "op": "send", "args": {"request": "r7", "thread": "t1", "prompt": {"text": "ship it", "images": ["<blob id>"], "files": []}, "delivery": "queue"}}
```

The server answers each with `{"re": 7, "ok": true}` once accepted, or `{"re": 7, "error": {"code": "…", "message": "…"}}`. Outcomes arrive as updates. A `request` string is unique per device within an epoch, and the server deduplicates by device, epoch and `request`, never server wide, so two phones may both use `r7`. A second `send` from the same device with a `request` already accepted in this epoch isn't run again: the server answers `{"re": id, "ok": true, "duplicate": true}` and sends that request's `request_done` or `request_failed` again once known. The server keeps each accepted `request` and its outcome for the whole epoch, even after the device's resume buffer is released, so a retry after a long absence still gets `duplicate` and the outcome. `start_thread` takes an optional `request` and deduplicates on it the same way, answering a retry with the first start's `thread_started` or `request_failed`; without it the server deduplicates `start_thread` on `draft`, so older phones keep working. `request_done` and `request_failed` go only to the device that sent the request. After an epoch change the phone doesn't retry an unacknowledged `send` on its own: it marks the message unconfirmed with a Resend button, since the snapshot's transcript shows whether it landed.

Server to client: `{"op": "update", "seq": 413, "update": {"type": "transcript", "thread": "t1", "op": {"type": "insert", "index": 18, "item": {}}}}`.

**Transcript window.** `reset` carries at most the last 200 items plus `{"earlier": <count>}`; `load_earlier {"thread", "before": <index>, "limit"}` pages back. Both shrink their window until the frame fits in 8 MiB.

**Cut to fit.** A single transcript item, or a whole answer such as `diff`, that can't fit in one 8 MiB frame is cut: the server shortens its longest strings and drops the tail of its longest lists until it fits, and adds `"cut": <bytes removed>` to that item or answer. The phone shows the item or answer as cut and offers the full version on the desktop. `cut` is an optional field inside protocol 2; a client that doesn't know it shows the shortened content.

**Worktree setup.** `start_thread` with a new worktree reports its setup before the thread exists. Until `thread_started`, each `worktree_setup` goes only to the device that started it, with `thread` set to that device's `draft`. After `thread_started`, later setup updates (scripts that don't block the thread) carry the thread id and go to the thread's watchers.

## Blobs

Images, attachments and file contents never go in JSON. They travel as blob records inside the encrypted channel. They don't use a separate HTTP path, because TLS ends at Cloudflare and an HTTP side channel would skip the end to end encryption.

- **Upload.** The phone picks a random 16-byte id, sends `{"id": 8, "op": "blob_put", "args": {"blob": "<hex id>", "size": 183204, "mime": "image/heic"}}`, then the blob's chunks. The server acknowledges after the final chunk. A `send` names blobs by id. An unreferenced blob is dropped after 10 minutes; at most 25 MiB per blob and 100 MiB pending per device.
- **Download.** `load_message_image` and `read_file` answer with `{"type": "blob", "request": "…", "blob": "<hex id>", "size": …, "mime": "…"}` followed by the chunks. The phone caches `load_message_image` blobs by `(machine, thread, entry, index)`, since a message's images never change. It doesn't cache `read_file` blobs, because the file can change. At most 64 MiB of blob downloads may wait on one device's socket; an answer that would pass that budget is `{"type": "request_failed", "request": "…", "code": "downloads_pending", "message": "…"}` instead, and the phone asks again once its earlier downloads arrive.
- **Interrupted transfers.** Neither side resumes a blob. A phone whose socket closes mid-download discards the partial blob and sends its request again after it reconnects. When a socket closes mid-upload, the server drops that device's unfinished uploads, and the phone uploads the blob again under a new blob id.

## Ops, frozen for protocol 2

Requests the phone may send. Each op outside the Session area is a `Studio` or `Workspace` method under the same name, with its arguments as named fields. Session ops are wire only: `hello`, `ping`, `load_earlier` and `blob_put` are handled by the server, and `watch` and `unwatch` call `watch_thread` and `unwatch_thread` for the device. The server refuses any other op with error code `not_remote`; the phone never needs settings, provider keys, terminals, daemon control or project setup.

| Area | Ops |
| --- | --- |
| Session | `hello`, `ping`, `watch` `{thread}`, `unwatch` `{thread}`, `load_earlier` `{thread, before, limit}`, `blob_put` `{blob, size, mime}` |
| Turns | `send` `{request, thread, prompt, delivery}`, `abort` `{thread}`, `send_queued_now` `{thread, queued}`, `remove_queued` `{thread, queued}`, `answer_approval` `{thread, request, answer}`, `answer_questions` `{thread, request, answers}`, `compact` `{thread}` |
| Threads | `start_thread` `{draft, project, prompt, settings, request?}`, `rename_thread` `{thread, title}`, `regenerate_title` `{thread}`, `fork_thread` `{thread}`, `set_archived` `{thread, archived}`, `set_pinned` `{thread, pinned}`, `set_settled` `{thread, settled}`, `snooze` `{thread, until}`, `mark_unread` `{thread, unread}`, `search_threads` `{request, query, limit}`, `list_archived` `{request}` |
| Agent | `load_models`, `set_model` `{thread, provider, model}`, `set_thinking_level` `{thread, level}`, `set_access_mode` `{thread, mode}`, `list_commands` `{request, project}`, `list_skills` `{request, project}`, `cancel_subagent` `{thread, child}` |
| Read only | `load_message_image` `{request, thread, entry, index}`, `git_status` `{project}`, `load_diff` `{request, project, scope}`, `list_pull_requests` `{request, project}`, `load_pull_request` `{request, project, number}`, `read_file` `{request, project, relative}` |

Updates the phone receives (`type`):

| Scope | Types |
| --- | --- |
| Global | `connection`, `projects`, `threads`, `thread_started`, `models`, `daemon_default`, `notification`, `theme`, `notice` |
| Watched thread | `thread`, `transcript`, `approvals`, `questions`, `questions_settled`, `queue`, `subagents`, `worktree_setup` |
| Answers to a request (sent only to the requesting device) | `request_done`, `request_failed`, `thread_search`, `archived_threads`, `commands`, `skills`, `git_status`, `diff`, `pull_requests`, `pull_request_detail`, `blob` |

Adding an op or update type, or a field a client must understand, raises `protocol`. A server ignores unknown fields in requests, and a client ignores unknown update types and fields.

`local-studio-core`'s `remote_smoke` example starts `serve` on a scratch data dir, pairs a client through `XXpsk3`, reconnects through `KK`, and drives every op in the table once over a real socket against the real model, checking each reply and the updates it causes. It also checks the refusals: a wrong pairing secret, a revoked device (closed with 4401 before any handshake), a `Host` not allowed, an op not in the table, the rate limit on each listener, a timeout counted as a failure, a socket closed after its header counted as a failure, the per address pending cap, that failures on the local listener never refuse a plain loopback client, that a replayed `KK` first message never closes the live socket and counts as a failure, that a full tunnel listener leaves the local listener and its loopback slots usable, that sockets rotating inside one IPv6 /64 share one address, that a Tailscale login is banned per rightmost `X-Forwarded-For` hop, that rotating earlier hops doesn't dodge a ban, that a loopback request with any `Tailscale-*` header isn't exempt, that revoke by a shared name is refused and revoke by a key fingerprint prefix removes one device, that a socket closed mid-upload frees its blob id, that a 64 MiB download budget answers `downloads_pending`, that held prompt text counts toward core's prompt backlog, that a send's outcome outlives its released session, that `start_thread` deduplicates on `request`, that `worktree_setup` reaches the phone under its own `draft`, and that a diff over 8 MiB arrives cut with `cut`. And it checks recovery: an approval pending before `watch` arrives on `watch`; an approval raised during a disconnect arrives after a `delta` resume and after a `snapshot` resume; an approval raised on an unwatched thread during a disconnect shows on Home after a `delta` resume; a follow-up sent with `delivery: queue` while a real `sleep` runs waits in the queue and runs after it; a supervised command writes its file only after the phone approves it; and after a `serve` restart (new epoch) the phone gets a snapshot with that thread's full state.

## Theme

`{"type": "theme", "theme": {…}}` goes in every snapshot and on every change. The payload is the resolved theme for the `studio` surface in the shape of Polycarp's `theme.json` (polycarp spaces/spec/themes.md): `schema`, `mode`, `light` and `dark` with the twelve color roles as `#rrggbb` (background, surface, sunken, foreground, strong, muted, border, accent, selection, urgent, working, done) plus `on_accent`, and `radius_large`, `radius`, `radius_small`, `border_width`, `blur`. The server resolves it; the phone never reads TOML or a file. The phone maps the roles onto its own: background to canvas, surface to cards, sunken to code, foreground to text, muted to secondary text, border to hairlines, accent to the primary tint, urgent, working and done to the status colors.

## Many machines

The phone holds one socket per paired machine and merges what they send by `machine.id`. Each thread row carries its machine; Home shows every machine's threads together; a request goes to the socket of the thread's machine. A machine that is offline keeps its last known rows, marked with how old they are.

## Notifications when the phone is asleep

- `notification` updates (`turn_done`, `needs_approval`, `needs_answer`, `failed`) go to every connected client; the app shows them as local notifications while it has a socket.
- When the app is suspended, iOS keeps no socket. APNs needs a push key from the user's paid Apple Developer account and a relay that holds it. `serve` takes notify hooks (`remote.notify: [{"kind": "webhook", "url": …}]`) and posts a notification's kind and thread id to them, never message text. v1 uses an ntfy style webhook; APNs through the tunnels lead's hosted service is v2 and is the only piece that needs the paid account.

## Question cards

Local Studio has command approvals but no request where the agent asks the user a question with options, and Eukhe's daemon has only `shell_approval` (issue #19 tracks the upstream change). In T3 it is the request the owner answers most: 179 in 30 days against 18 command approvals. Shape, copying the shell approval flow: daemon frame `user_question {active_session_id, request: {request_id, questions: [{header, question, options: [{label, description}], multi_select}]}, agent}`, answer `answer_user_question {request_id, answers: {<question>: <answer>}}`, and `user_question_settled`. In core: `Update::Questions { thread, request, questions }`, `Update::QuestionsSettled { thread, request }` and `Studio::answer_questions(thread, request, answers)`, carried on this wire unchanged.

## Protocol window

Each side states the newest protocol it speaks (`protocol`) and the oldest it still accepts (`min_protocol`), and keeps `min_protocol` at `protocol - 1` once a version has shipped (docs/channels.md: both sides speak their protocol and the one before it). Protocol 1 was the unencrypted draft and never shipped, so the first release is protocol 2 with `min_protocol` 2. When the windows do not overlap, the server sends `incompatible` before the handshake and closes; the phone names the machine, its version and channel, and which side to update. `/v1/health` reports both numbers so the phone can say this before pairing.

## Channels

docs/channels.md is the scheme. The iPhone app installs side by side: `org.local.studio.ios` ("Local Studio"), `org.local.studio.ios.nightly` ("Local Studio Nightly"), `org.local.studio.ios.dev` ("Local Studio Dev"), with URL schemes `localstudio`, `localstudio-nightly` and `localstudio-dev`. Pairing QR codes use the scheme of the channel the desktop app is on, and each app's scanner accepts only its own scheme. The phone carries the desktop app's version and tags, so a phone and a desktop from the same channel always share a protocol. The server reports its version and channel in `hello`, and the phone shows them on the Machines sheet.

## Desktop links: one sidebar for every machine

Status: accepted by the Remote lead on 2026-11-21 for building. No wire change: a desktop app is one more paired device (`platform` `macos`, `linux` or `windows`) speaking protocol 2 exactly as the phone does.

- **Where it lives.** The client side of this protocol (the opening header, `XXpsk3` and `KK` on `snow`, records, chunking, `hello` and resume, reconnect backoff) lives once in `local-studio-core`, and `local-studio-serve` uses the same record and handshake code. The desktop GUIs never speak the wire themselves.
- **Inside `Studio`, not beside it.** A linked machine is a task on the core's runtime that keeps one socket to that machine's serve. `Studio` merges what it sends into the updates every GUI already renders, so a remote thread opens in the same transcript, composer, queue and approval views as a local one, the way T3 Code mounts a remote thread in its one chat view.
- **Ids.** A remote thread's id in `Studio` is `@<machine id>/<thread id>`, and a remote project's is `@<machine id>/<project id>`. `ThreadRow` and `Project` gain `machine: Option<String>` (the machine id, `None` for local rows; omitted on the wire when `None`). `Studio` rewrites ids both ways at the link, so nothing above it parses them; the GUIs read `machine` to show the machine glyph and name on a remote row, as T3 Code does.
- **Routing.** `select_thread` on a remote id holds one `watch` on that machine and releases it on the next selection. Every `Studio` method whose op is in the protocol 2 table routes to the thread's machine with its ids rewritten, and its answer comes back rewritten. A method outside the table on a remote id (terminals, settings, project setup, `edit_from_here`, `delete_thread`) posts a `notice` naming the machine and does nothing. `start_thread` with a remote project starts the thread on that machine.
- **Global state.** `Update::Threads` and `Update::Projects` carry local rows followed by every linked machine's rows. A machine that is offline keeps its last rows, and its `Machine` entry says since when. A new global `Update::Machines { machines }` lists each linked machine: `id`, `name`, `os`, `version`, `channel`, `url` (the route in use), `state` (`connecting`, `connected`, `offline { since }`, `pair_again`, `incompatible { protocol, min_protocol }`, `refused { until }`), `latency_ms` from `ping`.
- **No loops.** A host never serves another machine's rows: the hub drops rows and projects whose `machine` is set and never forwards `machines` or `pairing`. Linking A to B and B to A therefore shows each machine's own threads once.
- **Pairing between desktops.** On the serving machine, "Pair a device" in the app (or `local-studio-serve pair`) mints the pairing as today and shows its QR code with the full link as text, with a Copy button, since both ends are desktops. `Studio::open_pairing(request)` answers `Update::Pairing { request, link, expires_at }`. On the other machine, Settings, Machines, "Add a machine" takes the pasted link (`Studio::pair_machine(link)`), accepts only its own channel's scheme, and shows the machine once the server's `hello` decrypts. `Studio::forget_machine(machine)` drops the link and its credential; revoking is still the serving machine's job.
- **Credentials.** `<data>/remote/links.json` (0600 in a 0700 folder) holds, per machine: machine id, name, this device's id and X25519 static private key for that machine, the machine's static public key, and its URLs with last success times. Moving it to the OS key store goes with the tunnel client's key store slice (#37).
- **First slice.** Rows, transcript (with `load_earlier`), send and queue, abort, approvals and questions, `start_thread`, rename, archive, pin, settle, snooze and mark unread, models, model and thinking level. Images in a remote transcript, `read_file`, diffs and pull requests (the blob ops) follow in the next slice; until then those views show "on <machine>" and nothing else.

## Who owns the data dir: serve as a service and the desktop app

This settles #36 second-S3. One process owns `<data>/studio.lock` at a time, and whichever owns it serves remote clients on the same ports, so a phone or another desktop never needs to know which one it reached.

- **Shared settings.** `<data>/remote/serve.json` holds what both owners need: `name`, `port` (18093), `tunnelPort` (18094 or absent), `allowHosts`, `allowTunnelHosts`, `urls` and `notify`. `local-studio-serve serve` reads it, and command line flags override it for that run. The owner rereads it on `SIGHUP` and when it changes, so adding a tunnel hostname needs no restart.
- **The service waits.** `local-studio-serve serve` started by launchd or systemd tries the lock; while another process holds it, serve logs that once and retries every 2 s. It never exits for a held lock, so the service manager never loops on it.
- **The app takes over.** When the desktop app starts and finds the lock held by a `local-studio-serve` process (the pid in `studio.lock`, checked by its executable name), it sends that process `SIGUSR1` and retries the lock for up to 15 s. Serve, on `SIGUSR1`, closes every remote socket with 1012 (service restart), shuts its `Studio` down, releases the lock, and doesn't try again until another process has held it or 20 s have passed. The Eukhe daemon keeps every running turn through the handover, as it does across any core restart.
- **The app serves while it runs.** An app that owns the lock and finds `serve.json` starts the same server in process, on the same listeners, with the same machine key and devices: the remote crate is a library the kit and the Linux app link. Phones and linked desktops reconnect within their backoff and get a snapshot, since the epoch is new. When the app quits it releases the lock and the waiting service takes over within 2 s.
- **Two subscribers.** The in-process server and the GUI both need every update, so `Studio::subscribe()` returns a second update stream that receives everything `tick()` does; `tick()` keeps serving the GUI unchanged.
- **Installing the service.** `local-studio-serve install [--tailscale] [serve flags]` writes `serve.json` and the user service: `~/Library/LaunchAgents/org.local.studio.serve.plist` (`KeepAlive`, `RunAtLoad`) on macOS, `~/.config/systemd/user/local-studio-serve.service` (`Restart=always`) on Linux, and loads it. `--tailscale` never mounts `/`, because `local-studio tunnel setup --tailnet` owns the machine's tailscale serve config (docs/tunnel.md). When `<data>/tunnel/route.json` exists, it mounts nothing and only adds that file's `hostname` to `allowHosts`. Otherwise it mounts only the two paths serve answers, `tailscale serve --bg --yes --https=443 --set-path=/v1/health http://127.0.0.1:18093/v1/health` and the same for `/v1/ws` (`--https-port` to pick another), adds the machine's tailnet name to `allowHosts`, and records the mounted paths in `serve.json` `tailscale`. `uninstall` removes the service and `serve.json`, and runs `tailscale serve --https=443 --set-path=<path> off` only for the paths install recorded, so it never touches a mount it didn't add. With `--no-load`, install and uninstall write and remove the files and print the tailscale commands they would run in `--json` (`tailscale.state: planned` or `left`) without running them. `LS_TAILSCALE` names another tailscale binary. `status` prints the owner, the listeners and the service state. A Windows logon task waits for the Windows core build (#45).

## Tunnel listener contract

For the one shared tunnel (Tychicus and Local Studio, owned by the tunnel lead):

- The tunnel's Local Studio ingress forwards `/v1/ws` and `/v1/health` to `http://127.0.0.1:18094`, serve's tunnel listener, never to 18093. The tunnel client's `Ports.serve` points there.
- The hostname reaches serve through `serve.json` `allowTunnelHosts`, written with `local-studio-serve allow-tunnel-host <name>` (or `--remove`), and serve picks it up without a restart. Its twin `local-studio-serve allow-host <name>` (or `--remove`) edits the local listener's `allowHosts` the same way, and with `--json` prints `{"state":"added"|"removed"|"unchanged","allowHosts":[...],"port":18093,"live":true|false}`; `local-studio tunnel setup --tailnet` calls it. Serve also adds `wss://<name>/v1/ws` to `machine.urls`, so phones learn the route on their next `hello`.
- A health check before `up` sends `GET /v1/health` to 18094 with `Host: <name>` and expects 200, and expects 403 for any other `Host`.
- Serve trusts `CF-Connecting-IP` only on 18094, which binds 127.0.0.1, so only a local process can set it; cloudflared is that process.

## Gaps against T3 Code

- **Question cards**, above.
- **Thread list paging.** `threads` sends every row. That's fine at hundreds of threads; T3 pages settled threads 25 at a time. Measure before adding paging.
