# Corrall (Rust)

Multi-account Claude proxy with automatic quota-based rotation for
[Claude Code](https://claude.ai/claude-code), rewritten in Rust with the
security findings from a review of the original Node.js implementation fixed
at the design level.

It sits between Claude Code and the Anthropic API, holds several Claude Max
(or API key) accounts, and moves to the next one when the current account gets
close to its 5-hour or weekly limit. The session keeps running instead of
stopping on a 429.

This is a from-scratch port of [KarpelesLab/teamclaude](https://github.com/KarpelesLab/teamclaude)
(MIT), published as TeamClaude until it was renamed to Corrall. The config
file format is compatible: the installer renames an existing
`~/.config/teamclaude.json` for you, see
[Migrating from TeamClaude](#migrating-from-teamclaude).

## Quick start

Install a signed release binary (the repository is private, so downloads go
through the GitHub CLI):

```bash
gh auth login                # once
gh api repos/NodeSpy/corrall/contents/scripts/install.sh -H "Accept: application/vnd.github.raw" | bash
```

The installer verifies the checksum (and the Sigstore signature and build
provenance when `cosign` / `gh attestation` are available), installs to
`~/.local/bin/corrall`, and, if a TeamClaude install is present (this project
before the rename, or the original Node.js one), migrates it in place: it backs
up the config, stops the `systemd --user` unit, renames the config, state and
MITM certificate files, replaces the unit, removes the old binary and the npm
package, then starts the new binary and waits for it to be healthy. Any failure
rolls back automatically, and `scripts/install.sh --rollback` does so on
demand. `--dry-run` prints the plan.

Or build from source:

```bash
cargo install --git https://github.com/NodeSpy/corrall --locked
```

Then:

```bash
corrall login       # browser OAuth, once per account
corrall server      # start the proxy; shows the TUI on a terminal
corrall run         # in another terminal: Claude Code through the proxy
```

Already logged into Claude Code? `corrall import` copies its credentials.
`corrall import --link` keeps reading them from Claude Code's own store on
every reload instead, so a `/login` there is picked up automatically.

Coming from [claudeacrobat](#coming-from-claudeacrobat)? `corrall
import-claudeacrobat` brings its accounts and pools across.

## Migrating from TeamClaude

Corrall is the same program under a new name, and everything that carried the
old name moves with it:

| TeamClaude | Corrall |
| --- | --- |
| `~/.local/bin/teamclaude` | `~/.local/bin/corrall` |
| `~/.config/teamclaude.json`, `teamclaude.state.json`, `teamclaude.lock` | `corrall.json`, `corrall.state.json`, `corrall.lock` |
| `teamclaude-ca.pem`, `teamclaude-leaf.pem`, `teamclaude-leaf.key` | `corrall-ca.pem`, `corrall-leaf.pem`, `corrall-leaf.key` (same CA, so nothing to re-trust) |
| `teamclaude.service` | `corrall.service` |
| `TEAMCLAUDE_CONFIG`, `TEAMCLAUDE_HOST`, `TEAMCLAUDE_LOG`, every other `TEAMCLAUDE_*` | `CORRALL_*` |
| `/teamclaude/status`, `/teamclaude/health`, … | `/corrall/status`, `/corrall/health`, … |
| `teamclaude_requests_total` and the other metrics | `corrall_*` |
| `NodeSpy/teamclaude` releases, `teamclaude-vX-<triple>.tar.gz` | `NodeSpy/corrall`, `corrall-vX-<triple>.tar.gz` |

`TC_ACCT`, `TC_POOL` and the `/tc-acct/` pin prefix are unchanged. The
`@karpeleslab/teamclaude` npm package and `KarpelesLab/teamclaude` keep their
names: they are the original project this one was ported from.

Run the installer from the quick start. It detects a TeamClaude install and
performs the whole move, with `--dry-run` showing exactly what it would touch
and `--rollback` undoing it. `teamclaude update` cannot cross the rename: it
looks for release assets under the old name. Afterwards update anything of
yours that spelled the old name, which the installer lists: shell wrappers
(`eval "$(corrall env)"`), `TEAMCLAUDE_*` environment variables, dashboards
and health checks on `/teamclaude/*`, Prometheus rules on `teamclaude_*`, and a
Codex `model_provider` you may have named `teamclaude`.

To migrate by hand instead, stop the old server, rename the files in the table
above, and reinstall the unit with `corrall service install`. A `corrall`
binary that finds `teamclaude.json` but no `corrall.json` refuses to start
rather than create an empty config beside your accounts.

## What it does

- Named pools: several independent fleets on one port, each with its own
  accounts, thresholds, routes and sessions, addressed by a `/pool/<name>`
  prefix on the base URL. An install with one pool is unaffected.
- Pools can select themselves from the launch context — working directory, git
  remote or an environment variable — so one wrapper puts every project on the
  right fleet without knowing about any of them.
- Rotates to the next account when the 5h session or 7d weekly bucket reaches
  the switch threshold (98% by default), preferring the lowest `priority` and,
  among equals, the account whose weekly window resets soonest.
- Tracks the per-model weekly cap (Fable, Sonnet) separately, so an account out
  of Fable quota still serves Opus and Sonnet.
- Tells a spent quota bucket (`unified-*-status: rejected`) apart from a
  per-minute rate limit. Only the first one rotates; the second is absorbed
  inline on the same account, with one failover hop onto an idle sibling and
  never a second: a 429 that follows the request onto another account is
  returned to the client instead of being spread across the fleet.
- Paces requests onto a freshly switched account (storm control) so a herd of
  agents failing over together cannot cascade down the fleet.
- Catches hardcoded `api.anthropic.com` endpoints through a local MITM forward
  proxy with a locally minted CA, not only what `ANTHROPIC_BASE_URL` covers.
- Holds the request open until quota resets instead of returning 429 when every
  account is spent (`holdSeconds`, off by default).
- Refreshes OAuth tokens before they expire, coalescing concurrent refreshes,
  and writes them back to the config atomically.
- Keeps the claude.ai connectors working: the connector list is relayed with
  Claude Code's own login, the connector hosts are tunnelled in MITM mode, and
  the proxy key travels in a header of its own rather than as
  `ANTHROPIC_API_KEY`, which would switch the connectors off. See
  [docs/configuration.md](docs/configuration.md#claudeai-connectors).
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
- Browser dashboard at `/corrall/dashboard`, plus `/corrall/status`,
  `/corrall/quota`, `/corrall/metrics` (Prometheus) and `/corrall/health`.

## Everyday commands

```bash
corrall accounts -v          # accounts with tier and token status
corrall status               # live proxy status (needs a running server)
corrall status --json
corrall switch <name>        # make the server prefer one account
corrall disable <name>       # pause an account without removing it
corrall priority <name> 1    # rotation order, lower = preferred
corrall threshold 90         # switch at 90% (or: threshold unified7d=90)
corrall distribute on        # spread sessions across equal-priority accounts
corrall probe 300            # background quota probe every 300s (zero-spend)
corrall warmup 600           # keep idle accounts' 5h windows running (spends a little)
corrall expiry on            # expiry-pressure routing (--tolerance 1.5 --preempt on)
corrall titles on            # name activity rows after the Claude Code session
corrall login --codex        # add an OpenAI Codex subscription
corrall import --codex       # or import the Codex CLI's login
corrall import-claudeacrobat --dry-run   # accounts from a claudeacrobat install
corrall route add fable --match '*fable*' --accounts personal-max
corrall pool list            # pools with their accounts and settings
corrall pool add work        # a second fleet, empty
corrall pool set work --threshold 90 --hold 120
corrall pool set work --account spare@example.com   # move an account in
corrall pool set work --match-path ~/Projects/acme  # auto-select it there
corrall pool set work --match-remote '(?i)acme/'    # ...or by git remote
corrall login --pool work    # add an account to that pool
corrall env --pool work      # export lines pointing at that pool
corrall env                  # export lines for eval "$(corrall env)"; keeps Claude Code in first-party mode (deferred tool loading on)
corrall env --api-key        # the pre-connectors form: ANTHROPIC_API_KEY, connectors off
corrall ca-path              # where the MITM CA certificate lives
corrall config check         # validate and print a redacted config
corrall server --listen 127.0.0.1:3457   # bind elsewhere for one run
corrall update --check       # is there a newer release?
corrall update               # install it and restart the service
corrall service install      # systemd --user unit (Linux)
corrall --help
```

Every account-changing command notifies a running server to reload; there is
also `POST /corrall/reload`.

Every account command takes `--pool <name>` (or `TC_POOL` in the environment)
and defaults to the pool named by `defaultPool`, so nothing has to change until
a second pool exists. `<name>` may also be an account's id, e-mail or uuid; a
handle that fits more than one account (two orgs sharing an e-mail) is refused
rather than guessed, so give the full name or the id. `corrall pool rm` refuses a pool that still holds
accounts unless `--force` is given.

With `--match-path` / `--match-remote` / `--match-env` rules in place, the
usual wrapper needs no per-project cases at all:

```bash
eval "$(corrall env)"; exec claude "$@"
```

`env` matches the launch directory (or `--cwd DIR`) against every non-default
pool in sorted name order, first match wins, default pool otherwise. Exports go
to stdout; when a rule fires, the pool and the reason go to stderr
(`[Corrall] pool "work" (path ~/Projects/acme)`). With no rules configured
it stays silent and emits exactly what it always did. `--pool` and `TC_POOL`
skip matching. See [docs/configuration.md](docs/configuration.md#auto-selecting-a-pool).

## How it works

1. Claude Code talks to the local proxy instead of `api.anthropic.com`, either
   through `ANTHROPIC_BASE_URL` or through `HTTPS_PROXY` plus the local CA
   (`corrall run` and `corrall env` set both up).
2. A `/pool/<name>` prefix on the base URL picks the fleet that serves the
   request; without one it is the pool named by `defaultPool`. The prefix is
   stripped before forwarding, and an unknown pool falls back to the default
   rather than failing. In MITM mode there is no local URL to carry it, so the
   pool rides in the proxy username next to the optional account pin
   (`http://[<pin>]~<pool>:@127.0.0.1:3456`); `corrall env --pool` writes
   whichever form applies.
3. The proxy picks an eligible account from that pool, injects the account's
   real token and rewrites `account_uuid` in the request body to match.
4. `anthropic-ratelimit-unified-*` response headers feed the session (5h) and
   weekly (7d) quota view, which is persisted to `corrall.state.json` and
   survives a restart.
5. At the threshold, rotation moves on. On a quota 429 the request is resent on
   another account of the same pool, so the client never sees the limit while
   some account still has headroom.
6. Expiring tokens, transient upstream errors, 401s and organization OAuth
   denials are handled inside the proxy and never interrupt the session.

## Configuration

Config lives at `~/.config/corrall.json` (`$XDG_CONFIG_HOME` and
`$CORRALL_CONFIG` honoured). It is written `0600` and atomically; unknown
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
      "match": { "paths": ["~/Projects/acme"], "remotes": ["(?i)^git@github\\.com:acme/"] },
      "accounts": [
        { "name": "work@example.com", "type": "oauth", "accessToken": "sk-ant-oat01-…" }
      ]
    }
  }
}
```

## Coming from claudeacrobat

[claudeacrobat](https://github.com/EdnitionCode/claudeacrobat) is the sibling Go
proxy; the two share this design and can run side by side on different ports.
`corrall import-claudeacrobat` brings its accounts across:

```bash
corrall import-claudeacrobat --dry-run   # what would land where
corrall import-claudeacrobat             # keep its pool layout
corrall import-claudeacrobat --pool work # or put everything in one pool
```

An account claudeacrobat owns arrives with its tokens; one it reads live from
Claude Code arrives as an `importFrom` pointing at the same file. Its pool
layout carries over unless `--pool` overrides it, re-running refreshes rather
than duplicates, and anything that cannot map is listed with the reason.
claudeacrobat's own files are only read, so it keeps working afterwards. Details
in [docs/configuration.md](docs/configuration.md#importing-from-claudeacrobat).

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
- **No secret in the process tree.** `corrall run`/`env` put the proxy key in
  the environment only when the proxy is bound off-loopback; on loopback the
  account pin and pool name travel alone.
- **OAuth callback hardened.** The login listener binds loopback only and checks
  `state` before trusting an `error` parameter.
- **No unattended updates.** `corrall update` is explicit, verified against the signed checksums, fenced against downgrades and major jumps, and rolls back if the restarted server is unhealthy. The daily check only reports.
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

Added: named pools with `/pool/<name>` routing and launch-context
auto-selection, Prometheus metrics, health endpoint, JSON logs
(`--log-format json`),
`config check`, `import --link`, `requireKeyOnLoopback`, `maxBodyBytes`,
tunnel allow-lists, graceful shutdown with state persistence, signed release
builds with provenance attestations, a verifying `update` subcommand, and the
security changes above.

Not ported (by choice): the unattended daily self-update (`update` is manual
and verifying), the sx.org residential egress
integration, the egress-IP guard, the remote TUI (`attach`), shell alias
installation, the launchd service file, warm-up wall-clock schedules (interval
mode only), and the Nix packaging.

## Paseo plugin

`paseo-plugin/` is a [Paseo](https://paseo.sh) plugin (Paseo ≥ 0.8) that shows
the same account usage as the TUI inside the Paseo app — one card per account
with gradient `Session (5h)` / `Weekly (7d)` / per-family bars, `F✓ S✓ O✓`
family flags, a spend line, plus per-account and per-pool controls.

### Install

Enable plugins in Paseo first — **Settings → Plugins → Enable plugins**, or set
`pluginsEnabled: true` in Paseo's `config.json` and run `paseo reload --json` —
then install from this repo:

```sh
paseo plugin add NodeSpy/corrall:paseo-plugin
```

The `:paseo-plugin` suffix points at the plugin's subdirectory. The repo is
private, so the machine running `paseo plugin add` needs Git access to
`NodeSpy/corrall` (an SSH key, or `gh auth login`) — the same requirement as the
binary installer. Paseo clones the repo, runs the plugin's build step
(`npm ci`, which pulls its one bundled dependency), and loads it. Watch the
daemon-side log with `paseo plugin logs corrall`.

To develop against a local checkout instead of the Git source:

```sh
cd paseo-plugin
npm install && npm run typecheck
paseo plugin install "$PWD"          # trust & load this directory
paseo plugin reload corrall          # after each edit
```

There is nothing else to configure: the plugin's daemon-side handler reads
Corrall's own `corrall.json` (`$XDG_CONFIG_HOME/corrall.json`, or
`$CORRALL_CONFIG` — the same file the CLI and TUI use) for the proxy `port` and
`apiKey`, then polls `GET /corrall/status` with the key in `x-api-key`. If
`corrall server` is running locally, the plugin finds it.

### Update

Pull the newest version and rebuild it in place with:

```sh
paseo plugin update corrall
```

That fetches the latest commit, re-runs the build step and reloads the plugin
(`paseo plugin logs corrall` shows the reload; `paseo plugin ls` shows the
commit it is on). If you installed from a **local checkout** instead, `git pull`
and then `paseo plugin reload corrall` (run `npm install` first if dependencies
changed).

Updating the **plugin** and updating the **daemon** are separate steps: the
plugin ships the app UI, while account and pool **management** also needs the
`corrall` binary to carry the matching control routes. Update the daemon with
`corrall update` (see [Updating](#updating)).

> **Daemon version.** The usage view works against any daemon that serves
> `GET /corrall/status`. The management actions — enable / disable / priority,
> create / edit pools, and add / re-login accounts — need a daemon built with
> the newer control routes (`/corrall/pools…`, `/corrall/login/…`); if those
> actions return 404, update the daemon (`corrall update`, or rebuild from
> source) and restart it.

- **Settings** — a "Corrall" screen under Paseo → Settings listing every pool
  and account with the TUI's colour rules (green→yellow→red bars; solid red when
  a window is exhausted). Also opened by the composer pill, the `/corrall`
  command and the command centre.
- **Actions** — Enable / Disable and priority up/down on each account, wired to
  `POST /corrall/pools/{pool}/accounts/{id}/…`. Changes are written to
  `corrall.json` and the running fleet is re-synced, exactly as the CLI's
  `corrall enable` / `disable` / `priority` do.
- **Add / re-login accounts** — "Add account" on each pool header, and
  "Re-login" on each account (highlighted when it needs a fresh login), run the
  same OAuth flow as `corrall login --token`, over the control routes
  (`POST /corrall/login/{start,submit,cancel}`). Because the app is usually
  remote from the daemon, it uses the **manual** flow: the plugin gives you a
  sign-in link to open, and you paste the `code#state` the redirect page shows
  back into the modal. Tokens are exchanged daemon-side and never pass through
  the app.
- **Create & edit pools** — "New pool" and each pool's **Edit** button post to
  `POST /corrall/pools` / `POST /corrall/pools/{name}`. Corrall has no single
  "balancing strategy": a pool's rotation is its **switch threshold** plus
  whether **sessions are distributed** (and per-account priority), so the forms
  edit those directly. Renaming a pool moves its config and re-keys its live
  fleet; it resets that pool's in-flight sessions and quota history, so update
  any `ANTHROPIC_BASE_URL` that pins `/pool/<name>`. Full pool tuning (match
  rules, probe interval) stays a CLI job (`corrall pool set`).
- **Composer pill** — a "corrall" pill (gauge icon) sits on every agent's
  composer. Tapping it opens a compact popover tabulating each account's 5h / 7d
  usage with a **Manage accounts & pools** button that jumps to the Settings
  screen. `/corrall` also opens the surface.
- **States** — the surface shows "not configured" when no config file exists and
  "not reachable" when the daemon is down, instead of erroring.

The plugin is unsandboxed and trusted, like every Paseo plugin: its server code
runs in the Paseo daemon and its client code in the app.

## Testing

`cargo test` runs the unit tests and an integration suite (`tests/proxy.rs`)
that drives the proxy in-process against a mock upstream speaking the
Anthropic wire shape. Each integration test corresponds to a behaviour the
original project learned from a real incident: quota-vs-rate-limit 429s,
family-bucket diversion, storm control, session distribution, pins, credential
stripping, passthrough, WebSocket relay, MITM interception and the auth gate.

What is **not** covered: the OAuth login and refresh flows against the real
Anthropic and OpenAI endpoints. Those need a real account; run
`corrall login` and `corrall api /api/oauth/profile` to exercise them.

## Building

```bash
cargo build --release          # binary at target/release/corrall
cargo test
cargo clippy --all-targets -- -D warnings
```

Rust 1.80 or newer. No OpenSSL: TLS is rustls with the ring provider.

## Updating

```bash
corrall update --check     # is there a newer release?
corrall update             # verify, swap, restart, health-check (rolls back on failure)
```

`update` fetches the release through the GitHub CLI, verifies the archive
against `SHA256SUMS` and its Sigstore signature, replaces the binary with an
atomic rename (keeping the old one as `corrall.prev`), restarts the
`systemd --user` unit if it was running, and waits for the health endpoint on
the configured listener address (no `curl` needed). It
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
  --certificate-identity-regexp 'github.com/NodeSpy/corrall' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com SHA256SUMS
gh attestation verify corrall-*.tar.gz --repo NodeSpy/corrall
```

## License

MIT. See [LICENSE](LICENSE). Derived from TeamClaude by KarpelesLab.
