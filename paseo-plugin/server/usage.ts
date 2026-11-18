// Corrall as a Paseo usage source (Paseo 0.11+). Every Corrall account becomes
// an account card in Paseo's own Usage modal and sidebar, and a Claude agent
// whose traffic goes through Corrall gets its pool's current account in the
// agent usage popover. Paseo draws the meters; this module only maps
// Corrall's status into windows.

import { readFile } from "node:fs/promises";
import { homedir } from "node:os";
import { join } from "node:path";

import type { PluginServerContext } from "@getpaseo/plugin/server";
import {
  hashAccountKey,
  toneFromUsedPct,
  unavailable,
  windowFromReportedDuration,
  type UsageAccount,
  type UsageBalance,
  type UsageDetail,
  type UsageReport,
  type UsageScope,
  type UsageWindow,
} from "@getpaseo/plugin/server/usage";
import { z } from "zod";

import type { AccountStatus, PoolStatus, StatusEnvelope, StatusResponse } from "../shared/contracts";
import { fetchStatus } from "./client";
import { spendView, stateLabel } from "../shared/format";

const input = z.object({ pool: z.string(), id: z.string() }).strict();
type AccountInput = z.infer<typeof input>;

const FIVE_HOURS = 5 * 3600;
const WEEK = 7 * 86400;

// One discovery or refresh fetches every account at once; share a single
// status read between them rather than hitting the daemon once per card.
let cached: { at: number; env: Promise<StatusEnvelope> } | null = null;
function status(): Promise<StatusEnvelope> {
  const now = Date.now();
  if (!cached || now - cached.at > 2000) cached = { at: now, env: fetchStatus() };
  return cached.env;
}

function accountKey({ pool, id }: AccountInput): string {
  return hashAccountKey(`corrall:${pool}:${id}`);
}

function toAccount(pool: PoolStatus, a: AccountStatus): UsageAccount {
  return { key: accountKey({ pool: pool.name, id: a.id }), label: a.name, harness: "Corrall", input: { pool: pool.name, id: a.id } };
}

// ── Discovery ────────────────────────────────────────────────────────────────

async function discover(scope: UsageScope): Promise<UsageAccount[]> {
  const env = await status();
  if (env.state !== "ok" || !env.status || !env.baseUrl) return [];
  const st = env.status;
  if (scope.kind === "global") {
    return st.pools.flatMap((pool) => pool.accounts.map((a) => toAccount(pool, a)));
  }
  if (scope.provider !== "claude") return [];
  const route = await sessionRoute(scope.env, env.baseUrl, st);
  const pool = route ? st.pools.find((p) => p.name === route.pool) : undefined;
  if (!route || !pool) return [];
  // A pinned session is served by that account. Otherwise Corrall pins each
  // session to an account itself, but the launch environment does not say
  // which session this is, so report what the pool is serving now.
  const pin = route.pin?.toLowerCase();
  const acct =
    (pin ? pool.accounts.find((a) => a.id.toLowerCase() === pin || a.name.toLowerCase() === pin) : undefined) ??
    pool.accounts.find((a) => a.serving) ??
    pool.accounts.find((a) => !a.disabled && a.state === "available");
  return acct ? [toAccount(pool, acct)] : [];
}

// Where a Claude session's traffic lands, or null when it does not go through
// this Corrall. Claude Code reads its user settings' `env` over the shell's,
// and `corrall claude-setup` writes ANTHROPIC_BASE_URL there, so look in both.
async function sessionRoute(
  launch: Record<string, string>,
  baseUrl: string,
  st: StatusResponse,
): Promise<{ pool: string; pin?: string } | null> {
  const env = { ...launch, ...(await settingsEnv(launch)) };
  // With a cloud provider selected Claude Code ignores both routes.
  if (env.CLAUDE_CODE_USE_BEDROCK || env.CLAUDE_CODE_USE_VERTEX) return null;
  const corrall = parse(baseUrl);
  if (!corrall) return null;

  // Base-URL mode: a `/pool/<name>` prefix picks the pool.
  const base = env.ANTHROPIC_BASE_URL ? parse(env.ANTHROPIC_BASE_URL) : null;
  if (base) {
    if (!sameEndpoint(base, corrall)) return null;
    const m = /^\/pool\/([^/]+)/.exec(base.pathname);
    return { pool: m ? decodeURIComponent(m[1]) : st.default_pool };
  }
  // MITM mode: the proxy username carries `[<pin>][~<pool>]`
  // (src/pools.rs split_pin_pool).
  const proxy = env.HTTPS_PROXY ?? env.https_proxy;
  const p = proxy ? parse(proxy) : null;
  if (!p || !sameEndpoint(p, corrall)) return null;
  const user = decodeURIComponent(p.username);
  const sep = user.indexOf("~");
  const pin = (sep < 0 ? user : user.slice(0, sep)).trim();
  const pool = sep < 0 ? "" : user.slice(sep + 1).trim();
  return { pool: pool || st.default_pool, pin: pin || undefined };
}

async function settingsEnv(launch: Record<string, string>): Promise<Record<string, string>> {
  const dir = launch.CLAUDE_CONFIG_DIR || join(launch.HOME || homedir(), ".claude");
  try {
    const raw = JSON.parse(await readFile(join(dir, "settings.json"), "utf8")) as { env?: Record<string, unknown> };
    const out: Record<string, string> = {};
    for (const [k, v] of Object.entries(raw.env ?? {})) if (typeof v === "string") out[k] = v;
    return out;
  } catch {
    return {};
  }
}

function parse(url: string): URL | null {
  try {
    return new URL(url);
  } catch {
    return null;
  }
}

const LOOPBACK = new Set(["127.0.0.1", "localhost", "[::1]", "::1", "0.0.0.0"]);
function sameEndpoint(a: URL, b: URL): boolean {
  const host = (u: URL) => (LOOPBACK.has(u.hostname) ? "loopback" : u.hostname.toLowerCase());
  const port = (u: URL) => u.port || (u.protocol === "https:" ? "443" : "80");
  return host(a) === host(b) && port(a) === port(b);
}

// ── Reports ──────────────────────────────────────────────────────────────────

async function fetchReport(value: unknown): Promise<UsageReport> {
  const ref = input.parse(value);
  const env = await status();
  if (env.state !== "ok" || !env.status) return { status: "error", error: env.message ?? "corrall is not reachable" };
  const pool = env.status.pools.find((p) => p.name === ref.pool);
  const a = pool?.accounts.find((x) => x.id === ref.id);
  // Gone since discovery: throw, and the next discovery drops the card.
  if (!pool || !a) throw new Error(`corrall account ${ref.id} is no longer in pool ${ref.pool}`);
  if (a.state === "needs_login") return unavailable({ kind: "no_quota", detail: "Signed out of corrall; log in again from Corrall settings" });

  const cooling = a.state === "cooling";
  const windows: UsageWindow[] = [
    win({ seconds: FIVE_HOURS, util: a.utilization_5h, reset: a.reset_5h, spent: cooling, summary: true }),
    win({ seconds: WEEK, util: a.utilization_7d, reset: a.reset_7d, spent: cooling, summary: true }),
    ...(a.models ?? []).map((m) =>
      win({ seconds: WEEK, util: m.utilization, reset: m.reset, spent: cooling || m.spent, scope: { id: m.label.toLowerCase(), label: m.label } }),
    ),
  ];

  const details: UsageDetail[] = [
    { id: "pool", label: "Pool", value: pool.name },
    {
      id: "state",
      label: "State",
      value: stateLabel(a.state, a.disabled, a.serving),
      tone: cooling ? "warning" : a.serving ? "ok" : "default",
    },
  ];
  if (a.last_error) details.push({ id: "last_error", label: "Last error", value: a.last_error.slice(0, 120), tone: "danger" });

  const balances: UsageBalance[] = [];
  const spend = a.spend ? spendView(a.spend) : null;
  if (spend && spend.currency === "USD") {
    balances.push({
      id: "extra_usage",
      label: "Extra usage",
      used: spend.used,
      limit: spend.limit && spend.limit > 0 ? spend.limit : null,
      unit: "usd",
      tone: spend.billed ? "warning" : "default",
    });
  } else if (spend) {
    details.push({ id: "extra_usage", label: "Extra usage", value: spend.label, tone: spend.billed ? "warning" : "default" });
  }

  return { status: "available", planLabel: a.tier, windows, balances, details };
}

function win(w: {
  seconds: number;
  util: number; // 0..1, -1 unknown
  reset?: string;
  spent: boolean;
  summary?: boolean;
  scope?: { id: string; label: string };
}): UsageWindow {
  const usedPct = w.util < 0 ? null : Math.round(w.util * 1000) / 10;
  return windowFromReportedDuration({
    durationSeconds: w.seconds,
    scope: w.scope,
    unknown: { id: "rolling", label: "Rolling", shortLabel: "" },
    utilizationPct: usedPct,
    resetsAt: w.reset ?? null,
    summary: w.summary,
    tone: w.spent ? "danger" : toneFromUsedPct(usedPct),
  });
}

export function registerUsageSource(server: PluginServerContext): void {
  server.registerUsageSource({
    id: "corrall",
    label: "Corrall",
    icon: "icon.svg",
    input,
    discover,
    fetch: fetchReport,
  });
}
