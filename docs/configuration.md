# Configuration

Path: `$TEAMCLAUDE_CONFIG`, else `$XDG_CONFIG_HOME/teamclaude.json`, else
`~/.config/teamclaude.json`. Written `0600`, atomically. Unknown keys are kept.

Runtime state (observed quota, usage counters) goes to `teamclaude.state.json`
beside it. Safe to delete; quota is re-learned from traffic.

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
| `TEAMCLAUDE_CONFIG` | Config path |
| `TEAMCLAUDE_HOST` | Override `proxy.host` |
| `TEAMCLAUDE_LOG` | `tracing` filter, e.g. `debug` |
| `TEAMCLAUDE_UPSTREAM_HEADERS_TIMEOUT_MS` | Time to first byte, default 120000 |
| `TEAMCLAUDE_UPSTREAM_BODY_TIMEOUT_MS` | Idle gap between body chunks, default 120000 |
| `TEAMCLAUDE_UPSTREAM_MAX_SOCKETS` | Idle pooled connections per host, default 256 |
| `TEAMCLAUDE_REFRESH_TIMEOUT_MS` | OAuth refresh timeout, default 30000 |
| `TEAMCLAUDE_RATE_LIMIT_ABSORB_MAX_SECONDS` | Longest `retry-after` absorbed inline when holding, default 60 |
| `TEAMCLAUDE_FAMILY_STALE_MS` | How long a spent family (Fable/Sonnet) reading is trusted before revalidation, default 1800000 |

## Control endpoints

All under `http://127.0.0.1:<port>/teamclaude/`, authenticated like any other
request. `POST` bodies are capped at 64 KiB. Browser-originated requests are
refused.

| Endpoint | Description |
| --- | --- |
| `GET health` | `{ ok, version }` |
| `GET dashboard` | Static HTML page; asks for the key and polls `status` |
| `GET status` | Full account, quota, route, session and client-usage view |
| `GET quota` | Tier-weighted fleet quota for status lines |
| `GET metrics` | Prometheus text format |
| `POST reload` | Re-read the config |
| `POST switch` `{ "account": "…" }` | Prefer one account |
| `POST route-pin` `{ "route": "…", "account": "…" }` | Pin a route (omit `account` to clear) |

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

## Updating

`teamclaude update --check` compares this binary with the latest GitHub
release. `teamclaude update` downloads the archive for this OS/arch through
the GitHub CLI (the repository is private), verifies it against `SHA256SUMS`
and the Sigstore signature (when `cosign` is installed), swaps the binary
atomically beside the old one (kept as `teamclaude.prev`), restarts the
`systemd --user` unit if it was running, and waits for `/teamclaude/health`.
If the new binary does not come up healthy the previous one is restored and
restarted. The implicit "latest" path never downgrades and refuses a new major
version without `--allow-major`; `--version vX.Y.Z` installs a specific tag on
purpose.

The server checks once a day (`updateCheck`, default on; or
`TEAMCLAUDE_DISABLE_UPDATE_CHECK=1`) and only *reports* a newer release in
`teamclaude status`, the TUI header and `/teamclaude/status` (`updateAvailable`).
Nothing is ever installed unattended.
