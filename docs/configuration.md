# Configuration

Path: `$TEAMCLAUDE_CONFIG`, else `$XDG_CONFIG_HOME/teamclaude.json`, else
`~/.config/teamclaude.json`. Written `0600`, atomically. Unknown keys are kept.

Runtime state (observed quota, usage counters) goes to `teamclaude.state.json`
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
`expiryRouting`, `match`. Everything else in the table below is daemon-global.

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

`teamclaude env --pool work` emits the right form for whichever mode is
configured. The default pool is never named in either form, so a one-pool
install emits byte-for-byte what it emitted before pools existed.

### Auto-selecting a pool

A pool can carry a `match` block, and then `teamclaude env` / `teamclaude run`
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
$ eval "$(teamclaude env)"
[TeamClaude] pool "work" (path ~/Projects/acme)
```

`teamclaude env --cwd DIR` matches against `DIR` instead of the current
directory, for a wrapper resolving a project it has not entered yet.

## Top level

| Field | Default | Description |
| --- | --- | --- |
| `proxy.port` | `3456` | Local port |
| `proxy.host` | `127.0.0.1` | Bind address. Anything non-loopback requires `proxy.apiKey` of at least 16 chars; the server refuses to start otherwise. `TEAMCLAUDE_HOST` overrides |
| `proxy.apiKey` | generated | Key clients present via `x-api-key` (or `Authorization: Bearer tc-…`, or the Basic password on CONNECT) |
| `proxy.clientKeys` | `[]` | `[{ "name", "key" }]`; usage is attributed to `name` |
| `proxy.requireKeyOnLoopback` | `false` | Require the key even from 127.0.0.1. Recommended on shared hosts |
| `proxy.sessionDetail` | `false` | Include a per-session breakdown in `/teamclaude/status` |
| `proxy.usageDimensions` | `[]` | `[{ "name", "header" }]`: request headers consumed by the proxy for attribution (not forwarded) |
| `proxy.maxBodyBytes` | `67108864` | Largest client request body accepted |
| `upstream` | `https://api.anthropic.com` | Upstream base URL. Must be https unless loopback |
| `switchThreshold` | `0.98` | Number, or table `{ "default": 0.98, "unified7d": 0.9, … }` keyed by bucket |
| `holdSeconds` | `0` | Hold a request this long when all accounts are exhausted instead of returning 429 |
| `distributeSessions` | `false` | Spread new sessions across equal-priority accounts, pinned per weekly bucket |
| `quotaProbeSeconds` | `0` | Background zero-spend usage probe interval (min 30) |
| `eventLogging` | `hide` | Claude Code telemetry: `hide` (forward, not shown), `block` (answer 200 locally), `show` |
| `blockedModels` | `[]` | Globs of models rejected with a fast 400 |
| `routes` | `[]` | `[{ "name", "match": [globs], "accounts": [names], "bucket"?, "color"? }]`, first match wins |
| `stormRamp` | on | `{ "enabled", "startConc": 1, "stepConc": 1, "stepMs": 250, "windowMs": 30000 }` |
| `expiryRouting` | off | `{ "enabled", "tolerance": 1.5, "preempt": true }`: rank the top priority tier by headroom ÷ seconds-to-reset of the governing weekly bucket, keep accounts within `tolerance` of the best, and with `preempt` re-rank the sticky/pinned account when its window rolls over |
| `warmupSeconds` | `0` | Keep-warm interval (min 60). Spawns `claude -p --bare --model haiku` per idle account through this proxy; spends a little quota |
| `sessionTitles` | off | `{ "enabled", "width": 18, "projectsDir"? }`: label activity rows with the Claude Code session title read from `~/.claude/projects` |
| `mitm.http1Only` | `true` | Offer only HTTP/1.1 inside the intercepted tunnel (needed for WebSocket / Remote Control) |
| `mitm.allowTunnel` | `false` | Allow blind CONNECT tunnels to non-intercepted hosts (port 443, public addresses only) |
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
| `importFrom` | Read tokens from this file (Claude Code's `~/.claude/.credentials.json`) on every reload instead of storing them |
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
| `TC_ACCT` | Pin `teamclaude run` / `env` to one account (uuid, org uuid, `uuid/org`, name or email). Removed from the child environment |
| `TC_POOL` | Send `teamclaude run` / `env` to one pool, the same way `TC_ACCT` chooses an account. `--pool` wins over it, and either skips `match` rules. Removed from the child environment |
| `TEAMCLAUDE_CONFIG` | Config path |
| `TEAMCLAUDE_HOST` | Override `proxy.host` |
| `TEAMCLAUDE_LOG` | `tracing` filter, e.g. `debug` |
| `TEAMCLAUDE_UPSTREAM_HEADERS_TIMEOUT_MS` | Time to first byte, default 120000 |
| `TEAMCLAUDE_UPSTREAM_BODY_TIMEOUT_MS` | Idle gap between body chunks, default 120000 |
| `TEAMCLAUDE_UPSTREAM_MAX_SOCKETS` | Idle pooled connections per host, default 256 |
| `TEAMCLAUDE_REFRESH_TIMEOUT_MS` | OAuth refresh timeout, default 30000 |
| `TEAMCLAUDE_RATE_LIMIT_ABSORB_MAX_SECONDS` | Longest `retry-after` absorbed inline when holding, default 60 |
| `TEAMCLAUDE_FAMILY_STALE_MS` | How long a spent family (Fable/Sonnet) reading is trusted before revalidation, default 1800000 |
| `TEAMCLAUDE_REPO` | `owner/repo` `teamclaude update` fetches releases from, default `NodeSpy/teamclaude` (the same knob `scripts/install.sh` reads) |

## Control endpoints

All under `http://127.0.0.1:<port>/teamclaude/`, authenticated like any other
request. `POST` bodies are capped at 64 KiB. Browser-originated requests are
refused.

| Endpoint | Description |
| --- | --- |
| `GET health` | `{ ok, version }` |
| `GET dashboard` | Static HTML page; asks for the key and polls `status` |
| `GET status` | Full account, quota, route, session and client-usage view. One entry per pool under `pools`; the default pool's view is also flattened onto the top level |
| `GET quota` | Tier-weighted fleet quota for status lines, for the addressed pool |
| `GET metrics` | Prometheus text format. Per-account series carry a `pool="…"` label |
| `POST reload` | Re-read the config |
| `POST switch` `{ "account": "…", "pool"? }` | Prefer one account. Without a pool, every pool is searched |
| `POST route-pin` `{ "route": "…", "account": "…", "pool"? }` | Pin a route (omit `account` to clear) |

A `/pool/<name>` prefix on the control path selects the pool too, so
`/pool/work/teamclaude/quota` reads the `work` fleet.

## Passthrough paths

`/v1/oauth/token`, `/api/oauth/*` and `/v1/code/*` (Remote Control, including
its WebSocket upgrade) are relayed to the Anthropic upstream with the client's
own `authorization` header and no account selection. Hop-by-hop headers and the
proxy key are stripped.

## Codex

Point the Codex CLI at the proxy in `~/.codex/config.toml`:

```toml
model_provider = "teamclaude"

[model_providers.teamclaude]
name = "teamclaude"
base_url = "http://127.0.0.1:3456/backend-api/codex"
wire_api = "responses"
```

or launch it behind the MITM proxy (`eval "$(teamclaude env)"`), which
intercepts `chatgpt.com` as soon as one Codex account is configured
(`ab.chatgpt.com`, OpenAI's telemetry host, is never intercepted). Codex and
Anthropic accounts rotate independently on one port.
