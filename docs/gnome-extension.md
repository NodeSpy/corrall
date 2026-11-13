# GNOME Shell indicator

A panel indicator for the local proxy, in `gnome/corrall@nodespy.github.io`.
It is read-only: it polls the control API and shows what the TUI shows. It
works with GNOME Shell 48, 49, 50 and 51.

## What it shows

**Panel label**, next to the icon:

- `41% / 23%`: the highest and the average 5h utilization among the accounts
  that can serve right now. The highest says how soon the next session will be
  bounced; the average says how much of the fleet is spent. With one such
  account it is a single number.
- `⏳ 2h 06m`: no account can serve. The countdown runs to the soonest reset
  of the window that is blocking an account (or the end of its rate limit).
  `exhausted` appears when accounts are blocked but none has a known reset.
- With more than one pool, one segment per pool: `default 41% / 23% · work ⏳ 12m`.
- Empty when there is nothing to report. The icon dims when the daemon cannot
  be reached.

An account "can serve" when it is not disabled and `blocked` is null in
`GET /corrall/status`: the same test the proxy applies when it picks one.

**Dropdown**:

- A header with the number of enabled accounts and active sessions.
- One entry per account, in priority order (under a pool heading when there is
  more than one pool): name, tier and priority, a badge (`active`, `ready`,
  `at limit · 12m`, `rate-limited · 1m`, `needs login`, `disabled`), the F, S
  and O flags for Fable, Sonnet and Opus (green when that family can be served
  now, red when the account can serve but that family's weekly window is
  spent, dim when it cannot serve at all), and bars for the 5h and 7d windows
  with their reset times. Fable and Sonnet weekly bars appear for accounts the
  plan meters them on. `active` means the current account, or one with live
  sessions or an open request.
- Sessions, those with a request open first (marked `●`), with the account,
  how long the request has run or how long ago the session was last heard
  from, requests, and tokens. Fetched from `GET /corrall/sessions` only while
  the menu is open. A daemon older than the one that added that endpoint
  answers 404 and the section stays hidden.
- A footer with the version, uptime, address, and the newer version when
  `updateAvailable` is set. `Refresh now` polls at once.

It polls every 10 s, and every 3 s while the menu is open. If the daemon is not
running the icon dims, the dropdown says so, and polling continues, so the
indicator recovers on its own when the daemon comes back.

## Install

    corrall gnome install

The extension is built into the binary. This writes it to
`~/.local/share/gnome-shell/extensions/corrall@nodespy.github.io` (under
`$XDG_DATA_HOME` when that is set), replacing an older copy, and enables it.
A directory that is a symlink is taken for a development checkout and left
alone. From a source checkout the same files can be copied by hand:

    mkdir -p ~/.local/share/gnome-shell/extensions
    cp -r gnome/corrall@nodespy.github.io ~/.local/share/gnome-shell/extensions/

## Enable

`corrall gnome install` enables it. A running shell only knows the extensions
it found when it started, which on Wayland means at login: when the shell has
not seen this one yet, the command turns it on in the shell's settings instead
and says so, and the indicator appears after you log out and back in. On X11,
`Alt+F2`, `r`, Enter restarts the shell without logging out. By hand:

    gnome-extensions enable corrall@nodespy.github.io

## Update

Run `corrall gnome install` again after `corrall update`, then log out and
back in (Wayland) or restart the shell (X11). A running shell keeps the old
code until then.

## Remove

    corrall gnome uninstall

## Settings

None in GNOME. The extension reads `~/.config/corrall.json` (or
`$XDG_CONFIG_HOME/corrall.json`, or `$CORRALL_CONFIG`) on every poll:

| Key | Used for |
| --- | --- |
| `proxy.port` | Port to poll. Default 3456 |
| `proxy.host` | Address to poll. A wildcard (`0.0.0.0`, `::`) or unset means `127.0.0.1` |
| `proxy.requireKeyOnLoopback` | When true, the key below is sent |
| `proxy.apiKey` | Sent as `x-corrall-key`, only when the daemon needs it: `requireKeyOnLoopback` is true, or `proxy.host` is a specific non-loopback address |

The key is read from the file each time. It is not copied into GSettings and
is never logged or shown. Without `requireKeyOnLoopback`, the extension sends
no key, as `curl http://127.0.0.1:3456/corrall/status` needs none. The poll and
display intervals are constants at the top of `extension.js`.

GNOME Shell does not inherit your terminal's environment. If the daemon runs
with a non-default `CORRALL_CONFIG` (the systemd unit sets one), the shell has
to see the same variable, for example through `~/.config/environment.d/`, or
the extension reads the default path instead.

## Troubleshooting

| Dropdown says | Meaning |
| --- | --- |
| `daemon not running` | Nothing answered on the configured address. Start it with `corrall service install` or `corrall server`, and check `proxy.port` and `proxy.host` |
| `proxy key rejected` | HTTP 401 or 403. `proxy.apiKey` is stale, or the daemon is reached through an address it does not treat as loopback |
| `no apiKey in corrall.json` | The daemon needs a key and the config has none |
| `cannot read corrall.json` | The file exists but is unreadable or not JSON. A missing file is fine and means the defaults |
| `HTTP 404` | The address answers but is not corrall |
| `bad status payload` | The reply was not the expected JSON; see `journalctl -f /usr/bin/gnome-shell` |

The icon does not appear after enabling: on Wayland, log out and in. Check
`gnome-extensions list` and `gnome-extensions show corrall@nodespy.github.io`;
`State: ERROR` comes with a message in `journalctl --user -b /usr/bin/gnome-shell`.
