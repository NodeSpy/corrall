// Formatting shared by the client surfaces and the daemon-side usage source.
// Plain values only: no React Native or Node imports.

import type { AccountState } from "./contracts";

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
