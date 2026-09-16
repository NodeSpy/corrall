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
| `quotaProbeSeconds` | `0` | Background zero-spend usage probe interval (min 30) |
| `eventLogging` | `hide` | Claude Code telemetry: `hide` (forward, not shown), `block` (answer 200 locally), `show` |
| `blockedModels` | `[]` | Globs of models rejected with a fast 400 |
| `routes` | `[]` | `[{ "name", "match": [globs], "accounts": [names], "bucket"?, "color"? }]`, first match wins |
| `stormRamp` | on | `{ "enabled", "startConc": 1, "stepConc": 1, "stepMs": 250, "windowMs": 30000 }` |
| `expiryRouting` | off | `{ "enabled", "tolerance": 1.5, "preempt": true }`: rank the top priority tier by headroom ÷ seconds-to-reset of the governing weekly bucket, keep accounts within `tolerance` of the best, and with `preempt` re-rank the sticky/pinned account when its window rolls over |
| `warmupSeconds` | `0` | Keep-warm interval (min 60). Spawns `claude -p --bare --model haiku` per idle account through this proxy; spends a little quota |
| `sessionTitles` | off | `{ "enabled", "width": 18, "projectsDir"? }`: label activity rows with the Claude Code session title read from `~/.claude/projects` |
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
| `TC_ACCT` | Pin `corrall run` / `env` to one account (uuid, org uuid, `uuid/org`, name or email). A handle that fits several accounts (an e-mail shared by two orgs, say) is refused; use the full name or id. Removed from the child environment |
| `TC_POOL` | Send `corrall run` / `env` to one pool, the same way `TC_ACCT` chooses an account. `--pool` wins over it, and either skips `match` rules. Removed from the child environment |
| `CORRALL_CONFIG` | Config path |
| `CORRALL_HOST` | Override `proxy.host` |
| `CORRALL_LOG` | `tracing` filter, e.g. `debug` |
| `CORRALL_UPSTREAM_HEADERS_TIMEOUT_MS` | Time to response headers, default 120000. Not a total deadline: a streamed response runs as long as chunks keep arriving |
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
| `POST reload` | Re-read the config |
| `POST switch` `{ "account": "…", "pool"? }` | Prefer one account. Without a pool, every pool is searched |
| `POST route-pin` `{ "route": "…", "account": "…", "pool"? }` | Pin a route (omit `account` to clear) |

A `/pool/<name>` prefix on the control path selects the pool too, so
`/pool/work/corrall/quota` reads the `work` fleet.

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
       "ANTHROPIC_CUSTOM_HEADERS": "x-corrall-key: tc-…"
     }
   }
   ```

   `corrall env` and `corrall run` emit exactly this (and never
   `ANTHROPIC_API_KEY`).
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

### Choosing a mode

Both ways of handing Claude Code the proxy key keep working. The difference is
what Claude Code does with its own login:

| | `ANTHROPIC_API_KEY` | header (default) |
| --- | --- | --- |
| Launcher | `corrall env --api-key`, `corrall run --api-key` | `corrall env`, `corrall run` |
| Claude Code's auth source | the proxy key | its claude.ai login (`/login` once) |
| claude.ai connectors | off | on |
| Request size | smaller: local tools only | larger: every connected connector's tool schemas ride along |
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
every eligible account was tried and answered 5xx or timed out, gets a 502
`api_error` (504 when every failure was a timeout) with no `retry-after`: those
accounts are healthy, and their quota resets say nothing about when the upstream
will answer again. `holdSeconds` holds either case for its duration and retries
the whole fleet before answering.

## Per-minute 429s

A 429 without `anthropic-ratelimit-unified-*-status: rejected` is a per-minute
limit, not a spent quota. With a `retry-after` of 15s or less the proxy waits
and retries the same account; longer than that it fails over once. Without a
`retry-after` header it pauses the account 5s and retries it, rather than
assuming 60s. A request rate-limited on two accounts in a row is answered 429
with the upstream `retry-after` instead of trying a third: a limit that follows
the request across accounts is not per-account, and each further hop only
marked another account unavailable to every other session. Every such 429 is
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
`systemd --user` unit if it was running, and waits for `/corrall/health` on
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
