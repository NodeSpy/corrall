// corrall GNOME Shell extension: a panel indicator for the local corrall
// proxy. It polls GET /corrall/status (and GET /corrall/sessions while the
// menu is open) and shows which accounts can serve, how full each 5h/7d
// window is, and which sessions have a request open. Ported from the
// claudeacrobat extension.

import Cairo from 'cairo';
import Clutter from 'gi://Clutter';
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import GObject from 'gi://GObject';
import Soup from 'gi://Soup?version=3.0';
import St from 'gi://St';

import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import * as PanelMenu from 'resource:///org/gnome/shell/ui/panelMenu.js';
import * as PopupMenu from 'resource:///org/gnome/shell/ui/popupMenu.js';

const POLL_SECONDS = 10;
const POLL_SECONDS_OPEN = 3;
const MAX_SESSIONS = 8;
const WARN_AT = 0.6;

// Colours shared with the TUI: green at 0%, yellow at 60%, red at 100%.
const GREEN = [0x33 / 255, 0xd1 / 255, 0x7a / 255];
const YELLOW = [0xf6 / 255, 0xd3 / 255, 0x2d / 255];
const RED = [0xe0 / 255, 0x1b / 255, 0x24 / 255];

/** Same lookup as corrall itself: $CORRALL_CONFIG, else $XDG_CONFIG_HOME/corrall.json, else ~/.config/corrall.json. */
function configPath() {
    const env = GLib.getenv('CORRALL_CONFIG');
    if (env)
        return env;
    const xdg = GLib.getenv('XDG_CONFIG_HOME');
    const base = xdg && GLib.path_is_absolute(xdg) ? xdg : GLib.build_filenamev([GLib.get_home_dir(), '.config']);
    return GLib.build_filenamev([base, 'corrall.json']);
}

function isLoopbackHost(h) {
    return h === 'localhost' || h === '::1' || h.startsWith('127.');
}

/**
 * Reads the listen address and, when the daemon needs one, the proxy key
 * from corrall's config. The key is read on every poll and kept only in the
 * returned object; it is never stored in settings or logged. A missing
 * config means the defaults (127.0.0.1:3456, no key). Returns {error} when
 * the file exists but cannot be used.
 */
function readConfig() {
    let proxy = {};
    try {
        const [ok, bytes] = GLib.file_get_contents(configPath());
        if (ok)
            proxy = JSON.parse(new TextDecoder().decode(bytes)).proxy || {};
    } catch (e) {
        if (!e.matches?.(GLib.FileError, GLib.FileError.NOENT))
            return {error: 'cannot read corrall.json'};
    }
    const port = Number.isInteger(proxy.port) ? proxy.port : 3456;
    let host = typeof proxy.host === 'string' ? proxy.host.trim().replace(/^\[|\]$/g, '') : '';
    if (host === '' || host === '0.0.0.0' || host === '::')
        host = '127.0.0.1';
    const display = host.includes(':') ? `[${host}]:${port}` : `${host}:${port}`;
    // A listener on a non-loopback address sees this client as non-loopback,
    // so the key is needed there even without requireKeyOnLoopback.
    const needKey = proxy.requireKeyOnLoopback === true || !isLoopbackHost(host);
    const key = needKey && typeof proxy.apiKey === 'string' ? proxy.apiKey.trim() : '';
    if (needKey && !key)
        return {error: 'no apiKey in corrall.json'};
    return {base: `http://${display}`, listen: display, key};
}

function clamp01(v) {
    return Math.min(1, Math.max(0, v));
}

function pct(v) {
    if (v === undefined || v === null || v < 0)
        return '—';
    return `${Math.round(clamp01(v) * 100)}%`;
}



/** "12m", "1h 05m", "2d 3h" until the given ISO time; '' when in the past. */
function countdown(iso, now) {
    if (!iso)
        return '';
    const t = GLib.DateTime.new_from_iso8601(iso, null);
    if (!t)
        return '';
    let secs = Math.round(t.difference(now) / GLib.TIME_SPAN_SECOND);
    if (secs <= 0)
        return 'now';
    const d = Math.floor(secs / 86400);
    secs -= d * 86400;
    const h = Math.floor(secs / 3600);
    secs -= h * 3600;
    const m = Math.floor(secs / 60);
    if (d > 0)
        return `${d}d ${h}h`;
    if (h > 0)
        return `${h}h ${String(m).padStart(2, '0')}m`;
    return `${Math.max(m, 1)}m`;
}

/** "15:10" when today, otherwise "Fri 15:00". */
function clockAt(iso, now) {
    if (!iso)
        return '';
    const t = GLib.DateTime.new_from_iso8601(iso, null)?.to_local();
    if (!t)
        return '';
    const sameDay = t.get_year() === now.get_year() && t.get_day_of_year() === now.get_day_of_year();
    return t.format(sameDay ? '%H:%M' : '%a %H:%M');
}

/** "12s ago", "3m ago", "1h ago". */
function ago(iso, now) {
    if (!iso)
        return '';
    const t = GLib.DateTime.new_from_iso8601(iso, null);
    if (!t)
        return '';
    const secs = Math.max(0, Math.round(now.difference(t) / GLib.TIME_SPAN_SECOND));
    if (secs < 60)
        return `${secs}s ago`;
    if (secs < 3600)
        return `${Math.floor(secs / 60)}m ago`;
    return `${Math.floor(secs / 3600)}h ${Math.floor((secs % 3600) / 60)}m ago`;
}

function fmtTokens(n) {
    if (n >= 1e9)
        return `${(n / 1e9).toFixed(1)}B`;
    if (n >= 1e6)
        return `${(n / 1e6).toFixed(1)}M`;
    if (n >= 1e3)
        return `${(n / 1e3).toFixed(0)}k`;
    return `${n}`;
}

/**
 * A utilization bar drawn with cairo. The fill is a gradient along the
 * whole track (green → yellow at 60% → red), clipped to the value, so
 * the tip reddens as the window fills. 'exhausted' paints solid red and
 * 'unknown' leaves the track empty.
 */
const Bar = GObject.registerClass(
class Bar extends St.DrawingArea {
    _init() {
        super._init({style_class: 'corrall-bar', x_expand: true, y_align: Clutter.ActorAlign.CENTER});
        this._value = 0;
        this._mode = 'unknown';
        this.connect('repaint', () => this._draw());
    }

    set(value, mode) {
        this._value = value;
        this._mode = mode;
        this.queue_repaint();
    }

    _rounded(cr, x, y, w, h) {
        const r = Math.min(h / 2, w / 2);
        cr.newSubPath();
        cr.arc(x + w - r, y + r, r, -Math.PI / 2, 0);
        cr.arc(x + w - r, y + h - r, r, 0, Math.PI / 2);
        cr.arc(x + r, y + h - r, r, Math.PI / 2, Math.PI);
        cr.arc(x + r, y + r, r, Math.PI, 3 * Math.PI / 2);
        cr.closePath();
    }

    _draw() {
        const cr = this.get_context();
        const [w, h] = this.get_surface_size();
        if (w <= 0 || h <= 0) {
            cr.$dispose();
            return;
        }

        this._rounded(cr, 0, 0, w, h);
        cr.setSourceRGBA(0.5, 0.5, 0.5, 0.25);
        cr.fill();

        if (this._mode !== 'unknown') {
            let fillW = w;
            if (this._mode !== 'exhausted') {
                const v = clamp01(this._value);
                fillW = v <= 0 ? 0 : Math.max(h, v * w);
            }
            if (fillW > 0) {
                cr.save();
                this._rounded(cr, 0, 0, fillW, h);
                cr.clip();
                if (this._mode === 'exhausted') {
                    cr.setSourceRGBA(...RED, 1);
                } else {
                    const g = new Cairo.LinearGradient(0, 0, w, 0);
                    g.addColorStopRGBA(0, ...GREEN, 1);
                    g.addColorStopRGBA(WARN_AT, ...YELLOW, 1);
                    g.addColorStopRGBA(1, ...RED, 1);
                    cr.setSource(g);
                }
                cr.paint();
                cr.restore();
            }
        }
        cr.$dispose();
    }
});

/** One labelled bar row: "5h  [=====---]  68%  resets 15:10". */
const BarRow = GObject.registerClass(
class BarRow extends St.BoxLayout {
    _init(label) {
        super._init({style_class: 'corrall-bar-row', x_expand: true});
        this._label = new St.Label({text: label, style_class: 'corrall-bar-label', y_align: Clutter.ActorAlign.CENTER});
        this._label.opacity = 170;
        this._bar = new Bar();
        this._pct = new St.Label({style_class: 'corrall-bar-pct', y_align: Clutter.ActorAlign.CENTER});
        this._reset = new St.Label({style_class: 'corrall-bar-reset', y_align: Clutter.ActorAlign.CENTER});
        this._reset.opacity = 170;
        this.add_child(this._label);
        this.add_child(this._bar);
        this.add_child(this._pct);
        this.add_child(this._reset);
    }

    update(value, mode, resetIso, now) {
        this._bar.set(value, mode);
        this._pct.text = mode === 'unknown' ? '—' : pct(mode === 'exhausted' ? 1 : value);
        if (mode === 'exhausted')
            this._reset.text = resetIso ? `resets in ${countdown(resetIso, now)}` : 'exhausted';
        else
            this._reset.text = resetIso ? `resets ${clockAt(resetIso, now)}` : '';
    }
});


/** Splits status into pools. Older daemons without `pools` are one pool. */
function poolsOf(status) {
    const list = Array.isArray(status.pools) && status.pools.length ? status.pools : [status];
    return list.map(p => ({name: p.pool || 'default', status: p, accounts: p.accounts || []}));
}

/** The bucket a "quota: 5h at 99%" / "capped: unified7d" reason is about. */
function reasonBucket(reason) {
    const m = /unified7dFable|unified7dSonnet|unified7d|unified5h|\b5h\b/.exec(reason || '');
    if (!m)
        return null;
    return m[0] === '5h' ? 'unified5h' : m[0];
}

/** Disabled, broken, or otherwise out for a reason that no clock will fix. */
function isDead(a) {
    const r = a.blocked || '';
    return a.disabled || a.status === 'error' || r.startsWith('error') || r.startsWith('no credential') ||
        r.startsWith('upstream refused') || r.startsWith('other provider');
}

/** ISO time at which a blocked account is expected to serve again, or null. */
function waitIso(a) {
    const r = a.blocked || '';
    if (isDead(a) || !r)
        return null;
    if (r.startsWith('rate-limited'))
        return a.rateLimitedUntil || null;
    if (r.startsWith('quota') || r.startsWith('capped')) {
        const key = reasonBucket(r);
        return key ? a.quota?.[key]?.resetAt || null : null;
    }
    return null;
}

/** "41% / 23%" (max / average 5h) among the accounts that can serve now. */
function servingLabel(accounts) {
    const utils = accounts
        .filter(a => !a.disabled && !a.blocked)
        .map(a => a.quota?.unified5h?.utilization)
        .filter(u => typeof u === 'number' && u >= 0);
    if (!utils.length)
        return '';
    if (utils.length === 1)
        return pct(utils[0]);
    const avg = utils.reduce((sum, u) => sum + u, 0) / utils.length;
    return `${pct(Math.max(...utils))} / ${pct(avg)}`;
}

/** Label for one pool: utilization, or a countdown when nothing can serve. */
function poolLabel(accounts, now) {
    if (!accounts.length)
        return {text: '', exhausted: false};
    if (accounts.some(a => !a.disabled && !a.blocked))
        return {text: servingLabel(accounts), exhausted: false};
    if (accounts.every(isDead))
        return {text: '', exhausted: false};
    const soonest = accounts.map(waitIso).filter(Boolean).sort()[0];
    return {text: soonest ? `⏳ ${countdown(soonest, now)}` : 'exhausted', exhausted: true};
}

function shortDuration(ms) {
    const s = Math.max(0, Math.round(ms / 1000));
    if (s < 60)
        return `${s}s`;
    if (s < 3600)
        return `${Math.floor(s / 60)}m`;
    return `${Math.floor(s / 3600)}h ${String(Math.floor((s % 3600) / 60)).padStart(2, '0')}m`;
}

/** A menu entry for one account: title row, flags and bars. */
const AccountItem = GObject.registerClass(
class AccountItem extends PopupMenu.PopupBaseMenuItem {
    _init() {
        super._init({reactive: false, can_focus: false});

        this._box = new St.BoxLayout({
            style_class: 'corrall-account',
            orientation: Clutter.Orientation.VERTICAL,
            x_expand: true,
        });
        this.add_child(this._box);

        const title = new St.BoxLayout({style_class: 'corrall-account-title', x_expand: true});
        this._name = new St.Label({style_class: 'corrall-account-name', y_align: Clutter.ActorAlign.CENTER});
        this._kind = new St.Label({style_class: 'corrall-footer', y_align: Clutter.ActorAlign.CENTER});
        this._kind.opacity = 150;
        this._badge = new St.Label({style_class: 'corrall-badge', y_align: Clutter.ActorAlign.CENTER});
        this._flags = new St.BoxLayout({style_class: 'corrall-flags', y_align: Clutter.ActorAlign.CENTER});
        this._flagLabels = {fable: new St.Label(), sonnet: new St.Label(), opus: new St.Label()};
        this._flags.add_child(this._flagLabels.fable);
        this._flags.add_child(new St.Label({text: ' '}));
        this._flags.add_child(this._flagLabels.sonnet);
        this._flags.add_child(new St.Label({text: ' '}));
        this._flags.add_child(this._flagLabels.opus);
        title.add_child(this._name);
        title.add_child(this._kind);
        title.add_child(new St.Widget({x_expand: true}));
        title.add_child(this._flags);
        title.add_child(this._badge);
        this._box.add_child(title);

        this._row5h = new BarRow('5h');
        this._row7d = new BarRow('7d');
        this._box.add_child(this._row5h);
        this._box.add_child(this._row7d);
        this._modelRows = new Map();
    }

    _setFlag(label, letter, account, family) {
        // Green ✓ when the family can be served now; red ✗ when the account
        // can serve in general but not this family (its own weekly window is
        // spent); dim ✗ when the account cannot serve anything.
        label.remove_style_class_name('corrall-flag-ok');
        label.remove_style_class_name('corrall-flag-spent');
        label.opacity = 255;
        if (account.models?.[family]) {
            label.text = `${letter}✓`;
            label.add_style_class_name('corrall-flag-ok');
            return;
        }
        label.text = `${letter}✗`;
        if (!account.blocked)
            label.add_style_class_name('corrall-flag-spent');
        else
            label.opacity = 110;
    }

    update(account, serving, now) {
        this._name.text = account.name;
        this._kind.text = `${account.tier || account.type || ''} · #${account.priority}`;

        this._badge.remove_style_class_name('corrall-badge-active');
        this._badge.remove_style_class_name('corrall-badge-cooling');
        this._badge.remove_style_class_name('corrall-badge-warn');
        const wait = waitIso(account);
        if (account.disabled) {
            this._badge.text = 'disabled';
        } else if (!account.blocked) {
            // "active" = carrying traffic now (the current account, or one
            // with live sessions or an open request); "ready" = idle.
            this._badge.text = serving ? 'active' : 'ready';
            if (serving)
                this._badge.add_style_class_name('corrall-badge-active');
        } else if (isDead(account)) {
            this._badge.text = account.unavailable === 'error' ? 'needs login' : (account.unavailable || 'unavailable');
            this._badge.add_style_class_name('corrall-badge-warn');
        } else {
            const what = account.unavailable === 'throttled' ? 'rate-limited' : 'at limit';
            this._badge.text = wait ? `${what} · ${countdown(wait, now)}` : what;
            this._badge.add_style_class_name('corrall-badge-cooling');
        }

        this._setFlag(this._flagLabels.fable, 'F', account, 'fable');
        this._setFlag(this._flagLabels.sonnet, 'S', account, 'sonnet');
        this._setFlag(this._flagLabels.opus, 'O', account, 'opus');

        // A bar is "exhausted" when the block reason names its bucket or its
        // utilization has reached 100%.
        const blockedOn = account.blocked && !isDead(account) ? reasonBucket(account.blocked) : null;
        const row = (bar, key, resetNow) => {
            const b = account.quota?.[key] || {};
            const u = typeof b.utilization === 'number' ? b.utilization : null;
            const mode = u === null ? 'unknown' : (blockedOn === key || u >= 1 ? 'exhausted' : 'normal');
            bar.update(u, mode, b.resetAt, resetNow);
        };
        row(this._row5h, 'unified5h', now);
        row(this._row7d, 'unified7d', now);

        // Per-family weekly windows, only for accounts the plan meters them on.
        const seen = new Set();
        for (const [key, label] of [['unified7dFable', '7d Fable'], ['unified7dSonnet', '7d Sonnet']]) {
            if (typeof account.quota?.[key]?.utilization !== 'number')
                continue;
            seen.add(key);
            let r = this._modelRows.get(key);
            if (!r) {
                r = new BarRow(label);
                this._modelRows.set(key, r);
                this._box.add_child(r);
            }
            row(r, key, now);
        }
        for (const [key, r] of this._modelRows) {
            if (!seen.has(key)) {
                r.destroy();
                this._modelRows.delete(key);
            }
        }
    }
});

/** One session, those with a request open first. */
const SessionItem = GObject.registerClass(
class SessionItem extends PopupMenu.PopupBaseMenuItem {
    _init() {
        super._init({reactive: false, can_focus: false});
        const box = new St.BoxLayout({style_class: 'corrall-session', x_expand: true});
        this._name = new St.Label({style_class: 'corrall-session-name', y_align: Clutter.ActorAlign.CENTER});
        this._account = new St.Label({style_class: 'corrall-session-meta', y_align: Clutter.ActorAlign.CENTER});
        this._account.opacity = 170;
        this._meta = new St.Label({style_class: 'corrall-session-meta', y_align: Clutter.ActorAlign.CENTER});
        this._meta.opacity = 170;
        box.add_child(this._name);
        box.add_child(this._account);
        box.add_child(new St.Widget({x_expand: true}));
        box.add_child(this._meta);
        this.add_child(box);
    }

    update(s, multiPool, now) {
        const busy = (s.inFlight || 0) > 0;
        const name = s.name || String(s.id || '').slice(0, 8);
        this._name.text = busy ? `● ${name}` : name;
        const account = s.account ? (multiPool && s.pool ? `${s.pool}/${s.account}` : s.account) : '';
        this._account.text = account;
        const t = s.tokens || {};
        const total = (t.input || 0) + (t.cacheCreation || 0) + (t.cacheRead || 0) + (t.output || 0);
        const when = busy
            ? (typeof s.busyMs === 'number' ? `running ${shortDuration(s.busyMs)}` : 'running')
            : ago(s.lastSeen, now);
        this._meta.text = `${when} · ${s.requests || 0} req · ${fmtTokens(total)} tok`;
    }
});

const Indicator = GObject.registerClass(
class Indicator extends PanelMenu.Button {
    _init(extension) {
        super._init(0.5, 'corrall');
        this._extension = extension;
        this._session = new Soup.Session({timeout: 5});
        this._cancellable = null;
        this._timer = 0;
        this._lastOk = null;

        const box = new St.BoxLayout({style_class: 'panel-status-menu-box'});
        this._icon = new St.Icon({
            gicon: Gio.icon_new_for_string(`${extension.path}/icons/corrall-symbolic.svg`),
            style_class: 'system-status-icon',
        });
        this._label = new St.Label({style_class: 'corrall-panel-label', y_align: Clutter.ActorAlign.CENTER});
        box.add_child(this._icon);
        box.add_child(this._label);
        this.add_child(box);

        this.menu.actor.add_style_class_name('corrall-menu');

        // Header: title + one-line daemon summary.
        this._header = new PopupMenu.PopupBaseMenuItem({reactive: false, can_focus: false});
        const hbox = new St.BoxLayout({style_class: 'corrall-header-row', x_expand: true});
        hbox.add_child(new St.Label({text: 'corrall', style_class: 'corrall-header', y_align: Clutter.ActorAlign.CENTER}));
        hbox.add_child(new St.Widget({x_expand: true}));
        this._summary = new St.Label({style_class: 'corrall-footer', y_align: Clutter.ActorAlign.CENTER});
        this._summary.opacity = 170;
        hbox.add_child(this._summary);
        this._header.add_child(hbox);
        this.menu.addMenuItem(this._header);

        this._accountsSection = new PopupMenu.PopupMenuSection();
        this.menu.addMenuItem(this._accountsSection);
        this._accountItems = new Map();
        this._poolHeaders = new Map();

        this._sessionsSep = new PopupMenu.PopupSeparatorMenuItem('Sessions');
        this.menu.addMenuItem(this._sessionsSep);
        this._sessionsSection = new PopupMenu.PopupMenuSection();
        this.menu.addMenuItem(this._sessionsSection);
        this._sessionItems = [];

        this.menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());
        this._refresh = new PopupMenu.PopupMenuItem('Refresh now');
        this._refresh.connect('activate', () => this._poll());
        this.menu.addMenuItem(this._refresh);
        this._footer = new PopupMenu.PopupBaseMenuItem({reactive: false, can_focus: false});
        this._footerLabel = new St.Label({style_class: 'corrall-footer'});
        this._footerLabel.opacity = 150;
        this._footer.add_child(this._footerLabel);
        this.menu.addMenuItem(this._footer);

        this.menu.connect('open-state-changed', (_m, open) => {
            if (open)
                this._poll();
            this._schedule();
        });

        this._showDown('starting…');
        this._poll();
        this._schedule();
    }

    _schedule() {
        if (this._timer) {
            GLib.source_remove(this._timer);
            this._timer = 0;
        }
        const secs = this.menu.isOpen ? POLL_SECONDS_OPEN : POLL_SECONDS;
        this._timer = GLib.timeout_add_seconds(GLib.PRIORITY_DEFAULT, secs, () => {
            this._poll();
            return GLib.SOURCE_CONTINUE;
        });
    }

    /** GET a control path; resolves to {status, data} (data parsed JSON or null). */
    _get(cfg, path, cancellable) {
        return new Promise((resolve, reject) => {
            const msg = Soup.Message.new('GET', `${cfg.base}${path}`);
            if (cfg.key)
                msg.request_headers.append('x-corrall-key', cfg.key);
            this._session.send_and_read_async(msg, GLib.PRIORITY_DEFAULT, cancellable, (session, res) => {
                try {
                    const bytes = session.send_and_read_finish(res);
                    const status = msg.get_status();
                    let data = null;
                    if (status === Soup.Status.OK)
                        data = JSON.parse(new TextDecoder().decode(bytes.get_data()));
                    resolve({status, data});
                } catch (e) {
                    reject(e);
                }
            });
        });
    }

    async _poll() {
        const cfg = readConfig();
        if (cfg.error) {
            this._showDown(cfg.error);
            return;
        }
        if (this._cancellable)
            this._cancellable.cancel();
        const cancellable = new Gio.Cancellable();
        this._cancellable = cancellable;
        try {
            const r = await this._get(cfg, '/corrall/status', cancellable);
            if (cancellable.is_cancelled())
                return;
            if (r.status !== Soup.Status.OK) {
                this._showDown(r.status === Soup.Status.UNAUTHORIZED || r.status === Soup.Status.FORBIDDEN ? 'proxy key rejected' : `HTTP ${r.status}`);
                return;
            }
            // Sessions only while the menu is open; older daemons answer 404
            // and the section simply stays hidden.
            let sessions = null;
            if (this.menu.isOpen) {
                try {
                    const s = await this._get(cfg, '/corrall/sessions', cancellable);
                    if (s.status === Soup.Status.OK)
                        sessions = s.data;
                } catch (e) {
                    if (cancellable.is_cancelled())
                        return;
                }
            }
            if (cancellable.is_cancelled())
                return;
            this._render(r.data, sessions, cfg);
        } catch (e) {
            if (cancellable.is_cancelled() || (e.matches && e.matches(Gio.IOErrorEnum, Gio.IOErrorEnum.CANCELLED)))
                return;
            if (e instanceof SyntaxError || e instanceof TypeError) {
                this._showDown('bad status payload');
                console.error(`corrall: ${e}`);
            } else {
                this._showDown('daemon not running');
            }
        }
    }

    // The icon stays monochrome so it matches the other status icons; the
    // label carries the state (percentage, or a countdown when exhausted)
    // and the icon only dims when the daemon is unreachable.
    _setIconState(state) {
        this._icon.opacity = state === 'down' ? 110 : 255;
    }

    _clearAccounts() {
        for (const item of this._accountItems.values())
            item.destroy();
        this._accountItems.clear();
        for (const item of this._poolHeaders.values())
            item.destroy();
        this._poolHeaders.clear();
        for (const item of this._sessionItems)
            item.destroy();
        this._sessionItems = [];
        this._sessionsSep.hide();
    }

    _showDown(reason) {
        this._setIconState('down');
        this._label.text = '';
        this._label.hide();
        this._summary.text = reason;
        this._clearAccounts();
        this._footerLabel.text = this._lastOk ? `last seen ${ago(this._lastOk, GLib.DateTime.new_now_utc())}` : 'start it with: corrall service install';
    }

    _render(status, sessions, cfg) {
        const now = GLib.DateTime.new_now_local();
        this._lastOk = now.to_utc().format_iso8601();
        const pools = poolsOf(status);
        const multi = pools.length > 1;

        // Panel: "max / avg" of the 5h windows among the accounts that can
        // serve right now (the max says how soon the next session will be
        // bounced, the average how much of the pool is spent), or a
        // countdown to the soonest reset when none can. With several pools
        // each one gets its own segment, prefixed by its name.
        const labels = pools.map(p => ({name: p.name, ...poolLabel(p.accounts, now)}));
        const shown = labels.filter(l => l.text);
        this._setIconState('ok');
        this._label.text = multi ? shown.map(l => `${l.name} ${l.text}`).join(' · ') : (shown[0]?.text || '');
        this._label.visible = this._label.text !== '';

        const accountCount = pools.reduce((n, p) => n + p.accounts.filter(a => !a.disabled).length, 0);
        const active = pools.reduce((n, p) => n + (p.status.sessions?.active || 0), 0);
        this._summary.text = `${accountCount} account${accountCount === 1 ? '' : 's'} · ${active} active session${active === 1 ? '' : 's'}`;

        // Accounts, in priority order within each pool, reusing items by key.
        // Pool headers appear only when there is more than one pool.
        const order = [];
        const seen = new Set();
        const seenHeaders = new Set();
        for (const p of pools) {
            if (multi) {
                seenHeaders.add(p.name);
                let h = this._poolHeaders.get(p.name);
                if (!h) {
                    h = new PopupMenu.PopupSeparatorMenuItem(p.name);
                    this._poolHeaders.set(p.name, h);
                    this._accountsSection.addMenuItem(h);
                }
                order.push(h);
            }
            const sorted = [...p.accounts].sort((a, b) => (a.priority ?? 0) - (b.priority ?? 0));
            for (const a of sorted) {
                const key = `${p.name}\u0000${a.id || a.name}`;
                seen.add(key);
                let item = this._accountItems.get(key);
                if (!item) {
                    item = new AccountItem();
                    this._accountItems.set(key, item);
                    this._accountsSection.addMenuItem(item);
                }
                const serving = !a.disabled && !a.blocked && (a.current || (a.sessions || 0) > 0 || (a.inFlight || 0) > 0);
                item.update(a, serving, now);
                order.push(item);
            }
        }
        for (const [key, item] of this._accountItems) {
            if (!seen.has(key)) {
                item.destroy();
                this._accountItems.delete(key);
            }
        }
        for (const [name, h] of this._poolHeaders) {
            if (!seenHeaders.has(name)) {
                h.destroy();
                this._poolHeaders.delete(name);
            }
        }
        order.forEach((item, i) => this._accountsSection.box.set_child_at_index(item, i));

        // Sessions, with a request open first (the daemon orders them).
        const rows = sessions ? (sessions.sessions || []).slice(0, MAX_SESSIONS) : [];
        while (this._sessionItems.length < rows.length) {
            const item = new SessionItem();
            this._sessionItems.push(item);
            this._sessionsSection.addMenuItem(item);
        }
        while (this._sessionItems.length > rows.length)
            this._sessionItems.pop().destroy();
        rows.forEach((s, i) => this._sessionItems[i].update(s, multi, now));
        this._sessionsSep.visible = rows.length > 0;

        const up = countdown(now.add_seconds(status.uptimeSeconds || 0).format_iso8601(), now);
        const update = status.updateAvailable ? ` · update ${status.updateAvailable}` : '';
        this._footerLabel.text = `v${status.version} · up ${up} · ${cfg.listen}${update}`;
    }

    destroy() {
        if (this._timer) {
            GLib.source_remove(this._timer);
            this._timer = 0;
        }
        if (this._cancellable) {
            this._cancellable.cancel();
            this._cancellable = null;
        }
        this._session.abort();
        this._session = null;
        super.destroy();
    }
});

export default class CorrallExtension extends Extension {
    enable() {
        this._indicator = new Indicator(this);
        Main.panel.addToStatusArea(this.uuid, this._indicator);
    }

    disable() {
        this._indicator?.destroy();
        this._indicator = null;
    }
}
