// Thin RPC handlers: validate input (Zod already did), call the daemon client,
// return the result. All network/error handling lives in ./client.

import type { ActionResult, StatusEnvelope } from "../shared/contracts";
import { accountPath, fetchStatus, postAction, postJSON } from "./client";

interface AccountRef {
  pool: string;
  id: string;
}

export function status(): Promise<StatusEnvelope> {
  return fetchStatus();
}

export function enable({ pool, id }: AccountRef): Promise<ActionResult> {
  return postAction(accountPath(pool, id, "enable"));
}

export function disable({ pool, id }: AccountRef): Promise<ActionResult> {
  return postAction(accountPath(pool, id, "disable"));
}

export function priority({ pool, id, priority: value }: AccountRef & { priority: number }): Promise<ActionResult> {
  return postAction(accountPath(pool, id, "priority"), { priority: value });
}

export function reload(): Promise<ActionResult> {
  return postAction("/corrall/reload");
}

// ── Pool management ──────────────────────────────────────────────────────────

export function poolCreate({
  name,
  switchThreshold,
  distributeSessions,
}: {
  name: string;
  switchThreshold?: number;
  distributeSessions?: boolean;
}): Promise<ActionResult> {
  return postAction("/corrall/pools", { name, switchThreshold, distributeSessions });
}

export function poolUpdate({
  name,
  switchThreshold,
  distributeSessions,
  newName,
}: {
  name: string;
  switchThreshold?: number;
  distributeSessions?: boolean;
  newName?: string;
}): Promise<ActionResult> {
  return postAction(`/corrall/pools/${encodeURIComponent(name)}`, {
    switchThreshold,
    distributeSessions,
    newName: newName ?? "",
  });
}

// ── Manual OAuth login ───────────────────────────────────────────────────────

export async function loginStart({ pool, name }: { pool: string; name?: string }): Promise<{
  ok: boolean;
  flowId?: string;
  authorizeUrl?: string;
  redirectUri?: string;
  message?: string;
}> {
  const r = await postJSON<{
    flow_id?: string;
    authorize_url?: string;
    redirect_uri?: string;
  }>("/corrall/login/start", { pool, name });
  return {
    ok: r.ok,
    flowId: r.flow_id,
    authorizeUrl: r.authorize_url,
    redirectUri: r.redirect_uri,
    message: r.message,
  };
}

export async function loginSubmit({ flowId, code }: { flowId: string; code: string }): Promise<{
  ok: boolean;
  name?: string;
  created?: boolean;
  message?: string;
}> {
  const r = await postJSON<{ name?: string; created?: boolean }>("/corrall/login/submit", {
    flow_id: flowId,
    code,
  });
  return { ok: r.ok, name: r.name, created: r.created, message: r.message };
}

export function loginCancel({ flowId }: { flowId: string }): Promise<ActionResult> {
  return postAction("/corrall/login/cancel", { flow_id: flowId });
}
