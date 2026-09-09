# Security Policy

## Reporting a vulnerability

Please report vulnerabilities privately through GitHub's
[private vulnerability reporting](https://github.com/NodeSpy/corrall/security/advisories/new)
rather than a public issue. Include the version, reproduction steps and impact.

## Supported versions

Only the latest release on `main` receives fixes.

## Threat model in one paragraph

Corrall holds the OAuth refresh tokens of every account it rotates. Anyone
who can send a request that the proxy accepts can spend those accounts' quota,
and anyone who can read the config file owns the accounts outright. The design
therefore assumes: the config directory is private (0700/0600), the listener
is loopback-only unless a key is configured, and the proxy never forwards a
credential it received from a client. See [docs/security-review.md](docs/security-review.md)
for the review of the original implementation and the fixes carried into this
one.

## Verifying you have the genuine project

The only canonical source is https://github.com/NodeSpy/corrall. There is no
self-updater by design: update through `cargo install` or a release you verify
yourself.
