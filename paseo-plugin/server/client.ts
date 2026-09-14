// The only module that talks to the Corrall daemon. It discovers the daemon's
// port and proxy key from Corrall's own config file, calls the control API with
// that key, and normalises the daemon's camelCase status JSON into the flatter
// shape the surface renders (see shared/contracts.ts).

import { readFile } from "node:fs/promises";
import { homedir } from "node:os";
import { join } from "node:path";

import type { ActionResult, StatusEnvelope, StatusResponse } from "../shared/contracts";
import { StatusResponseSchema } from "../shared/contracts";

interface Daemon {
  baseUrl: string; // e.g. "http://127.0.0.1:3456"
  apiKey: string; // "" when a loopback daemon needs no key
}

// Mirrors config_path() in src/config.rs: a single `corrall.json` under
// $XDG_CONFIG_HOME (falling back to ~/.config), on every platform — never an
// OS-specific config dir. `$CORRALL_CONFIG` overrides it, as it does for the CLI.
function configPath(): string {
  const override = process.env.CORRALL_CONFIG;
  if (override) return override;
  const base = process.env.XDG_CONFIG_HOME || join(homedir(), ".config");
  return join(base, "corrall.json");
}

// The host a client on this machine dials. Mirrors Config::dial_host: a wildcard
// or `localhost` bind is reachable over loopback, anything else is kept as-is.
function dialHost(host: string | undefined): string {
  const h = (host ?? "").trim().replace(/^\[|\]$/g, "");
  if (h === "" || h === "0.0.0.0" || h === "::" || h.toLowerCase() === "localhost") {
    return "127.0.0.1";
  }
  return h.includes(":") ? `[${h}]` : h;
}

// Reads Corrall's config. Returns null when it is absent or malformed — the
// caller reports that as "not configured" rather than an error.
async function readDaemon(): Promise<Daemon | null> {
  let raw: string;
  try {
    raw = await readFile(configPath(), "utf8");
  } catch {
    return null;
  }
  try {
    const cfg = JSON.parse(raw) as { proxy?: { port?: number; host?: string; apiKey?: string } };
    const proxy = cfg.proxy ?? {};
    const port = typeof proxy.port === "number" && proxy.port > 0 ? proxy.port : 3456;
    const apiKey = typeof proxy.apiKey === "string" ? proxy.apiKey : "";
    return { baseUrl: `http://${dialHost(proxy.host)}:${port}`, apiKey };
  } catch {
    return null;
  }
}

const TIMEOUT_MS = 2500;

async function call(daemon: Daemon, method: "GET" | "POST", path: string, body?: unknown): Promise<Response> {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), TIMEOUT_MS);
  try {
    return await fetch(daemon.baseUrl + path, {
      method,
      headers: {
        // Corrall authenticates the control plane with the proxy key. A loopback
        // daemon accepts requests without it, but we send it whenever we have it.
        ...(daemon.apiKey ? { "x-api-key": daemon.apiKey } : {}),
        // A Host header a rebinding guard will accept.
        host: new URL(daemon.baseUrl).host,
        ...(body !== undefined ? { "content-type": "application/json" } : {}),
      },
      body: body !== undefined ? JSON.stringify(body) : undefined,
      signal: controller.signal,
    });
  } finally {
    clearTimeout(timer);
  }
}

// Distinguishes "daemon down" (connection refused / aborted) from other errors.
function unreachable(err: unknown): boolean {
  const e = err as { name?: string; code?: string; cause?: { code?: string } };
  const code = e?.code || e?.cause?.code;
  return (
    e?.name === "AbortError" ||
    code === "ECONNREFUSED" ||
    code === "ECONNRESET" ||
    code === "ENOTFOUND" ||
    code === "EHOSTUNREACH"
  );
}

// ── status normalisation ─────────────────────────────────────────────────────

// A utilization fraction, or -1 when the daemon has not observed it yet (the
// UI's "unknown" sentinel).
function util(x: unknown): number {
  return typeof x === "number" ? x : -1;
}

function iso(x: unknown): string | undefined {
  return typeof x === "string" && x ? x : undefined;
}

// Corrall's `switchThreshold` is either a single number or a per-bucket table;
// the UI shows one figure, so collapse a table to its 5h / default entry.
function threshold(x: unknown): number {
  if (typeof x === "number") return x;
  if (x && typeof x === "object") {
    const t = x as Record<string, number>;
    const v = t.unified5h ?? t.default ?? Object.values(t)[0];
    if (typeof v === "number") return v;
  }
  return 0.8;
}

// Quota / throttle reasons that should read as "cooling" (a spent window that
// will reset) rather than a hard error.
const COOLING = new Set(["throttled", "quota", "capped", "advisor-quota", "advisor-capped", "entitlement"]);

// Collapse Corrall's status / blocked / disabled / unavailable fields into the
// one-word lifecycle the surface colours by.
function accountState(a: Record<string, unknown>): import("../shared/contracts").AccountState {
  if (a.disabled) return "disabled";
  const reason = String(a.blocked ?? "").toLowerCase();
  const un = String(a.unavailable ?? "").toLowerCase();
  if (/login|auth|token|refresh|401|unauthor/.test(reason) || /login|auth/.test(un)) return "needs_login";
  if (COOLING.has(un)) return "cooling";
  if (a.blocked) return "unknown";
  return "available";
}

function modelBar(label: string, bucket: unknown, active: boolean): import("../shared/contracts").ModelStatus | null {
  const b = (bucket ?? {}) as Record<string, unknown>;
  if (typeof b.utilization !== "number") return null;
  return { label, utilization: b.utilization, reset: iso(b.resetAt), spent: !active };
}

function mapAccount(a: Record<string, unknown>): import("../shared/contracts").AccountStatus {
  const q = (a.quota ?? {}) as Record<string, unknown>;
  const models = (a.models ?? {}) as Record<string, unknown>;
  const u5 = (q.unified5h ?? {}) as Record<string, unknown>;
  const u7 = (q.unified7d ?? {}) as Record<string, unknown>;
  const fableActive = !!models.fable;
  const sonnetActive = !!models.sonnet;
  const bars = [modelBar("Fable", q.unified7dFable, fableActive), modelBar("Sonnet", q.unified7dSonnet, sonnetActive)].filter(
    (b): b is import("../shared/contracts").ModelStatus => b != null,
  );
  const spendRaw = q.spend as Record<string, unknown> | null | undefined;
  const spend = spendRaw
    ? {
        enabled: !!spendRaw.enabled,
        used_minor: typeof spendRaw.usedMinor === "number" ? spendRaw.usedMinor : undefined,
        limit_minor: typeof spendRaw.limitMinor === "number" ? spendRaw.limitMinor : undefined,
        currency: typeof spendRaw.currency === "string" ? spendRaw.currency : undefined,
        exponent: typeof spendRaw.exponent === "number" ? spendRaw.exponent : 2,
        user_disabled: typeof spendRaw.userDisabled === "boolean" ? spendRaw.userDisabled : undefined,
        disabled_reason: typeof spendRaw.disabledReason === "string" ? spendRaw.disabledReason : undefined,
      }
    : undefined;
  return {
    id: String(a.id ?? ""),
    name: String(a.name ?? ""),
    kind: String(a.type ?? "oauth"),
    state: accountState(a),
    priority: typeof a.priority === "number" ? a.priority : 0,
    disabled: !!a.disabled,
    utilization_5h: util(u5.utilization),
    utilization_7d: util(u7.utilization),
    reset_5h: iso(u5.resetAt),
    reset_7d: iso(u7.resetAt),
    models: bars.length ? bars : undefined,
    spend,
    fable_active: fableActive,
    sonnet_active: sonnetActive,
    opus_active: !!models.opus,
    serving: !!a.current,
    requests: Number((a.usage as Record<string, unknown> | undefined)?.totalRequests ?? 0),
    tier: typeof a.tier === "string" ? a.tier : undefined,
    last_error: typeof a.error === "string" && a.error ? a.error : undefined,
  };
}

function mapPool(p: Record<string, unknown>): import("../shared/contracts").PoolStatus {
  const accounts = Array.isArray(p.accounts) ? (p.accounts as Record<string, unknown>[]).map(mapAccount) : [];
  const sessions = (p.sessions ?? {}) as Record<string, unknown>;
  return {
    name: String(p.pool ?? ""),
    default: !!p.default,
    switch_threshold: threshold(p.switchThreshold),
    distribute_sessions: !!p.distributeSessions,
    sessions: typeof sessions.active === "number" ? sessions.active : 0,
    requests: accounts.reduce((n, a) => n + a.requests, 0),
    accounts,
  };
}

function normalize(raw: Record<string, unknown>): StatusResponse {
  const pools = Array.isArray(raw.pools) ? (raw.pools as Record<string, unknown>[]).map(mapPool) : [];
  return {
    version: String(raw.version ?? ""),
    default_pool: String(raw.defaultPool ?? pools.find((p) => p.default)?.name ?? pools[0]?.name ?? ""),
    pools,
  };
}

export async function fetchStatus(): Promise<StatusEnvelope> {
  const daemon = await readDaemon();
  if (!daemon) {
    return {
      state: "not_configured",
      message: "No corrall config found. Run `corrall server` once to create it.",
    };
  }
  try {
    const res = await call(daemon, "GET", "/corrall/status");
    if (res.status === 401) {
      return {
        state: "error",
        baseUrl: daemon.baseUrl,
        message: "Proxy key rejected (401). Restart corrall and reload the plugin.",
      };
    }
    if (!res.ok) {
      return { state: "error", baseUrl: daemon.baseUrl, message: `Daemon returned HTTP ${res.status}.` };
    }
    const raw = (await res.json()) as Record<string, unknown>;
    const status = StatusResponseSchema.parse(normalize(raw));
    return { state: "ok", baseUrl: daemon.baseUrl, status };
  } catch (err) {
    if (unreachable(err)) {
      return {
        state: "unreachable",
        baseUrl: daemon.baseUrl,
        message: "corrall is not reachable. Is `corrall server` running?",
      };
    }
    return { state: "error", baseUrl: daemon.baseUrl, message: err instanceof Error ? err.message : String(err) };
  }
}

// Shared POST helper for the mutating control routes. Returns a plain ok/message
// result and never throws.
export async function postAction(path: string, body?: unknown): Promise<ActionResult> {
  const daemon = await readDaemon();
  if (!daemon) return { ok: false, message: "corrall is not configured." };
  try {
    const res = await call(daemon, "POST", path, body);
    if (res.ok) return { ok: true };
    let message = `HTTP ${res.status}`;
    try {
      const parsed = (await res.json()) as { error?: string };
      if (parsed?.error) message = parsed.error;
    } catch {
      // non-JSON error body; keep the status code
    }
    return { ok: false, message };
  } catch (err) {
    if (unreachable(err)) return { ok: false, message: "corrall is not reachable." };
    return { ok: false, message: err instanceof Error ? err.message : String(err) };
  }
}

// Encodes pool/id for the pool-qualified account control routes.
export function accountPath(pool: string, id: string, action: string): string {
  return `/corrall/pools/${encodeURIComponent(pool)}/accounts/${encodeURIComponent(id)}/${action}`;
}

// A POST that returns the parsed JSON body (login start/submit need the payload,
// not just ok/!ok). On any failure it resolves to `{ ok: false, message }` shaped
// like the daemon's success envelope, so handlers can return it directly.
export async function postJSON<T extends Record<string, unknown>>(
  path: string,
  body: unknown,
): Promise<T & { ok: boolean; message?: string }> {
  const fail = (message: string) => ({ ok: false, message }) as T & { ok: boolean; message?: string };
  const daemon = await readDaemon();
  if (!daemon) return fail("corrall is not configured.");
  try {
    const res = await call(daemon, "POST", path, body);
    const json = (await res.json().catch(() => ({}))) as Record<string, unknown>;
    if (res.ok) {
      return { ok: true, ...(json as T) } as T & { ok: boolean; message?: string };
    }
    const message = typeof json.error === "string" ? json.error : `HTTP ${res.status}`;
    return fail(message);
  } catch (err) {
    if (unreachable(err)) return fail("corrall is not reachable.");
    return fail(err instanceof Error ? err.message : String(err));
  }
}
