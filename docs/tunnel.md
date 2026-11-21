# One tunnel per machine

Decision, 2026-11-21 (Claude, tunnels lead, with Donovan's answers). Donovan, 2026-11-21: "1 tunnel for cua and bua, and also this tunnel is the same localstudio will use for syncing across devices." Then: the tunnel is Local Studio's, and Polycarp's name leaves it.

A machine has at most one public route: one cloudflared process with one ingress, or one `tailscale serve` config on port 443. Local Studio owns it on every route. It carries both products:

| Paths | Goes to |
| --- | --- |
| `/v1/health`, `/v1/ws` | local-studio-serve: the tunnel listener (18094) on a Cloudflare route, the local listener (18093) on the tailnet route (docs/remote.md, Tunnel listener contract) |
| `/mcp`, `/.well-known/oauth-protected-resource`, `/.well-known/oauth-protected-resource/mcp`, `/.well-known/oauth-authorization-server`, `/.well-known/oauth-authorization-server/mcp`, `/authorize`, `/token`, `/register`, `/revoke` | Tychicus serve (8787): browser use and computer use |
| anything else | 404 |

These are the eleven paths of baney75/local-studio-tunnels docs/INTERFACE.md. One hostname serves both products, split by these exact paths: the tailnet route has one name per machine, and on the hosted route a second name would halve the zone's tunnel cap.

## The route record

`<data dir>/tunnel/route.json`, where the data dir is `$LOCAL_STUDIO_DATA_DIR` or `~/.local-studio`. Mode 0600 in a 0700 folder. Local Studio writes it; Tychicus reads it.

```json
{
  "v": 1,
  "owner": "local-studio",
  "route": "cloudflare",
  "hostname": "studio.example.com",
  "aliases": [],
  "ports": { "mcp": 8787, "serve": 18094 }
}
```

- `route` is `hosted` (Local Studio's waitlist service), `cloudflare` (the person's own account and domain), or `tailnet` (`tailscale serve`, no Funnel).
- `aliases` are extra hostnames on the same tunnel with the same routes, kept only while a connector moves to `hostname`.
- Ports are loopback, 1024 to 65535, different from each other, and never 18091 (the controller).

## Local Studio sets it up

```
local-studio tunnel setup --cloudflare <name> [--alias <name>]...   the person's own Cloudflare account
local-studio tunnel setup --tailnet                                 tailnet only, no Funnel
local-studio tunnel request                                         the hosted route (waitlist)
local-studio tunnel adopt [--hostname <name>]                       take over a Tychicus tunnel already on this machine
local-studio tunnel status                                          the route, its health and the next step
local-studio tunnel off                                             stop and remove the route; the Cloudflare tunnel and DNS stay
```

`setup` also takes `--serve <port>` and `--mcp <port>` (defaults 18094 on Cloudflare, 18093 on the tailnet, and 8787).

### Your own Cloudflare account

`setup --cloudflare` uses the pinned cloudflared. Without `~/.cloudflared/cert.pem` (only its presence is checked; it is never read) it answers `needs_signin` with the command `<cloudflared> tunnel login` for the person to run. Then it reuses or creates the tunnel `local-studio-<machine>` (a tunnel of that name whose credentials aren't on this machine is refused as `tunnel_elsewhere`), runs `cloudflared tunnel route dns` for the hostname and each alias (a name taken or put on another zone answers `needs_dns`), and writes `<data dir>/tunnel/cloudflared.yml` (0600): for each hostname the eleven paths as anchored `path:` rules (`^/mcp$`, `^/\.well-known/oauth-protected-resource$`), then `http_status:404`. A per-user service runs `cloudflared tunnel --no-autoupdate --config <that yml> run`: launchd `com.localstudio.tunnel` in `~/Library/LaunchAgents` on macOS, `local-studio-tunnel.service` under `systemctl --user` on Linux. Windows has no service yet, so `--cloudflare` answers `unsupported` there. The credentials stay where cloudflared wrote them. Before any service change, `setup`, `adopt` and `off` refuse with `unsafe_service_path`, changing nothing, when the real launchctl or systemctl would run while `HOME` isn't the account's own home, or a stand-in (`LS_LAUNCHCTL`, `LS_SYSTEMCTL`) would run while it is.

Once the service runs, setup registers each hostname with `local-studio-serve allow-tunnel-host` and checks the public hostname: `/v1/health` must answer 200, and `/mcp` must answer 401 with `WWW-Authenticate` naming `resource_metadata` when Tychicus serve listens on `ports.mcp` (`checks.mcp` is `not_listening` otherwise, which doesn't fail setup). The checks retry for about a minute while DNS settles. If they fail, the service is stopped and removed, so nothing stays exposed, and the answer is `error` with code `going_live`. route.json is written last.

### Tailnet

`setup --tailnet` reads `tailscale status --json` (not running or signed out answers `needs_signin`, with Tailscale's sign-in URL when it gives one) and mounts each of the eleven paths on https 443 with `tailscale serve --bg --yes --https=443 --set-path=<path> http://127.0.0.1:<port><path>`: `/v1` paths to the local listener (18093), the rest to `ports.mcp`. It never runs Funnel, and refuses, changing nothing, when Funnel is on for 443, 443 forwards raw TCP, or a path already goes somewhere else. It records the paths it added in `<data dir>/tunnel/tailnet.json`; `off` removes exactly those. route.json carries `ports.serve` 18093 on this route.

### Adopting a Tychicus tunnel

`adopt` reads only Tychicus's `remote.json` and `cloudflared.yml` for the tunnel id, the credentials file path and the old hostname. Without `--yes`, or without a terminal, it answers `confirm` with the plan and changes nothing. With it: the DNS record for `--hostname` (when it differs), Local Studio's yml with the new hostname and the old one as an alias on the same tunnel and credentials file, Local Studio's service, checks on both names, then Tychicus's `com.tychicus.cloudflared` (or `tychicus-cloudflared.service`) is stopped, keeping its files, and both names are checked again with Local Studio's connector alone. route.json comes next, then `tychicus remote join --json`; when Tychicus can't join, the answer is `needs_tychicus_join` with that command. Any failure before route.json removes Local Studio's service and yml, starts Tychicus's service again if it was stopped, and answers `error` with code `adopt_rolled_back`. A DNS record already added stays.

While both connectors run, Cloudflare spreads requests across them, and Tychicus's connector answers 404 for names and paths its own config lacks, so the first checks retry until Local Studio's connector answers.

### off

`off` works on every channel. It removes Local Studio's service and yml, the tailnet paths it added, and route.json, asks `local-studio-serve allow-tunnel-host --remove` for each name, and stops a hosted cloudflared. The Cloudflare tunnel, its DNS records and the tailnet node stay. A route run by Tychicus is refused with `not_ours`.

### Dev channel only, for now

Donovan, 2026-11-21: tunnel setup ships on the dev channel only (docs/channels.md). On stable and nightly, `setup`, `adopt` and `request` don't run and the settings screen shows a waitlist signup instead. The gate reads `local_studio_core::CHANNEL`, which builds stamp from `LOCAL_STUDIO_BUILD_CHANNEL` and local builds set to `dev`. `status` and `off` work on every channel, so a machine that moves off dev can still see and remove its route. The waitlist is a signup-only Worker on a free workers.dev address that stores emails and holds no Cloudflare API token; it moves to the hosted route's own account when that exists.

### Agents can run it

An agent must be able to set a tunnel up for a person. Every `tunnel` command takes `--json`, never prompts when stdin isn't a terminal, and returns one state with the next step: `ready`, `needs_signin` (the person signs in to Cloudflare or Tailscale; the state carries the URL or the command), `needs_dns`, `confirm` (adopt's plan, waiting for `--yes`), `needs_tychicus_join`, `off`, `waitlist` (stable or nightly; carries the signup URL), or `error` with a code (exit 1). On stable and nightly, `status` with no route and no hosted request also answers `waitlist`. `--yes` accepts the steps that change the machine, such as adopting a Tychicus tunnel. Signing in stays with the person: an agent hands over the URL and polls `status --json`.

### Going live

Before a route carries a path, Local Studio checks auth there (baney75/local-studio-tunnels docs/DESIGN.md, Going live): `/mcp` answers 401 with `WWW-Authenticate` naming the resource metadata, and `/v1/health` answers 200 for the route's hostname and 403 for any other Host. It registers the hostname with `local-studio-serve allow-tunnel-host <name>`. The Cloudflare route uses the pinned, verified cloudflared, the same release and digests as Tychicus's download, and reuses Tychicus's copy when its digest matches.

## Tychicus joins

`tychicus remote join` reads the record, stops any tunnel of its own without deleting it, and serves on `ports.mcp` with `https://<hostname>` as its public address, answering OAuth on each alias too. On a machine with Local Studio, `tychicus remote setup` runs `local-studio tunnel setup` and then joins. On a machine without Local Studio, Tychicus keeps its own `remote setup` with the same exact paths.

`local-studio tunnel adopt` moves a Tychicus tunnel to Local Studio without a gap: it keeps the tunnel and its credentials, adds the new hostname, keeps the old one as an alias, starts Local Studio's service, then runs `tychicus remote join`. Any failure puts Tychicus's service back.

## Not yet

- The hosted route waits on Donovan's choice of a Cloudflare account and domain (`STABLE_SERVICE`).
- Tychicus's Funnel route (port 8443) opens the whole port, so it can't keep to exact paths; Local Studio doesn't use Funnel.
