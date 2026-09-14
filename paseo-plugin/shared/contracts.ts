// Wire contracts shared by the client surface and the daemon-side handlers.
//
// Corrall's `GET /corrall/status` JSON is camelCase with nested `quota` / `usage`
// objects (src/manager.rs `account_json`, src/pools.rs `status`). Rather than
// spread that shape through the UI, the daemon-side client (server/client.ts)
// normalises it into the flatter, snake_cased view below and every RPC returns
// that. So these schemas describe the *normalised* view, not the raw daemon JSON.

import { defineRpc } from "@getpaseo/plugin";
import { z } from "zod";

// A one-word lifecycle for an account, derived from Corrall's status/blocked/
// disabled fields (there is no single enum on the wire).
export const AccountStateSchema = z.enum([
  "available",
  "cooling",
  "needs_login",
  "disabled",
  "unknown",
]);
export type AccountState = z.infer<typeof AccountStateSchema>;

// A weekly window scoped to one model family (Fable / Sonnet).
export const ModelStatusSchema = z.object({
  label: z.string(),
  utilization: z.number(), // 0..1, -1 unknown
  reset: z.string().optional(),
  spent: z.boolean(),
});
export type ModelStatus = z.infer<typeof ModelStatusSchema>;

// Extra-usage ("spend") dollars, normalised from `quota.spend`. Amounts are
// minor units (e.g. cents); divide by 10**exponent for the display figure.
export const SpendSchema = z.object({
  enabled: z.boolean(),
  used_minor: z.number().optional(),
  limit_minor: z.number().optional(),
  currency: z.string().optional(),
  exponent: z.number(),
  user_disabled: z.boolean().optional(),
  disabled_reason: z.string().optional(),
});
export type Spend = z.infer<typeof SpendSchema>;

// One account within a pool, normalised from Corrall's `account_json`.
export const AccountStatusSchema = z.object({
  id: z.string(), // Corrall's stable account id, used by the control routes
  name: z.string(),
  kind: z.string(), // "oauth" | "apikey"
  state: AccountStateSchema,
  priority: z.number(),
  disabled: z.boolean(),
  utilization_5h: z.number(),
  utilization_7d: z.number(),
  reset_5h: z.string().optional(),
  reset_7d: z.string().optional(),
  models: z.array(ModelStatusSchema).optional(),
  spend: SpendSchema.optional(),
  fable_active: z.boolean(),
  sonnet_active: z.boolean(),
  opus_active: z.boolean(),
  serving: z.boolean(),
  requests: z.number(),
  tier: z.string().optional(),
  last_error: z.string().optional(),
});
export type AccountStatus = z.infer<typeof AccountStatusSchema>;

// One pool. Corrall has no single "balancing" knob — behaviour is the switch
// threshold plus whether sessions are distributed — so both surface here.
export const PoolStatusSchema = z.object({
  name: z.string(),
  default: z.boolean(),
  switch_threshold: z.number(),
  distribute_sessions: z.boolean(),
  sessions: z.number(),
  requests: z.number(),
  accounts: z.array(AccountStatusSchema),
});
export type PoolStatus = z.infer<typeof PoolStatusSchema>;

// The normalised status document the surface renders.
export const StatusResponseSchema = z.object({
  version: z.string(),
  default_pool: z.string(),
  pools: z.array(PoolStatusSchema),
});
export type StatusResponse = z.infer<typeof StatusResponseSchema>;

// Envelope so the surface can distinguish "not configured", "not reachable" and
// "ok" without the RPC ever throwing.
export const StatusEnvelopeSchema = z.object({
  state: z.enum(["ok", "not_configured", "unreachable", "error"]),
  message: z.string().optional(),
  baseUrl: z.string().optional(),
  status: StatusResponseSchema.optional(),
});
export type StatusEnvelope = z.infer<typeof StatusEnvelopeSchema>;

// Result shared by all mutating actions.
export const ActionResultSchema = z.object({
  ok: z.boolean(),
  message: z.string().optional(),
});
export type ActionResult = z.infer<typeof ActionResultSchema>;

const AccountRefSchema = z.object({ pool: z.string(), id: z.string() });

// ── RPC contracts ────────────────────────────────────────────────────────────

export const statusRpc = defineRpc({
  name: "corrall.status",
  input: z.object({}),
  output: StatusEnvelopeSchema,
});

export const enableRpc = defineRpc({
  name: "corrall.enable",
  input: AccountRefSchema,
  output: ActionResultSchema,
});

export const disableRpc = defineRpc({
  name: "corrall.disable",
  input: AccountRefSchema,
  output: ActionResultSchema,
});

export const priorityRpc = defineRpc({
  name: "corrall.priority",
  input: AccountRefSchema.extend({ priority: z.number().int() }),
  output: ActionResultSchema,
});

export const reloadRpc = defineRpc({
  name: "corrall.reload",
  input: z.object({}),
  output: ActionResultSchema,
});

// ── Pool management ──────────────────────────────────────────────────────────

export const poolCreateRpc = defineRpc({
  name: "corrall.pool.create",
  input: z.object({
    name: z.string(),
    switchThreshold: z.number().optional(),
    distributeSessions: z.boolean().optional(),
  }),
  output: ActionResultSchema,
});

// Edit an existing pool: change the switch threshold, session distribution
// and/or rename it. Omitted fields are left unchanged by the daemon.
export const poolUpdateRpc = defineRpc({
  name: "corrall.pool.update",
  input: z.object({
    name: z.string(),
    switchThreshold: z.number().optional(),
    distributeSessions: z.boolean().optional(),
    newName: z.string().optional(),
  }),
  output: ActionResultSchema,
});

// ── Manual OAuth login ───────────────────────────────────────────────────────

// start returns a URL to open plus a flow id; the user pastes the code#state
// the redirect page shows; submit exchanges it daemon-side.
export const loginStartRpc = defineRpc({
  name: "corrall.login.start",
  input: z.object({ pool: z.string(), name: z.string().optional() }),
  output: z.object({
    ok: z.boolean(),
    flowId: z.string().optional(),
    authorizeUrl: z.string().optional(),
    redirectUri: z.string().optional(),
    message: z.string().optional(),
  }),
});

export const loginSubmitRpc = defineRpc({
  name: "corrall.login.submit",
  input: z.object({ flowId: z.string(), code: z.string() }),
  output: z.object({
    ok: z.boolean(),
    name: z.string().optional(),
    created: z.boolean().optional(),
    message: z.string().optional(),
  }),
});

export const loginCancelRpc = defineRpc({
  name: "corrall.login.cancel",
  input: z.object({ flowId: z.string() }),
  output: ActionResultSchema,
});
