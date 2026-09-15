# Security review of the original TeamClaude and what this port changes

The Node.js implementation at `KarpelesLab/teamclaude` (v1.1.16, commit
`eed7b33`) was reviewed before this rewrite. The review covered the proxy
server and MITM path, credential/OAuth/state handling, and the CLI, updater,
service and CI files. Findings are listed with the severity assigned during the
review and the disposition in this Rust implementation.

Overall the original was careful: constant-time key compare, loopback bind by
default, PKCE with random state, 0600 on every config write, a CA key that is
never persisted, no TLS verification disabled anywhere, credentials excluded
from status output, and control-character stripping at the log boundary. The
items below are the gaps.

## Proxy server and MITM

| # | Severity | Finding (original) | Disposition here |
| --- | --- | --- | --- |
| 1 | High | Loopback trust keyed only on the socket address; a DNS-rebinding page resolving to 127.0.0.1 passes both the key gate and the `Sec-Fetch-Site: same-origin` CSRF check, gaining readable status, account switching, and free inference with injected fleet tokens. A plain `no-cors` POST to `/v1/messages` also lands because the CSRF gate covered only `/teamclaude/*`. | Fixed. `proxy/auth.rs`: the loopback exemption also requires a loopback `Host`; any request carrying `Origin` or a non-`none` `Sec-Fetch-Site` is refused on every path. `proxy.requireKeyOnLoopback` removes the exemption entirely. |
| 2 | Medium | Client `Authorization` forwarded when the selected account is `apikey`, so a natively logged-in Claude Code sent its own OAuth token to third-party upstreams alongside the third party's key. | Fixed. `authorization`, `x-api-key`, `cookie`, `chatgpt-account-id` are stripped before credential injection on every path (`forward.rs::CLIENT_CREDENTIAL_HEADERS`). |
| 3 | Medium | Unbounded request-body buffering on the forward path and the token-relay path. | Fixed. `proxy.maxBodyBytes` (64 MiB default) returns 413; control bodies capped at 64 KiB. |
| 4 | Low | Proxy API key relayed verbatim to upstream on passthrough and HTTP-forward paths. | Fixed by #2; plain-HTTP forward proxying is not supported at all. |
| 5 | Low | Internal connect-error strings returned to clients. | Fixed. Clients get fixed messages; detail goes to the server log. |
| 6 | Low | Account pins resolved to an array index and reused across retries; a reload during a held request could repoint the pin at another account. | Fixed. Accounts are addressed by stable id everywhere; `tried` sets hold ids. |
| 7 | Low | Egress guard fails open when the IP probe is unreachable. | Feature not ported. |
| 8 | Info | Length-leaking key compare, `modelMap[obj.model]` prototype lookup, IPv6 CONNECT mis-parse, sx.org key in a query string, plaintext listener off-box. | Compare pads to equal length; `BTreeMap` lookups; `[v6]:port` parsed; sx.org not ported; off-box bind logs a TLS warning and requires a key. |

## Credentials, OAuth, config and state

| # | Severity | Finding (original) | Disposition here |
| --- | --- | --- | --- |
| 1 | Medium | Config and state written with truncate-in-place; a crash mid-write leaves an unparsable file holding every refresh token. | Fixed. `security::write_private_atomic`: temp file, fsync, rename, 0600. |
| 2 | Medium | Cross-process lost update: a CLI `login` running while the server refreshed a token could overwrite the rotated refresh token with a stale one. | Fixed. `Config::update` takes a cross-process `flock` on `corrall.lock`, re-reads under it, then writes atomically; every CLI mutation does its network work first and mutates inside the lock. |
| 3 | Medium | An OAuth account with a third-party `upstream` sends its Anthropic bearer token to that host. | Fixed. `Account::upstream_for` refuses non-Anthropic hosts for subscription accounts; the account is skipped and the reason logged. |
| 4 | Low | Anthropic OAuth callback listener bound all interfaces; the `error` parameter was honoured before the `state` check, so a LAN host could abort a login. | Fixed. Binds 127.0.0.1, ignores non-loopback peers, checks `state` first. |
| 5 | Low | Session map keyed on an unbounded, attacker-controlled header. | Fixed. Ids validated (charset, ≤128), map capped at 10 000 with idle eviction. |
| 6 | Low | Crash log mode enforced only on creation. | No crash log; the process logs to stderr/journal. |

Sound and kept: PKCE S256, refresh only to the fixed token endpoint, dead-token
guard, read-only import of Claude Code's store, credentials absent from status
and state output.

## CLI, updater, service, CI

| # | Severity | Finding (original) | Disposition here |
| --- | --- | --- | --- |
| 1 | High (headless) | Daily unattended `npm install -g` from the registry with no provenance check, no `--ignore-scripts`, no major-version fence. | Fixed. Nothing installs unattended: the server only *reports* a newer release. `corrall update` is operator-invoked, fetches through authenticated `gh`, verifies the archive against `SHA256SUMS` and its Sigstore signature, refuses downgrades and silent major jumps, and rolls the binary back if the restarted server is not healthy. |
| 2 | Medium | `TC_ACCT` + MITM put the proxy master key into `HTTPS_PROXY` for every subprocess Claude Code spawns. | Fixed. `env`/`run` include the key only when the proxy is bound off-loopback; on loopback the pin travels alone. |
| 3 | Medium | Client-controlled `model`, path and session id reached the TUI unsanitised (terminal escape injection). | Fixed. Everything shown in the TUI or logged goes through `security::safe_text`; session ids are validated on ingest. |
| 4 | Low | Terminal title stripped C0 but not C1/format characters. | Title setting not ported; `safe_text` strips all control and C1 characters. |
| 5 | Medium | Publish workflow installed unpinned devDependencies with scripts enabled in a job holding `id-token: write`; mutable action tags; unbranched `workflow_dispatch`. | CI here is read-only, SHA-pinned where third-party, and publishes nothing. |
| 6 | Low | systemd unit unquoted paths, no hardening directives. | Unit has `NoNewPrivileges`, `PrivateTmp`, `ProtectSystem=strict`, `ReadWritePaths` limited to the config directory. |
| 7 | Low | `spawnSync('claude', …, { shell: win32 })` re-joins arguments on Windows. | `std::process::Command` with an argument vector; never a shell. |

## Additional hardening in this port

- Blind CONNECT tunnels are off by default (`mitm.allowTunnel`), and when on
  refuse loopback, RFC1918, link-local, ULA, CGNAT and metadata addresses and
  any port other than 443 unless listed in `mitm.tunnelAllow`.
- Non-loopback bind without a key of at least 16 characters fails config
  validation; so does a plaintext `upstream` to a non-loopback host.
- Failed authentication is delayed 250 ms and counted in
  `corrall_auth_failures_total`.
- Upstream redirects are never followed with a credential attached.
- Request-log files redact `authorization`/`x-api-key` and are swept by age.
- Memory safety and no `unsafe` in the crate.

## Changes for claude.ai connectors (2026-09-15)

| Change | Surface | Disposition |
| --- | --- | --- |
| `content-length` is no longer copied from the client (`forward.rs::HOP_BY_HOP`, also used by `relay.rs`). | Two `content-length` lines went upstream whenever the rewritten body kept its size; Cloudflare answered 400. Duplicate framing headers are also the classic request-smuggling ambiguity. | Fixed; the proxy sends only the length of the body it actually writes. |
| `/v1/mcp_servers` and `/api/organizations/*/mcp/*` join the passthrough list (`relay.rs::is_connector_path`). | Relayed with the client's own `authorization`, no fleet token. Same trust as the existing OAuth-refresh and Remote Control passthroughs: an authenticated (or loopback-exempt) client can reach these upstream paths through the proxy with its own credentials only. Bodies capped at 1 MiB; proxy key stripped. | Accepted. Nothing of the fleet is exposed; the `org` segment must be non-empty and only the `mcp` subtree matches. |
| `platform.claude.com`, `mcp-proxy.anthropic.com` and `*.mcp.claude.com` get a blind tunnel on 443 regardless of `mitm.allowTunnel` (`mitm.rs::is_session_tunnel_host`). | Widens the open-relay question by three Anthropic-owned names. CONNECT authentication still runs first; other ports and every other host keep the old rules. The suffix match is anchored to `.mcp.claude.com`, so `evil-mcp.claude.com` does not match. The tunnel dials the name as given, so a DNS answer from Anthropic's zone is trusted, as it is for the intercepted host. | Accepted. The alternative (refusing) broke token refresh and connectors for every MITM-mode session. |
| The proxy key is accepted in `x-corrall-key` (`auth.rs::PROXY_KEY_HEADER`), added to `CLIENT_CREDENTIAL_HEADERS` and to the relay's drop list. | One more header that carries the key. Stripped on every path before anything goes upstream; never written to request logs (those record the upstream-bound header set). `corrall env`/`run` now emit it through `ANTHROPIC_CUSTOM_HEADERS` instead of `ANTHROPIC_API_KEY`, and only when the client will not be a loopback peer. | Accepted. Residual: Claude Code applies `ANTHROPIC_CUSTOM_HEADERS` to its SDK client, whose base URL is the proxy in base-URL mode and which is intercepted in MITM mode, so the key does not leave the box in either launch mode Corrall configures. A hand-written `settings.json` that sets the header while pointing the SDK elsewhere would send the key there. |
| `corrall env` decides whether a key is needed from the dial address rather than the bind address (`cli.rs::env_lines`). | A wildcard bind is reached over loopback, where the server exempts the peer anyway, so the emitted key was redundant and switched off Claude Code's login. A specific non-loopback bind still emits the key. | Fixed. |

## Residual risks

- The listener is plain HTTP. Off-box use needs a TLS terminator in front, and
  the key still authorises the whole fleet; prefer per-client keys.
- The MITM leaf key on disk (0600) lets a same-user process impersonate
  `api.anthropic.com` to a client that trusts the local CA. This is inherent to
  the MITM feature; use `--no-mitm` where that matters.
- A same-user process that can reach the loopback listener can spend the
  fleet's quota unless `proxy.requireKeyOnLoopback` is on. That is the same
  trust boundary as the original and is documented rather than closed.
