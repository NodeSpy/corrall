# Configuration

Path: `$CORRALL_CONFIG`, else `$XDG_CONFIG_HOME/corrall.json`, else
`~/.config/corrall.json`. Written `0600`, atomically. Unknown keys are kept.
A pre-rename `teamclaude.json` beside a missing `corrall.json` is an error, not
a fresh start: `scripts/install.sh` renames it (see the README).

Runtime state (observed quota, usage counters) goes to `corrall.state.json`
beside it. Safe to delete; quota is re-learned from traffic.

## Pools

Accounts live in named pools, each with its own rotation, thresholds and
sessions. An account belongs to exactly one pool. Requests reach a pool through
a `/pool/<name>` prefix on the base URL; anything without one goes to
`defaultPool`.

A config written before pools is migrated on first load: every account and
rotation setting moves into `pools.default`, and the file is re-saved. Nothing
to do by hand, and a one-pool install behaves exactly as before.

```json
{
  "defaultPool": "default",
  "pools": {
    "default": { "accounts": [ … ] },
    "work": { "switchThreshold": 0.9, "holdSeconds": 120, "accounts": [ … ] }
  }
}
```

Pool names are 1–32 characters of `a-z`, `0-9` and `-`, not starting or ending
with `-`. No name is reserved: routing lives under the fixed `/pool/` keyword,
which no real API path uses, so a pool may be called `v1` or `api`.

| Field | Default | Description |
| --- | --- | --- |
| `defaultPool` | `default` | Pool serving requests that carry no `/pool/<name>` prefix |
| `pools` | one `default` pool | Name → pool. Each pool holds `accounts`, `routes` and its own rotation settings |

Per-pool fields: `accounts`, `routes`, `switchThreshold`, `holdSeconds`,
`distributeSessions`, `quotaProbeSeconds`, `blockedModels`, `stormRamp`,
`expiryRouting`, `match`, `fallbackPool`. Everything else in the table below is daemon-global.

### Reaching a pool

In base-URL mode the pool is a path prefix, ahead of any `/tc-acct/<pin>`:

```
ANTHROPIC_BASE_URL=http://127.0.0.1:3456/pool/work
ANTHROPIC_BASE_URL=http://127.0.0.1:3456/pool/work/tc-acct/alice
```

The server strips both prefixes before forwarding, so the upstream sees the
path the client asked for. An unknown pool falls back to `defaultPool` rather
than failing the request — a stale base URL should not take a client down.

MITM mode has no local URL to hang a prefix on, so the pool travels in the
proxy username beside the optional pin, as `[<pin>]~<pool>`:

```
HTTPS_PROXY=http://alice~work:@127.0.0.1:3456
HTTPS_PROXY=http://~work:@127.0.0.1:3456
```

`corrall env --pool work` emits the right form for whichever mode is
configured. The default pool is never named in either form, so a one-pool
install emits byte-for-byte what it emitted before pools existed.

### Auto-selecting a pool

A pool can carry a `match` block, and then `corrall env` / `corrall run`
picks it from the launch context — no per-project wrapper needed:

```json
"work": {
  "match": {
    "paths": ["~/Projects/acme"],
    "remotes": ["(?i)^git@github\\.com:acme/"],
    "env": { "TC_CTX": "^work-", "ACME_CI": "" }
  }
}
```

| Group | Matches when |
| --- | --- |
| `paths` | The launch directory is one of these directories, or nested under it. A leading `~` expands; a trailing `/*` or `/**` is ignored |
| `remotes` | A regular expression matches `git remote get-url origin`, run in the launch directory. No repo or no `origin` fails the rule quietly |
| `env` | Variable → regular expression its value must match. An empty pattern means "matches whenever the variable is set"; an unset or empty variable never matches |

A pool matches when **any** one condition in its block does. Non-default pools
are tried in sorted name order and the first match wins, so the outcome does
not depend on config order; with nothing matching, the default pool serves.
Rules on the default pool itself are ignored — it is already the fallback — and
`config check` warns about them. Every pattern must compile, checked when the
config loads and when `pool set` writes it.

`--pool` and `TC_POOL` skip matching entirely. When nothing matches, `env`
emits exactly the lines it emitted before pools existed and says nothing on
stderr, so an install that writes no rules is unaffected. When a rule does
fire, the chosen pool and the reason go to **stderr** while the exports still
go to stdout:

```
$ eval "$(corrall env)"
[Corrall] pool "work" (path ~/Projects/acme)
```

`corrall env --cwd DIR` matches against `DIR` instead of the current
directory, for a wrapper resolving a project it has not entered yet.

## Top level

| Field | Default | Description |
| --- | --- | --- |
| `proxy.port` | `3456` | Local port. `corrall server --port N` overrides it for one run |
| `proxy.host` | `127.0.0.1` | Bind address. Anything non-loopback requires `proxy.apiKey` of at least 16 chars; the server refuses to start otherwise. `CORRALL_HOST` overrides, as does `corrall server --listen HOST:PORT` for one run. `corrall env`/`run` and the CLI's own control calls dial loopback when this is a wildcard (`0.0.0.0`, `::`) — nothing can connect to a wildcard — and dial the host itself when it is specific |
| `proxy.apiKey` | generated | Key clients present via `x-corrall-key` (the form `corrall env` emits, through `ANTHROPIC_CUSTOM_HEADERS`), `x-api-key`, `Authorization: Bearer tc-…`, or the Basic password on CONNECT. Generated once as `tc-…` and written back if the file has none; a key you set is never touched |
| `proxy.clientKeys` | `[]` | `[{ "name", "key" }]`; usage is attributed to `name` |
| `proxy.requireKeyOnLoopback` | `false` | Require the key even from 127.0.0.1. Recommended on shared hosts |
| `proxy.sessionDetail` | `false` | Include a per-session breakdown in `/corrall/status` |
| `proxy.usageDimensions` | `[]` | `[{ "name", "header" }]`: request headers consumed by the proxy for attribution (not forwarded) |
| `proxy.maxBodyBytes` | `67108864` | Largest client request body accepted |
| `upstream` | `https://api.anthropic.com` | Upstream base URL. Must be https unless loopback |
| `switchThreshold` | `0.98` | Number, or table `{ "default": 0.98, "unified7d": 0.9, … }` keyed by bucket |
| `holdSeconds` | `0` | Hold a request this long when no account can take it, instead of answering at once (429 when the fleet is spent, 502/504 when every account failed; see [When nothing can serve](#when-nothing-can-serve)) |
| `distributeSessions` | `false` | Spread new sessions across equal-priority accounts, pinned per weekly bucket |
| `distributeBy` | `sessions` | What a new session is placed on while `distributeSessions` is on. `sessions`: the account serving the fewest sessions, then the fewest open requests. `usage`: the account with the most quota left, by 5h utilization and then by the governing weekly one, each compared in whole percent; accounts within a percent of each other fall back to session count, so several sessions starting together spread out before any response has moved a reading. An account with no reading counts as unused. A session already placed stays where it is either way. `corrall distribute usage` sets both keys |
| `quotaProbeSeconds` | `0` | Background zero-spend usage probe interval per account (min 30). Accounts are staggered across the interval rather than probed together, and an account added or re-logged-in by `login` is probed at once. A run that draws HTTP 429 from the usage endpoint stops early and the pool waits twice the interval |
| `eventLogging` | `hide` | Claude Code telemetry: `hide` (forward, not shown), `block` (answer 200 locally), `show` |
| `blockedModels` | `[]` | Globs of models rejected with a fast 400 |
| `routes` | `[]` | `[{ "name", "match": [globs], "accounts": [names], "bucket"?, "color"? }]`, first match wins |
| `stormRamp` | on | `{ "enabled", "startConc": 1, "stepConc": 1, "stepMs": 250, "windowMs": 30000 }` |
| `expiryRouting` | off | `{ "enabled", "tolerance": 1.5, "preempt": true }`: rank the top priority tier by headroom ÷ seconds-to-reset of the governing weekly bucket, keep accounts within `tolerance` of the best, and with `preempt` re-rank the sticky/pinned account when its window rolls over |
| `soonestReset` | `false` | Within a priority tier, spend first the quota that is lost first. Accounts are ranked by governing weekly reset, soonest first, with weeks ending within 24 hours of the soonest one counted as equal. Within that band they are ranked by 5h reset, soonest first, with resets within 15 minutes of the soonest counted as equal; an account with no running 5h window counts as starting one now, so it goes after every running window. `distributeSessions` then spreads new sessions by load only across accounts that tie on both bands. Bands are measured from the accounts still available, and accounts at the switch threshold are skipped as usual. A sticky account, or a session pinned to one, moves only to an account in an earlier weekly or 5h band |
| `warmupSeconds` | `0` | Keep-warm interval (min 60). Spawns `claude -p --bare --model haiku` per idle account through this proxy; spends a little quota |
| `fableColumnFirst` | `false` | In the dashboard's accounts table, show the Fable weekly window before the shared 7d one. On a terminal with room for one weekly column, it is the first that stays. Also a row in the dashboard's settings |
| `sessionTitles` | off | `{ "enabled", "width": 18, "projectsDir"? }`: name sessions in [the sessions table](#the-sessions-table) and the activity log from Claude Code's own files on this machine. The name Claude Code shows for a running session and its directory come from `~/.claude/sessions/<pid>.json`; a `/rename`d or generated title, and the directory of a session that has exited, from the transcript under `~/.claude/projects`. Only `.json` files are opened in `sessions/`, and only for UUID-shaped session ids. `width` is the label's width in the log; `projectsDir` moves both directories (`sessions/` is taken to sit beside it). Off, a session is its id |
| `mitm.http1Only` | `true` | Offer only HTTP/1.1 inside the intercepted tunnel (needed for WebSocket / Remote Control). With `false` the tunnel also offers `h2` and serves whichever the client negotiates |
| `mitm.allowTunnel` | `false` | Allow blind CONNECT tunnels to non-intercepted hosts (port 443, public addresses only: the name is resolved first and refused if any answer is loopback, private, link-local or the metadata address). `platform.claude.com`, `mcp-proxy.anthropic.com` and `*.mcp.claude.com` are tunnelled regardless, see [claude.ai connectors](#claudeai-connectors) |
| `mitm.tunnelAllow` | `[]` | Explicit `host` or `host:port` allow-list for blind tunnels |
| `logDir` | unset | One file per request. Directory 0700, files 0600 |
| `logLevel` | `body` | `body`, `headers`, or `off` |
| `logMaxBodyBytes` | `262144` | Per-direction body cap in log files (`0` = unlimited) |
| `logRetentionHours` | `72` | Sweep age (`0` = keep) |
| `upstreamProxy` | unset | Outbound proxy URL for everything sent upstream. Unset = honour `HTTPS_PROXY`/`ALL_PROXY`; `""` = ignore the environment |
| `noProxy` | `$NO_PROXY` | Hosts bypassing `upstreamProxy` |

## Accounts

| Field | Description |
| --- | --- |
| `id` | Stable id issued on first read. Leave alone |
| `name` | Display name; also accepted by every command and by `TC_ACCT` |
| `type` | `oauth` or `apikey` |
| `provider` | `codex` for an OpenAI Codex subscription (default Anthropic). Codex tokens default to `importFrom: ~/.codex/auth.json` when no tokens are stored |
| `accountId` / `planType` | ChatGPT account id and plan (Codex), filled from the login |
| `priority` | Lower is preferred (default 0) |
| `disabled` | Excluded from rotation |
| `accessToken` / `refreshToken` / `expiresAt` | OAuth tokens (ms epoch). Written back on refresh |
| `refreshTokenExpiresAt` | When the login itself expires (ms epoch), as the token endpoint reports it. Refreshing does not extend it, so past it the account needs `corrall login` again. From five days out, `corrall status`, the TUI and the pool log say so. Unknown (and not warned about) until a login or refresh reports it |
| `importFrom` | Linked account: read tokens from this file (Claude Code's `~/.claude/.credentials.json`) instead of storing them. The proxy never refreshes a linked account; it re-reads the file near expiry, after a 401, and every few seconds while the account is in error, and leaves the refresh to the CLI that owns the file. Refreshing it here would rotate that CLI's refresh token and log it out |
| `apiKey` | For `apikey` accounts |
| `upstream` | Per-account base URL (third-party Anthropic-compatible API). Refused for `oauth` accounts pointing off Anthropic |
| `modelMap` | `{ "claude-sonnet-4-6": "deepseek-v4-pro" }` |
| `stripRequestFields` | Top-level body fields to drop for this account |
| `maxUsage` | Hard cap, number or per-bucket table. At the cap the account gets nothing, pins included |
| `accountUuid`, `orgUuid`, `orgName`, `email`, `rateLimitTier`, `seatTier` | Filled from the OAuth profile |

Bucket keys: `unified5h`, `unified7d`, `unified7dFable`, `unified7dSonnet`,
`tokens`, `requests`.

## Environment variables

| Variable | Effect |
| --- | --- |
| `TC_ACCT` | Pin `corrall run` / `env` to one account (uuid, org uuid, `uuid/org`, name or email). A handle that fits several accounts (an e-mail shared by two orgs, say) is refused; use the full name or id. Removed from the child environment |
| `TC_POOL` | Send `corrall run` / `env` to one pool, the same way `TC_ACCT` chooses an account. `--pool` wins over it, and either skips `match` rules. Removed from the child environment |
| `CORRALL_CONFIG` | Config path |
| `CORRALL_HOST` | Override `proxy.host` |
| `CORRALL_LOG` | `tracing` filter, e.g. `debug` |
| `CORRALL_UPSTREAM_HEADERS_TIMEOUT_MS` | Time to response headers, default 120000. Not a total deadline: a streamed response runs as long as chunks keep arriving. On expiry the request is answered 504 and not resent (see "When nothing can serve") |
| `CORRALL_UPSTREAM_BODY_TIMEOUT_MS` | Idle gap between body chunks, default 120000 |
| `CORRALL_UPSTREAM_MAX_SOCKETS` | Idle pooled connections per host, default 256 |
| `CORRALL_REFRESH_TIMEOUT_MS` | OAuth refresh timeout, default 30000 |
| `CORRALL_RATE_LIMIT_ABSORB_MAX_SECONDS` | Longest `retry-after` absorbed inline when holding, default 60 |
| `CORRALL_FAMILY_STALE_MS` | How long a spent family (Fable/Sonnet) reading is trusted before revalidation, default 1800000 |
| `CORRALL_REPO` | `owner/repo` `corrall update` fetches releases from, default `NodeSpy/corrall` (the same knob `scripts/install.sh` reads) |

## Control endpoints

All under `http://127.0.0.1:<port>/corrall/`, authenticated like any other
request. `POST` bodies are capped at 64 KiB. Browser-originated requests are
refused.

| Endpoint | Description |
| --- | --- |
| `GET health` | `{ ok, version }` |
| `GET dashboard` | Static HTML page; asks for the key and polls `status` |
| `GET status` | Full account, quota, route, session and client-usage view. One entry per pool under `pools`; the default pool's view is also flattened onto the top level |
| `GET quota` | Tier-weighted fleet quota for status lines, for the addressed pool |
| `GET metrics` | Prometheus text format. Per-account series carry a `pool="…"` label |
| `GET sessions` | Who is being served: `{ active, known, names, shown, sessions: [...] }`, one entry per session heard from in the last hour across every pool (at most 100; `active` and `known` count them all), those with a request open first, then the most recent. Each has `id`, `pool`, `client`, `name`, `title`, `project`, `account`, `model` (the one most of its requests named), `active`, `inFlight`, `busyMs` (how long its oldest open request has run, else null), `idleMs`, `ageMs`, `firstSeen`, `lastSeen`, `requests`, `tokens` (`input`, `output`, `cacheRead`, `cacheCreation`) and `cacheHit` (cache reads as a share of prompt tokens). `names` is whether `sessionTitles` is on |
| `GET activity` | The activity feed as server-sent events: one JSON object per `data:` line (`type` is `start`, `account`, `end` or `log`), a `: ping` comment every 15 s. Stays open until the client leaves. What `corrall attach` tails. A `start` carries the request's `bytes` and `tools` (how many tool definitions it sends in full rather than deferred); an `end` carries `tokens` (`input`, `cacheRead`, `cacheCreation`, `output`) when the response reported usage. The feed opens with the last 900 events the server saw, oldest first, each with `at` (ms since the epoch) to mark it as past; events without `at` are live |
| `POST reload` | Re-read the config |
| `POST probe` | Start a quota probe of every OAuth account now. `{ ok: true, result: "started" }`, or `ok: false` with `result` `already-running` / `too-soon` (and `waitSeconds`) |
| `POST switch` `{ "account": "…", "pool"? }` | Prefer one account. Without a pool, every pool is searched |
| `POST pools/<pool>/accounts/<id>/enable`, `…/disable`, `…/priority` `{ "priority": n }` | What `corrall enable`, `disable` and `priority` do: written to the config, applied to the running fleet. `<id>` is the account's id, name or e-mail |
| `POST pools/<pool>/accounts/<id>/probe` | Read that one account's quota now. Answers like `POST probe`; refused within 30 s of the last probe of the same account, and 404 for an API-key account, which has no usage endpoint |
| `POST route-pin` `{ "route": "…", "account": "…", "pool"? }` | Pin a route (omit `account` to clear) |

A `/pool/<name>` prefix on the control path selects the pool too, so
`/pool/work/corrall/quota` reads the `work` fleet.

### The dashboard

`corrall server` in a terminal draws the dashboard; `corrall attach` draws the
same one for a server that is already running, without starting a second: it
polls `status` and `sessions` every 2 s and tails `activity`. Bare `corrall`
picks for you: it attaches when a server answers at the configured address and
starts one otherwise. Quitting an attached dashboard leaves the server
running; if the server stops answering, the header says so and the tables keep
the last state they saw. `--url http://host:port` and `--key` attach to a
server elsewhere.

The main screen has the accounts, [the sessions](#the-sessions-table) and the
activity log. A cursor sits on one account:

| Key | Does |
| --- | --- |
| `↑` `↓` (`k` `j`) | Move the cursor |
| `Enter` | Open that account: everything `status` knows about it, the sessions it is serving and the log lines that name it |
| `s` | Switch to it |
| `d` | Disable it, or enable it again |
| `+` `-` | Move its priority by one (the lowest number is tried first) |
| `p` | Probe every account's quota now; in the account's own screen, probe just that one |
| `a` | Add an account: a browser opens on Claude's sign-in page and the screen closes when it comes back. With the browser on another machine, paste the address it ends on. Signing in as an account that is already here renews its login |
| `i` | Import a Claude Code login from `~/.claude*/.credentials.json`, linked (Claude Code keeps refreshing it) or copied |
| `t` | List every request in the activity pane, or go back to news only |
| `l` | The whole log, 2000 lines back: `↓` `↑` and `PgDn` `PgUp` scroll, `w` keeps only warnings, errors and failed requests |
| `g` | Settings |
| `R` | Reload the config |
| `q` | Quit (from another screen: back) |

In an account's screen, `L` logs it in again and `x` removes it, after a `y`.

**Settings** lists, for one pool at a time: the switch threshold, how new
sessions are spread (`off`, by sessions, by usage), the quota probe interval,
the hold, soonest-reset and expiry-pressure routing. Below those, for the
whole server: session names, what happens to telemetry requests, the
keep-warm interval, and which weekly quota column the accounts table shows
first (7d or 7d fable). `Enter` steps a choice or opens a number for typing; the
line under the list says what the row under the cursor means. Each change is
written to the config file and the server reloads it, exactly as the matching
command (`corrall threshold`, `distribute`, `probe`, …) would. Three more rows
run something, each after a `y`: point Claude Code at the proxy, take that
back out (`corrall setup claude` and `--remove`), and install the
[service](#running-as-a-service).

Switching, disabling, priorities and probes go through the control API, so
they work on any server the dashboard is attached to. Adding, importing and
removing accounts and the settings screen edit the config file on this
machine, as the CLI does, so they are offered only when that file is the
server's: always under `corrall server`, and under `attach` unless `--url`
points somewhere else.

The activity pane shows what is worth reading: an account switch and why
(`Switched to account "b": "a" is out (quota: 5h at 99%)`), a request for one
model family being diverted, a session moving account when sessions are
distributed, an account coming back (`Account "a" can serve again`), probes,
warnings, and requests that failed. Requests that went through are left to the
sessions table; `t` lists them in the pane too, and the log screen always has
them. An attached dashboard starts with the server's recent history rather
than an empty pane, each line at the time it happened.

A finished request's line in the log ends with what it cost, `184.2k in (96%
cached), 250 out`, and carries the size of the request body beside the model.
A session that sends megabytes each turn with little of it cached is usually
one started before `setup claude`, without deferred tool loading; the headless
log (`journalctl`, or the log file on macOS) adds how many tool definitions
each request carried in full.

### The sessions table

Between the accounts and the activity log the dashboard lists the sessions the
proxy has heard from in the last hour, one row per `x-claude-code-session-id`:

| Column | Meaning |
| --- | --- |
| (mark) | A spinner while the session has a request open |
| `session` | The first eight characters of its id |
| `name` | What Claude Code calls it: a `/rename`d title, else its running name (`myrepo-e0`), else a generated title, else its directory. Needs `corrall titles on`; without it, or for a session from another machine, the id. A named client key shows as `[client]` in front |
| `account` | The account serving it (`pool/account` with more than one pool) |
| `model` | The model most of its requests named, so a side request on a small model does not rename the row |
| `last` | How long its open request has been running (`12s…`), or how long ago it was last heard from |
| `age` | Time since its first request |
| `req` | Requests served |
| `in`, `cache r`, `cache w`, `out` | Tokens: uncached input, cache reads, cache writes, output |
| `hit` | Cache reads as a share of all prompt tokens. A long session far below the others is paying for its context again on each turn |
| `title` | The generated or `/rename`d title, when there is room |

Rows with a request open come first, then the most recently active; a session
idle for more than two minutes is dimmed. On a narrow terminal columns are
dropped, `title` first, down to name, account, last and output tokens. A
request that names no session (another client, a Codex CLI) has no row and
shows in the activity log while it is open, as before.

## Running as a service

`corrall service install` writes a per-user service for this binary and the
config it is using, starts it, and has it start at every login. Run it again
after moving the binary or the config. `service status` asks the service
manager how it is doing, `service uninstall` stops it and removes the file,
and `service print` shows what `install` would write.

| | Linux | macOS |
| --- | --- | --- |
| Manager | `systemd --user` | launchd, in the user's `gui/<uid>` domain |
| File | `~/.config/systemd/user/corrall.service` | `~/Library/LaunchAgents/com.nodespy.corrall.plist` |
| Restarted | when it fails (`Restart=on-failure`) | whenever it exits (`KeepAlive`) |
| Log | `journalctl --user -u corrall.service` | `~/Library/Logs/corrall.log` (mode 0600; appended to, never rotated) |
| Restart by hand | `systemctl --user restart corrall.service` | `launchctl kickstart -k gui/$(id -u)/com.nodespy.corrall` |

Both run `corrall server --headless` with `CORRALL_CONFIG` set. On macOS the
agent needs the user's desktop session: installing over SSH on a Mac nobody is
logged in to fails, and says so. launchd gives a job a bare `PATH`, so the
plist adds the Homebrew directories for the `gh` the release check uses.
`corrall update` restarts whichever service is running, and
`scripts/install.sh` stops and reloads it around a swap.

## Setting Claude Code up

`corrall setup claude` writes the proxy into Claude Code's user settings
(`$CLAUDE_CONFIG_DIR/settings.json`, else `~/.claude/settings.json`), so every
session goes through it without a wrapper or a shell export. It writes the
`env` entries `corrall env --no-mitm` prints, and the rest of the file is left
as it was:

```json
{
  "env": {
    "ANTHROPIC_BASE_URL": "http://127.0.0.1:3456",
    "ENABLE_TOOL_SEARCH": "auto",
    "_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL": "1"
  }
}
```

The proxy key is added (as `ANTHROPIC_CUSTOM_HEADERS`) only when the server
will ask for one, `API_TIMEOUT_MS` only when the pool has `holdSeconds`, and
`--api-key` selects [the other mode](#choosing-a-mode). A file that ends up
holding the key is written at mode 0600.

So that nothing else routes or signs Claude Code in, setup also removes from
that file:

| Removed | Why |
| --- | --- |
| `ANTHROPIC_BASE_URL` (replaced), `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_API_KEY`, `ANTHROPIC_CUSTOM_HEADERS`, top-level `apiKeyHelper` | Another gateway and its credential. The token, key and helper also take Claude Code off its claude.ai login |
| `HTTPS_PROXY`, `HTTP_PROXY` and their lower-case forms | A forward proxy; the base URL is dialled directly |
| `CLAUDE_CODE_USE_BEDROCK`, `_VERTEX`, `_FOUNDRY`, `_MANTLE`, `_ANTHROPIC_AWS` and the `ANTHROPIC_*_BASE_URL` of each | A cloud provider. With one selected, `ANTHROPIC_BASE_URL` is not read |

The file as it was is copied to `settings.json.corrall-backup-<time>` (mode
0600) first, and the command reports the names of the keys it set and removed,
never their values. `--dry-run` reports without writing. Running it again
changes nothing.

`corrall setup claude --remove` takes out the entries setup wrote, and only
those: an `API_TIMEOUT_MS` of your own stays. It does not put back what setup
removed; that is in the backup.

What a settings file cannot do, compared with `corrall run` and `corrall env`:

- **One pool.** The file serves every directory, so [pool
  matching](#auto-selecting-a-pool) does not apply. `--pool <name>` picks the
  pool; the default pool otherwise.
- **Base-URL mode only.** No MITM, so only what `ANTHROPIC_BASE_URL` covers
  goes through the proxy.
- **No fallback.** With the proxy down Claude Code cannot connect. Setup warns
  when the server is not answering; `corrall service install` keeps it running.

On macOS setup goes one step further, since nothing else there brings the
proxy up at login. After writing the settings it checks the [launchd
service](#running-as-a-service):

| It finds | It does |
| --- | --- |
| The service running and the proxy answering | Says so |
| No service, and nothing answering on the proxy's port | Installs the service and starts it (what `corrall service install` does), then waits for it to answer |
| The service loaded but the proxy silent | Waits a few seconds for a start in progress, then points at the log |
| The proxy answering with no service behind it | Warns that it was started by hand and will stop with its terminal. It does not install: the two would fight over the port |

`--no-service` leaves the service alone and only warns, `--dry-run` reports
what would be installed, and `--remove` never touches the service.

Three things can still override the file. A settings `env` value wins over the
same variable exported in a shell, but a variable the file does not set still
arrives from the shell, so setup warns when one of the variables above is
exported where it runs. A project's `.claude/settings.json` or
`.claude/settings.local.json`, and an organization's managed settings, rank
above user settings in Claude Code.

## Passthrough paths

`/v1/oauth/token`, `/api/oauth/*`, `/v1/code/*` (Remote Control, including
its WebSocket upgrade), `/v1/mcp_servers` and `/api/organizations/*/mcp/*`
(the claude.ai connectors, see below) are relayed to the Anthropic upstream
with the client's own `authorization` header and no account selection.
Hop-by-hop headers and the proxy key are stripped.

## claude.ai connectors

Claude Code loads the MCP connectors a user authorised on claude.ai only while
its own claude.ai login is the auth source. As soon as `ANTHROPIC_API_KEY` (or
`ANTHROPIC_AUTH_TOKEN`, or an `apiKeyHelper`) is set, Claude Code switches to
that and drops the connectors with a note that they "are disabled because
ANTHROPIC_API_KEY or another auth source is set". That is Claude Code's
behaviour, not the proxy's, but a `settings.json` that hands the proxy key over
as `ANTHROPIC_API_KEY` triggers it.

Three things make connectors work through Corrall:

1. **Stay logged in to Claude Code** (`/login` there, once) and do not set
   `ANTHROPIC_API_KEY`. When the proxy is dialled over loopback no key is
   needed at all. When it is (`requireKeyOnLoopback`, or a proxy on another
   machine), pass it in Corrall's own header instead, which Claude Code adds
   to every request and the proxy strips before anything goes upstream:

   ```json
   {
     "env": {
       "ANTHROPIC_BASE_URL": "http://127.0.0.1:3456",
       "ANTHROPIC_CUSTOM_HEADERS": "x-corrall-key: tc-…",
       "ENABLE_TOOL_SEARCH": "auto",
       "_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL": "1"
     }
   }
   ```

   `corrall env` and `corrall run` emit exactly this (and never
   `ANTHROPIC_API_KEY`), and [`corrall setup claude`](#setting-claude-code-up)
   writes it into the settings file; the last two variables are explained
   [below](#request-size-behind-a-custom-base-url).
2. **The connector list is relayed with the client's own login.**
   `/v1/mcp_servers` and `/api/organizations/*/mcp/*` are passthrough paths:
   the connectors belong to the login that authorised them, and an injected
   account token would answer with another user's list, whose ids the connector
   proxy then refuses for the client's token. In base-URL mode Claude Code
   dials `api.anthropic.com` for these directly; in MITM mode they arrive
   through the intercepted tunnel, which is where the passthrough matters.
3. **The connector MCP proxy is reachable in MITM mode.** Connector traffic goes
   to `mcp-proxy.anthropic.com`, and Claude Code refreshes its login at
   `platform.claude.com`; the first-party connectors live under
   `*.mcp.claude.com`. These hosts get a blind CONNECT tunnel on port 443
   whatever `mitm.allowTunnel` says, since they carry the client's own token
   and nothing the proxy would rotate. Other hosts still need `allowTunnel`
   or `tunnelAllow`.

MCP servers configured in Claude Code itself (`claude mcp add`, `.mcp.json`)
are not affected by any of this: stdio servers never touch the proxy, and
remote ones only do in MITM mode, where their hosts need a tunnel.

### Request size behind a custom base URL

Claude Code treats any `ANTHROPIC_BASE_URL` other than `api.anthropic.com` as a
third-party endpoint and switches deferred tool loading off, on the assumption
that the endpoint may not forward the ToolSearch tool's `tool_reference`
blocks. Every MCP tool schema then rides on every request in full, and with a
few claude.ai connectors attached those schemas are most of the request body.
Measured on the same turn against a local capture endpoint, Claude Code
2.1.273 with three connectors connected:

| | tools in request | body bytes |
| --- | --- | --- |
| custom base URL, nothing else | 111 (81 from connectors) | 325,924 |
| plus `ENABLE_TOOL_SEARCH=auto`, `_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL=1` | 13 | 59,623 |

Corrall forwards requests to Anthropic unchanged, `tool_reference` blocks
included, so `corrall env` and `corrall run` set both variables in base-URL
mode. `_CLAUDE_CODE_ASSUME_FIRST_PARTY_BASE_URL=1` has Claude Code treat the
base URL as a first-party host (which also turns its gateway discovery off and
the `count_tokens` endpoint on); `ENABLE_TOOL_SEARCH=auto` turns deferred tool
loading back on, which the host check otherwise forces off. MITM mode needs
neither, since there `ANTHROPIC_BASE_URL` stays unset. A hand-written
`settings.json` should carry both, as in the recipe above.

### Choosing a mode

Both ways of handing Claude Code the proxy key keep working. The difference is
what Claude Code does with its own login:

| | `ANTHROPIC_API_KEY` | header (default) |
| --- | --- | --- |
| Launcher | `corrall env --api-key`, `corrall run --api-key` | `corrall env`, `corrall run` |
| Claude Code's auth source | the proxy key | its claude.ai login (`/login` once) |
| claude.ai connectors | off | on |
| Request size | smaller: local tools only | larger: every connected connector's tool schemas ride along, unless deferred ([above](#request-size-behind-a-custom-base-url)) |
| Works with no Claude Code login | yes | no |

`--api-key` emits `ANTHROPIC_API_KEY` always, whatever the bind address, since
its point is the mode rather than authentication; in MITM mode the proxy
strips the header it produces. A hand-written `settings.json` picks either
form the same way: the `env` block above, or `"ANTHROPIC_API_KEY": "tc-…"`.

## When nothing can serve

Two different situations end with no account for a request, and they are
answered differently. A **spent fleet**, where every eligible account is held
back by quota or a rate limit, gets a 429 `rate_limit_error` with a
`retry-after` taken from the soonest of those accounts' recoveries (their
throttle, or their 5h/7d reset, at most 3600 s). A **failing upstream**, where
every eligible account was tried and answered 5xx or refused the connection,
gets a 502 `api_error` with no `retry-after`: those accounts are healthy, and
their quota resets say nothing about when the upstream will answer again.
`holdSeconds` holds either case for its duration and retries the whole fleet
before answering.

A pool with a `fallbackPool` (`corrall pool set tap --fallback default`)
answers a **spent fleet** differently: the request is served by the fallback
pool's accounts instead, and by that pool's own fallback if it is spent too,
never visiting a pool twice. It happens per request, so the first request after
the pool's quota resets is served from the pool again. Pinned requests and a
failing upstream never fall back. When every pool in the chain is spent,
`holdSeconds` (of the requested pool) applies and the retry starts again from
the requested pool.

A request that was **already sent** and then got no answer is different again:
no response headers within `CORRALL_UPSTREAM_HEADERS_TIMEOUT_MS`, or a
connection that died after the body went out, ends the request at once with a
504 (or 502) `api_error` and is **not** resent on another account. The upstream
may well still be running and billing that prompt; resending it would pay for
it twice, and a client retry on top would pay a third time. The client decides
whether to retry. Only a failure before anything left this side (DNS, TCP
connect, TLS) moves the request to a sibling.

## Per-minute 429s

A 429 without `anthropic-ratelimit-unified-*-status: rejected` is a per-minute
limit, not a spent quota. With a `retry-after` of 15s or less the proxy waits
and retries the same account; longer than that it fails over once. Without a
`retry-after` header it pauses the account 5s and retries it, rather than
assuming 60s. A request rate-limited on two accounts in a row is answered 429
with the upstream `retry-after` instead of trying a third: a limit that follows
the request across accounts is not per-account, and each further hop only
marked another account unavailable to every other session. Once two accounts
have answered a 429 without `retry-after` within 30s, the next such 429 is
passed to the client at once with `retry-after: 10` and no inline retry: the
throttle is in front of the whole fleet, and each retry only added load to it
on top of the client's own retries. Every such 429 is
logged at `warn` with its `retry-after`, `content-type`, `cf-ray`,
`request-id` and the start of its body.

## Importing from claudeacrobat

`corrall import-claudeacrobat` reads the account files of a
[claudeacrobat](https://github.com/EdnitionCode/claudeacrobat) install and
writes them into this config. It is a read: claudeacrobat's own files are never
touched, so both proxies keep working (on their own ports) afterwards.

```bash
corrall import-claudeacrobat --dry-run          # what would land where
corrall import-claudeacrobat                    # keep its pool layout
corrall import-claudeacrobat --pool work        # put everything in one pool
corrall import-claudeacrobat --from /srv/acrobat-state
```

| Flag | Effect |
| --- | --- |
| `--from DIR` | claudeacrobat's state directory. Default: the `state_dir` in `~/.config/claudeacrobat/config.json`, else `~/.local/state/claudeacrobat` |
| `--pool P` | Import everything into `P` (created if new) instead of the pool each account came from |
| `--dry-run` | Print the plan and write nothing |

Mapping, the inverse of claudeacrobat's own `import-teamclaude`:

| claudeacrobat | corrall |
| --- | --- |
| `kind: owned` (it holds and refreshes the tokens) | `type: oauth` with `accessToken` / `refreshToken` / `expiresAt` |
| `kind: linked` (tokens read live from Claude Code) | `type: oauth` with `importFrom` set to that credentials file |
| `accounts/` | the pool named by `defaultPool` |
| `pools/<name>/accounts/` | pool `<name>`, created if new |
| `profile`, `priority`, `disabled` | `accountUuid`, `orgUuid`, `orgName`, `email`, `subscriptionType`, `rateLimitTier`, `priority`, `disabled` |

Without `--pool`, claudeacrobat's pool layout carries over as it stands: two
fleets it kept apart stay apart, because merging them would have both rotations
spending one account's quota without either knowing.

Re-running is safe. An account is matched by `accountUuid` (then by name) and
refreshed in place, keeping whatever corrall-only settings it had — a route,
a `modelMap`, its own `upstream`. Anything with no corrall equivalent is
reported rather than guessed at: an account with no token, a pool whose name is
outside [corrall's charset](#pools), an API-key or Codex account that already
holds the same name.

## Codex

Point the Codex CLI at the proxy in `~/.codex/config.toml`:

```toml
model_provider = "corrall"

[model_providers.corrall]
name = "corrall"
base_url = "http://127.0.0.1:3456/backend-api/codex"
wire_api = "responses"
```

or launch it behind the MITM proxy (`eval "$(corrall env)"`), which
intercepts `chatgpt.com` as soon as one Codex account is configured
(`ab.chatgpt.com`, OpenAI's telemetry host, is never intercepted). Codex and
Anthropic accounts rotate independently on one port.

## Updating

`corrall update --check` compares this binary with the latest GitHub
release. `corrall update` downloads the archive for this OS/arch through
the GitHub CLI (the repository is private), verifies it against `SHA256SUMS`
and the Sigstore signature (when `cosign` is installed), swaps the binary
atomically beside the old one (kept as `corrall.prev`), restarts the
[service](#running-as-a-service) if it was running, and waits for `/corrall/health` (presenting the proxy key, so a server with `requireKeyOnLoopback` passes) on
the address `proxy.host` actually answers on (loopback for a wildcard bind);
the check is made by the binary itself, so `curl` is not required. If the new
binary does not come up healthy the previous one is restored and restarted. The implicit "latest" path never downgrades and refuses a new major
version without `--allow-major`; `--version vX.Y.Z` installs a specific tag on
purpose.

The server checks once a day (`updateCheck`, default on; or
`CORRALL_DISABLE_UPDATE_CHECK=1`) and only *reports* a newer release in
`corrall status`, the TUI header and `/corrall/status` (`updateAvailable`).
While the server's first check is still pending (it runs shortly after
start), `corrall status` does one on demand so a freshly started proxy
still shows an available update; the result is cached for an hour in
`corrall.update-check.json` next to the config. The same flag and variable
disable both checks. Nothing is ever installed unattended.
