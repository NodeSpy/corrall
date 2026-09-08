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

Install a signed release binary (the repository is private, so downloads go
through the GitHub CLI):

```bash
gh auth login                # once
gh api repos/NodeSpy/teamclaude/contents/scripts/install.sh -H "Accept: application/vnd.github.raw" | bash
```

The installer verifies the checksum (and the Sigstore signature and build
provenance when `cosign` / `gh attestation` are available), installs to
`~/.local/bin/teamclaude`, and, if the original Node.js TeamClaude is present,
swaps over in place: it backs up the config, stops the `systemd --user` unit,
removes the npm package, starts the new binary under the same unit and waits
for it to be healthy. Any failure rolls back automatically, and
`scripts/install.sh --rollback` does so on demand. `--dry-run` prints the plan.

Or build from source:

```bash
cargo install --git https://github.com/NodeSpy/teamclaude --locked
```

Then:

```bash
teamclaude login       # browser OAuth, once per account
teamclaude server      # start the proxy; shows the TUI on a terminal
teamclaude run         # in another terminal: Claude Code through the proxy
```

Already logged into Claude Code? `teamclaude import` copies its credentials.
`teamclaude import --link` keeps reading them from Claude Code's own store on
every reload instead, so a `/login` there is picked up automatically.

## What it does

- Named pools: several independent fleets on one port, each with its own
  accounts, thresholds, routes and sessions, addressed by a `/pool/<name>`
  prefix on the base URL. An install with one pool is unaffected.
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
- OpenAI Codex subscriptions pooled alongside Claude accounts (`login --codex`,
  `import --codex`), rotating independently on the same port.
- Expiry-pressure routing (`expiry on`): prefer the account whose ample weekly
  quota is about to be forfeited, and re-rank when a window rolls over.
- Keep-warm (`warmup N`), quota probe (`probe N`), session titles from Claude
  Code's own files (`titles on`), usage dimensions for per-project attribution.
- Client-side OAuth token refresh and Remote Control (`/v1/code/*`, including
  its WebSocket) pass through untouched with the client's own credentials.
- Browser dashboard at `/teamclaude/dashboard`, plus `/teamclaude/status`,
  `/teamclaude/quota`, `/teamclaude/metrics` (Prometheus) and `/teamclaude/health`.

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
teamclaude warmup 600           # keep idle accounts' 5h windows running (spends a little)
teamclaude expiry on            # expiry-pressure routing (--tolerance 1.5 --preempt on)
teamclaude titles on            # name activity rows after the Claude Code session
teamclaude login --codex        # add an OpenAI Codex subscription
teamclaude import --codex       # or import the Codex CLI's login
teamclaude route add fable --match '*fable*' --accounts personal-max
teamclaude pool list            # pools with their accounts and settings
teamclaude pool add work        # a second fleet, empty
teamclaude pool set work --threshold 90 --hold 120
teamclaude pool set work --account spare@example.com   # move an account in
teamclaude login --pool work    # add an account to that pool
teamclaude env --pool work      # export lines pointing at that pool
teamclaude env                  # export lines for eval "$(teamclaude env)"
teamclaude ca-path              # where the MITM CA certificate lives
teamclaude config check         # validate and print a redacted config
teamclaude service install      # systemd --user unit (Linux)
teamclaude --help
```

Every account-changing command notifies a running server to reload; there is
also `POST /teamclaude/reload`.

Every account command takes `--pool <name>` (or `TC_POOL` in the environment)
and defaults to the pool named by `defaultPool`, so nothing has to change until
a second pool exists. `teamclaude pool rm` refuses a pool that still holds
accounts unless `--force` is given.

## How it works

1. Claude Code talks to the local proxy instead of `api.anthropic.com`, either
   through `ANTHROPIC_BASE_URL` or through `HTTPS_PROXY` plus the local CA
   (`teamclaude run` and `teamclaude env` set both up).
2. A `/pool/<name>` prefix on the base URL picks the fleet that serves the
   request; without one it is the pool named by `defaultPool`. The prefix is
   stripped before forwarding, and an unknown pool falls back to the default
   rather than failing. In MITM mode there is no local URL to carry it, so the
   pool rides in the proxy username next to the optional account pin
   (`http://[<pin>]~<pool>:@127.0.0.1:3456`); `teamclaude env --pool` writes
   whichever form applies.
3. The proxy picks an eligible account from that pool, injects the account's
   real token and rewrites `account_uuid` in the request body to match.
4. `anthropic-ratelimit-unified-*` response headers feed the session (5h) and
   weekly (7d) quota view, which is persisted to `teamclaude.state.json` and
   survives a restart.
5. At the threshold, rotation moves on. On a quota 429 the request is resent on
   another account of the same pool, so the client never sees the limit while
   some account still has headroom.
6. Expiring tokens, transient upstream errors, 401s and organization OAuth
   denials are handled inside the proxy and never interrupt the session.

## Configuration

Config lives at `~/.config/teamclaude.json` (`$XDG_CONFIG_HOME` and
`$TEAMCLAUDE_CONFIG` honoured). It is written `0600` and atomically; unknown
keys are preserved so hand edits are safe. A proxy API key is generated on
first use. See [docs/configuration.md](docs/configuration.md) for every field.

Accounts and rotation settings live inside a pool. A config written before
pools existed is migrated into `pools.default` on load and re-saved, so an
upgrade needs no manual work.

```json
{
  "proxy": { "port": 3456, "apiKey": "tc-…", "requireKeyOnLoopback": false },
  "upstream": "https://api.anthropic.com",
  "defaultPool": "default",
  "pools": {
    "default": {
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
    },
    "work": {
      "switchThreshold": 0.9,
      "accounts": [
        { "name": "work@example.com", "type": "oauth", "accessToken": "sk-ant-oat01-…" }
      ]
    }
  }
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
  logs are written with a temp file + rename at mode 0600. Config updates take
  a cross-process `flock` and re-read before writing, so a CLI command running
  while the server refreshes a token cannot clobber the rotated refresh token.
- **Safer MITM defaults.** Blind CONNECT tunnels to other hosts are off unless
  `mitm.allowTunnel` is set, and then refuse private addresses and non-443
  ports. The CA private key is never written to disk.
- **No secret in the process tree.** `teamclaude run`/`env` put the proxy key in
  the environment only when the proxy is bound off-loopback; on loopback the
  account pin and pool name travel alone.
- **OAuth callback hardened.** The login listener binds loopback only and checks
  `state` before trusting an `error` parameter.
- **No unattended updates.** `teamclaude update` is explicit, verified against the signed checksums, fenced against downgrades and major jumps, and rolls back if the restarted server is unhealthy. The daily check only reports.
- Constant-time key comparison; failed auth is delayed; internal error text is
  never echoed to clients; control characters are stripped from anything that
  reaches a terminal or a log.

Turn on `proxy.requireKeyOnLoopback` on any host where other users or untrusted
processes run.

## Compared with the original

Ported: rotation and threshold logic, per-model buckets, the two kinds of 429
with one failover hop, storm control, session tracking and distribution,
expiry-pressure routing with rollover preemption, routes and route pins,
`TC_ACCT` pinning in both modes, MITM proxy with a local CA, hold on
exhaustion, token refresh with the dead-token guard, quota probe, keep-warm,
session titles, `importFrom`, Codex accounts, third-party backends with
`modelMap`/`stripRequestFields`, request logging with bounds and retention,
per-client keys, usage dimensions, client token-refresh and Remote Control
passthrough (including the WebSocket), the state file, the browser dashboard,
the TUI, and the CLI.

Added: named pools with `/pool/<name>` routing, Prometheus metrics, health
endpoint, JSON logs (`--log-format json`),
`config check`, `import --link`, `requireKeyOnLoopback`, `maxBodyBytes`,
tunnel allow-lists, graceful shutdown with state persistence, signed release
builds with provenance attestations, and the security changes above.

Not ported (by choice): the self-updater, the sx.org residential egress
integration, the egress-IP guard, the remote TUI (`attach`), shell alias
installation, the launchd service file, warm-up wall-clock schedules (interval
mode only), and the Nix packaging.

## Testing

`cargo test` runs the unit tests and an integration suite (`tests/proxy.rs`)
that drives the proxy in-process against a mock upstream speaking the
Anthropic wire shape. Each integration test corresponds to a behaviour the
original project learned from a real incident: quota-vs-rate-limit 429s,
family-bucket diversion, storm control, session distribution, pins, credential
stripping, passthrough, WebSocket relay, MITM interception and the auth gate.

What is **not** covered: the OAuth login and refresh flows against the real
Anthropic and OpenAI endpoints. Those need a real account; run
`teamclaude login` and `teamclaude api /api/oauth/profile` to exercise them.

## Building

```bash
cargo build --release          # binary at target/release/teamclaude
cargo test
cargo clippy --all-targets -- -D warnings
```

Rust 1.80 or newer. No OpenSSL: TLS is rustls with the ring provider.

## Updating

```bash
teamclaude update --check     # is there a newer release?
teamclaude update             # verify, swap, restart, health-check (rolls back on failure)
```

`update` fetches the release through the GitHub CLI, verifies the archive
against `SHA256SUMS` and its Sigstore signature, replaces the binary with an
atomic rename (keeping the old one as `teamclaude.prev`), restarts the
`systemd --user` unit if it was running, and waits for the health endpoint. It
never downgrades or crosses a major version unless told to (`--version`,
`--allow-major`), refuses to overwrite a `cargo build` in a checkout, and never
runs unattended. The server checks daily and only *tells* you a release exists
(`Update` row in `status`, TUI header, `updateAvailable` in the status JSON);
turn that off with `"updateCheck": false`.

`scripts/install.sh` remains the first-install and swap-over path: it also
backs up config and state and rolls back the whole swap.

## Releases

Pushing a `v*` tag (or running the Release workflow by hand with a tag name)
builds static binaries for Linux (x86_64/aarch64, musl) and macOS
(x86_64/aarch64), publishes them as a GitHub release with `SHA256SUMS` signed
keylessly through Sigstore and a build-provenance attestation per archive.
`scripts/install.sh` consumes exactly these assets. To cut a release:

```bash
git tag -a v2.0.1 -m "v2.0.1" && git push origin v2.0.1
```

Verify an archive by hand with:

```bash
cosign verify-blob --bundle SHA256SUMS.sigstore.json \
  --certificate-identity-regexp 'github.com/NodeSpy/teamclaude' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com SHA256SUMS
gh attestation verify teamclaude-*.tar.gz --repo NodeSpy/teamclaude
```

## License

MIT. See [LICENSE](LICENSE). Derived from TeamClaude by KarpelesLab.
