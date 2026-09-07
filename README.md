# TeamClaude (Rust)

Multi-account Claude proxy with automatic quota-based rotation for
[Claude Code](https://claude.ai/claude-code), rewritten in Rust with the
security findings from a review of the original Node.js implementation fixed
at the design level.

It sits between Claude Code and the Anthropic API, holds several Claude Max
(or API key) accounts, and moves to the next one when the current account gets
close to its 5-hour or weekly limit. The session keeps running instead of
stopping on a 429.

This is a from-scratch port of [KarpelesLab/teamclaude](https://github.com/KarpelesLab/teamclaude)
(MIT). The config file format is compatible, so an existing
`~/.config/teamclaude.json` works unchanged.

## Quick start

```bash
cargo install --git https://github.com/NodeSpy/teamclaude
# or: git clone … && cargo build --release && cp target/release/teamclaude ~/.local/bin/

teamclaude login       # browser OAuth, once per account
teamclaude server      # start the proxy; shows the TUI on a terminal
teamclaude run         # in another terminal: Claude Code through the proxy
```

Already logged into Claude Code? `teamclaude import` copies its credentials.
`teamclaude import --link` keeps reading them from Claude Code's own store on
every reload instead, so a `/login` there is picked up automatically.

## What it does

- Rotates to the next account when the 5h session or 7d weekly bucket reaches
  the switch threshold (98% by default), preferring the lowest `priority` and,
  among equals, the account whose weekly window resets soonest.
- Tracks the per-model weekly cap (Fable, Sonnet) separately, so an account out
  of Fable quota still serves Opus and Sonnet.
- Tells a spent quota bucket (`unified-*-status: rejected`) apart from a
  per-minute rate limit. Only the first one rotates; the second is absorbed
  inline on the same account, with one failover hop onto an idle sibling.
- Paces requests onto a freshly switched account (storm control) so a herd of
  agents failing over together cannot cascade down the fleet.
- Catches hardcoded `api.anthropic.com` endpoints through a local MITM forward
  proxy with a locally minted CA, not only what `ANTHROPIC_BASE_URL` covers.
- Holds the request open until quota resets instead of returning 429 when every
  account is spent (`holdSeconds`, off by default).
- Refreshes OAuth tokens before they expire, coalescing concurrent refreshes,
  and writes them back to the config atomically.
- Session affinity: with `distributeSessions` on, each Claude Code session is
  pinned per weekly bucket so its prompt cache stays warm while new sessions
  spread across equal-priority accounts.
- Any Anthropic-compatible API (DeepSeek, GLM, ...) as a low-priority fallback,
  with per-account model mapping and request-field stripping.
- Repairs orphaned `tool_use`/`tool_result` pairs so a compacted transcript
  cannot wedge a session with a non-retryable 400.
- TUI with quota bars, reset countdowns, live activity log, account switching,
  config reload and a one-shot quota probe.
- `/teamclaude/status`, `/teamclaude/quota`, `/teamclaude/metrics` (Prometheus)
  and `/teamclaude/health` endpoints.

## Everyday commands

```bash
teamclaude accounts -v          # accounts with tier and token status
teamclaude status               # live proxy status (needs a running server)
teamclaude status --json
teamclaude switch <name>        # make the server prefer one account
teamclaude disable <name>       # pause an account without removing it
teamclaude priority <name> 1    # rotation order, lower = preferred
teamclaude threshold 90         # switch at 90% (or: threshold unified7d=90)
teamclaude distribute on        # spread sessions across equal-priority accounts
teamclaude probe 300            # background quota probe every 300s (zero-spend)
teamclaude route add fable --match '*fable*' --accounts personal-max
teamclaude env                  # export lines for eval "$(teamclaude env)"
teamclaude ca-path              # where the MITM CA certificate lives
teamclaude config check         # validate and print a redacted config
teamclaude service install      # systemd --user unit (Linux)
teamclaude --help
```

Every account-changing command notifies a running server to reload; there is
also `POST /teamclaude/reload`.

## How it works

1. Claude Code talks to the local proxy instead of `api.anthropic.com`, either
   through `ANTHROPIC_BASE_URL` or through `HTTPS_PROXY` plus the local CA
   (`teamclaude run` and `teamclaude env` set both up).
2. The proxy picks an eligible account, injects that account's real token and
   rewrites `account_uuid` in the request body to match.
3. `anthropic-ratelimit-unified-*` response headers feed the session (5h) and
   weekly (7d) quota view, which is persisted to `teamclaude.state.json` and
   survives a restart.
4. At the threshold, rotation moves on. On a quota 429 the request is resent on
   another account, so the client never sees the limit while some account still
   has headroom.
5. Expiring tokens, transient upstream errors, 401s and organization OAuth
   denials are handled inside the proxy and never interrupt the session.

## Configuration

Config lives at `~/.config/teamclaude.json` (`$XDG_CONFIG_HOME` and
`$TEAMCLAUDE_CONFIG` honoured). It is written `0600` and atomically; unknown
keys are preserved so hand edits are safe. A proxy API key is generated on
first use. See [docs/configuration.md](docs/configuration.md) for every field.

```json
{
  "proxy": { "port": 3456, "apiKey": "tc-…", "requireKeyOnLoopback": false },
  "upstream": "https://api.anthropic.com",
  "switchThreshold": 0.98,
  "holdSeconds": 0,
  "distributeSessions": false,
  "quotaProbeSeconds": 0,
  "accounts": [
    { "name": "me@example.com", "type": "oauth", "importFrom": "~/.claude/.credentials.json" },
    { "name": "spare@example.com", "type": "oauth", "priority": 1,
      "accessToken": "sk-ant-oat01-…", "refreshToken": "sk-ant-ort01-…", "expiresAt": 1774384968427,
      "maxUsage": { "unified7d": 0.6 } },
    { "name": "deepseek", "type": "apikey", "priority": 100,
      "apiKey": "sk-…", "upstream": "https://api.deepseek.com/anthropic",
      "modelMap": { "claude-sonnet-4-6": "deepseek-v4-pro" },
      "stripRequestFields": ["context_management"] }
  ],
  "routes": [
    { "name": "fable", "match": ["*fable*"], "accounts": ["me@example.com"] }
  ]
}
```

## Security

This port was preceded by a security review of the original. The review and
what changed are in [docs/security-review.md](docs/security-review.md). In
short:

- **DNS rebinding defence.** The loopback exemption from the proxy key also
  requires a loopback `Host` header, and any browser-originated request
  (`Origin` / `Sec-Fetch-Site`) is refused on every path, not only the control
  plane.
- **Client credentials never travel upstream.** `authorization`, `x-api-key`
  and `cookie` from the client are stripped before the account credential is
  injected, on every path.
- **Subscription tokens only go to Anthropic.** An OAuth account configured with
  a third-party `upstream` is refused rather than handing a Claude Max token to
  that host.
- **Bounded everything.** Request bodies are capped (`proxy.maxBodyBytes`, 64
  MiB default), control bodies at 64 KiB, the session map at 10 000 entries
  with validated ids, request logs truncated and swept.
- **Atomic, private persistence.** Config, state, certificate keys and request
  logs are written with a temp file + rename at mode 0600 and a cross-process
  lock, so a crash mid-write cannot destroy every refresh token.
- **Safer MITM defaults.** Blind CONNECT tunnels to other hosts are off unless
  `mitm.allowTunnel` is set, and then refuse private addresses and non-443
  ports. The CA private key is never written to disk.
- **No secret in the process tree.** `teamclaude run`/`env` put the proxy key in
  the environment only when the proxy is bound off-loopback; on loopback the
  pin travels alone.
- **OAuth callback hardened.** The login listener binds loopback only and checks
  `state` before trusting an `error` parameter.
- **No self-updater.** Update deliberately requires an operator action.
- Constant-time key comparison; failed auth is delayed; internal error text is
  never echoed to clients; control characters are stripped from anything that
  reaches a terminal or a log.

Turn on `proxy.requireKeyOnLoopback` on any host where other users or untrusted
processes run.

## Compared with the original

Ported: rotation and threshold logic, per-model buckets, the two kinds of 429,
storm control, session tracking and distribution, routes and route pins,
`TC_ACCT` pinning (both modes), MITM proxy with local CA, hold on exhaustion,
token refresh with the dead-token guard, quota probe, `importFrom`, third-party
backends with `modelMap`/`stripRequestFields`, request logging with bounds and
retention, per-client keys and usage attribution, the state file, the TUI, and
the CLI.

Added: Prometheus metrics, health endpoint, JSON logs (`--log-format json`),
`config check`, `import --link`, `requireKeyOnLoopback`, `maxBodyBytes`,
tunnel allow-lists, graceful shutdown with state persistence, and the security
changes above.

Not ported (by choice or scope): the self-updater, the sx.org residential
egress integration, the egress-IP guard, keep-warm scheduling (it spends quota
by spawning `claude`), session titles from `~/.claude/projects`, the remote TUI
(`attach`), the browser dashboard page, shell alias installation, Codex/OpenAI
accounts, launchd service files, and the Nix packaging.

## Building

```bash
cargo build --release          # binary at target/release/teamclaude
cargo test
cargo clippy --all-targets -- -D warnings
```

Rust 1.80 or newer. No OpenSSL: TLS is rustls with the ring provider.

## License

MIT. See [LICENSE](LICENSE). Derived from TeamClaude by KarpelesLab.
