// Colour maths and formatting shared by the surface, bars and pill. Kept free
// of react-native imports so it is trivially testable.

import type { AccountState } from "../shared/contracts";

// The gradient stops mirror the TUI: green at 0%, yellow at 60%, red at 100%,
// so a bar's tip reddens as its window fills (README "TUI colour rules").
const GREEN: RGB = [63, 185, 80]; // #3fb950
const YELLOW: RGB = [210, 153, 34]; // #d29922
const RED: RGB = [248, 81, 73]; // #f85149

type RGB = [number, number, number];

function lerp(a: number, b: number, t: number): number {
  return Math.round(a + (b - a) * t);
}

function mix(a: RGB, b: RGB, t: number): RGB {
  return [lerp(a[0], b[0], t), lerp(a[1], b[1], t), lerp(a[2], b[2], t)];
}

function hex([r, g, b]: RGB): string {
  return `#${[r, g, b].map((v) => v.toString(16).padStart(2, "0")).join("")}`;
}

// Colour at position `frac` (0..1) along the green→yellow→red gradient, with
// the yellow knee at 0.6 to match the TUI.
export function gradientColorAt(frac: number): string {
  const f = Math.max(0, Math.min(1, frac));
  if (f <= 0.6) return hex(mix(GREEN, YELLOW, f / 0.6));
  return hex(mix(YELLOW, RED, (f - 0.6) / 0.4));
}

export const EXHAUSTED_RED = hex(RED);

// Distinct hues used to tell pools apart at a glance — cycled by pool index for
// the header spine and the accounts' grouping border. Chosen to stay legible on
// both light and dark surfaces.
const POOL_ACCENTS = [
  "#6e8bff", // indigo
  "#3fb9a8", // teal
  "#d9a441", // amber
  "#d96fb0", // pink
  "#8bbf5a", // green
  "#a679e0", // purple
];
export function poolAccent(index: number): string {
  return POOL_ACCENTS[((index % POOL_ACCENTS.length) + POOL_ACCENTS.length) % POOL_ACCENTS.length];
}

// Threshold band for at-a-glance colouring (pill, spend). danger ≥90%,
// warning ≥70%, otherwise neutral/ok.
export type Band = "ok" | "warn" | "danger";
export function band(util: number): Band {
  if (util >= 0.9) return "danger";
  if (util >= 0.7) return "warn";
  return "ok";
}

export function bandColor(
  b: Band,
  colors: { statusSuccess: string; statusWarning: string; statusDanger: string },
): string {
  return b === "danger"
    ? colors.statusDanger
    : b === "warn"
      ? colors.statusWarning
      : colors.statusSuccess;
}

// "72%" — utilization is 0..1; -1 means unknown.
export function pct(util: number): string {
  if (util < 0) return "—";
  return `${Math.round(util * 100)}%`;
}

// A short "resets in 3h 12m" / "resets in 45s" from an RFC3339 timestamp.
export function resetsIn(iso: string | undefined, now: number): string | null {
  if (!iso) return null;
  const t = Date.parse(iso);
  if (Number.isNaN(t)) return null;
  const secs = Math.round((t - now) / 1000);
  if (secs <= 0) return "resets now";
  const d = Math.floor(secs / 86400);
  const h = Math.floor((secs % 86400) / 3600);
  const m = Math.floor((secs % 3600) / 60);
  if (d > 0) return `resets in ${d}d ${h}h`;
  if (h > 0) return `resets in ${h}h ${m}m`;
  if (m > 0) return `resets in ${m}m`;
  return `resets in ${secs}s`;
}

// A one-word status label for an account, matching the TUI/`status` vocabulary.
export function stateLabel(state: AccountState, disabled: boolean, serving: boolean): string {
  if (disabled) return "disabled";
  if (state === "cooling") return "cooling";
  if (state === "needs_login") return "needs login";
  if (serving) return "active";
  if (state === "available") return "ready";
  return state;
}

// Spend rendering. Amounts are minor units scaled by exponent (e.g. cents,
// exponent 2). Mirrors `corrall status`: an explicit limit of 0 means extra
// usage is switched off for the account whatever `enabled` reports, so it is
// never a "$X of $0.00" bill; money already spent is still shown once billing
// has been turned off; and nothing is shown when billing is off and nothing
// was ever spent.
//
// The tone is the band to paint: "billed" once real money is going out,
// "warn" while billing is merely enabled or was on earlier this month, "off"
// when extra usage is disabled outright.
export type SpendTone = "off" | "warn" | "billed";
export interface SpendView {
  used: number;
  limit: number | null; // the configured cap; null when none is set
  currency: string;
  billed: boolean; // real money is being billed right now (enabled, used > 0)
  tone: SpendTone;
  label: string;
}
export function spendView(spend: {
  enabled: boolean;
  used_minor?: number;
  limit_minor?: number;
  currency?: string;
  exponent: number;
  user_disabled?: boolean;
  disabled_reason?: string;
}): SpendView | null {
  const scale = 10 ** (spend.exponent ?? 0);
  const used = (spend.used_minor ?? 0) / scale;
  const spent = used > 0;
  if (!spend.enabled && !spent) return null;
  const limitOff = spend.limit_minor === 0;
  const limit = spend.limit_minor != null ? spend.limit_minor / scale : null;
  const currency = spend.currency || "USD";
  const sym = currency === "USD" ? "$" : `${currency} `;
  const fmt = (n: number) => `${sym}${n.toFixed(2)}`;
  // A zero limit is "no cap", not a cap of nothing.
  const amount = limit != null && limit > 0 ? `${fmt(used)} of ${fmt(limit)}` : fmt(used);
  if (limitOff) {
    const label = spent ? `extra usage disabled — ${amount} used this month` : "extra usage disabled";
    return { used, limit, currency, billed: false, tone: "off", label };
  }
  if (spend.enabled) {
    const label = spent
      ? `billing real money — ${amount} used this month`
      : `extra-usage billing enabled${limit != null ? ` — up to ${fmt(limit)}` : ""}`;
    return { used, limit, currency, billed: spent, tone: spent ? "billed" : "warn", label };
  }
  // Off now, but it was on earlier this month: say what went out and why it stopped.
  const why = spend.user_disabled
    ? "now disabled by the account holder"
    : spend.disabled_reason
      ? `now off (${spend.disabled_reason.slice(0, 40)})`
      : "now off";
  return { used, limit, currency, billed: false, tone: "warn", label: `${amount} spent this month, ${why}` };
}

// The window that will bounce a session first: the highest 5h utilization among
// enabled accounts across all pools. Returns null when there is nothing to show.
export function highestSessionUtil(
  pools: { accounts: { disabled: boolean; utilization_5h: number }[] }[],
): number | null {
  let max: number | null = null;
  for (const pool of pools) {
    for (const acct of pool.accounts) {
      if (acct.disabled) continue;
      if (max == null || acct.utilization_5h > max) max = acct.utilization_5h;
    }
  }
  return max;
}
