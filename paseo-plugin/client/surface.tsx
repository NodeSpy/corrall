// The "Corrall" settings surface: live per-pool / per-account usage with
// gradient bars, spend and family flags, plus per-account enable / disable /
// priority controls and pool management. Polls GET /corrall/status (via the
// daemon-side RPC) on an interval, matching the TUI's cadence.

import { useCallback, useEffect, useRef, useState } from "react";
import { ActivityIndicator, Pressable, Text, View } from "react-native";

import type { PluginTheme } from "@getpaseo/plugin";
import type { PluginSurfaceProps } from "@getpaseo/plugin/client";
import { useRpc } from "@getpaseo/plugin/client";
import { Icon, ScrollView, useToast } from "@getpaseo/plugin/client/react-native";

import type { AccountStatus, PoolStatus, StatusEnvelope } from "../shared/contracts";
import { disableRpc, enableRpc, priorityRpc, statusRpc } from "../shared/contracts";
import { FamilyGlyphs, UtilizationBar } from "./bars";
import { AddAccountModal, EditPoolModal, NewPoolModal } from "./manage";
import { EXHAUSTED_RED, poolAccent, resetsIn, spendView, stateLabel } from "./theme";

// Which management modal is open, if any. Lifted to the surface so a single
// modal instance is driven from any pool/account button.
type ModalState =
  | { kind: "add"; pool: string; presetName?: string; relogin?: boolean }
  | { kind: "editPool"; pool: string; threshold: number; distribute: boolean; isDefault: boolean }
  | { kind: "newPool" }
  | null;

const POLL_MS = 10000;

export function CorrallSurface({ theme, layout }: PluginSurfaceProps) {
  const callStatus = useRpc(statusRpc);
  const [env, setEnv] = useState<StatusEnvelope | null>(null);
  const [loading, setLoading] = useState(true);
  const [now, setNow] = useState(() => Date.now());
  const [modal, setModal] = useState<ModalState>(null);
  const mounted = useRef(true);

  const refresh = useCallback(async () => {
    try {
      const next = await callStatus({});
      if (mounted.current) setEnv(next);
    } catch (err) {
      if (mounted.current) {
        setEnv({ state: "error", message: err instanceof Error ? err.message : String(err) });
      }
    } finally {
      if (mounted.current) {
        setLoading(false);
        setNow(Date.now());
      }
    }
  }, [callStatus]);

  useEffect(() => {
    mounted.current = true;
    void refresh();
    const id = setInterval(() => void refresh(), POLL_MS);
    return () => {
      mounted.current = false;
      clearInterval(id);
    };
  }, [refresh]);

  const pad = layout.compact ? 12 : 20;

  const reachable = env?.state === "ok";
  const openAdd = (pool: string, opts?: { presetName?: string; relogin?: boolean }) => setModal({ kind: "add", pool, ...opts });

  return (
    <View style={{ flex: 1, backgroundColor: theme.colors.surface0 }}>
      <Header
        theme={theme}
        env={env}
        loading={loading}
        onRefresh={() => void refresh()}
        onNewPool={reachable ? () => setModal({ kind: "newPool" }) : undefined}
        pad={pad}
      />
      <ScrollView style={{ flex: 1 }} contentContainerStyle={{ padding: pad, paddingTop: 0, gap: 28 }}>
        {env == null ? null : env.state === "ok" && env.status ? (
          env.status.pools.length === 0 ? (
            <Notice theme={theme} icon="Inbox" text="No pools configured yet." />
          ) : (
            env.status.pools.map((pool, i) => (
              <PoolCard
                key={pool.name}
                theme={theme}
                pool={pool}
                index={i}
                now={now}
                onChanged={() => void refresh()}
                onAddAccount={openAdd}
                onEditPool={() =>
                  setModal({
                    kind: "editPool",
                    pool: pool.name,
                    threshold: pool.switch_threshold,
                    distribute: pool.distribute_sessions,
                    isDefault: pool.default,
                  })
                }
              />
            ))
          )
        ) : env.state === "not_configured" ? (
          <Notice theme={theme} icon="Settings" text={env.message ?? "corrall is not configured."} />
        ) : env.state === "unreachable" ? (
          <Notice theme={theme} icon="PlugZap" text={env.message ?? "corrall is not reachable."} />
        ) : (
          <Notice theme={theme} icon="TriangleAlert" text={env.message ?? "Something went wrong."} tone="danger" />
        )}
      </ScrollView>

      <AddAccountModal
        theme={theme}
        open={modal?.kind === "add"}
        pool={modal?.kind === "add" ? modal.pool : ""}
        presetName={modal?.kind === "add" ? modal.presetName : undefined}
        relogin={modal?.kind === "add" ? modal.relogin : undefined}
        onClose={() => setModal(null)}
        onDone={() => void refresh()}
      />
      <NewPoolModal theme={theme} open={modal?.kind === "newPool"} onClose={() => setModal(null)} onDone={() => void refresh()} />
      <EditPoolModal
        theme={theme}
        open={modal?.kind === "editPool"}
        pool={modal?.kind === "editPool" ? modal.pool : ""}
        currentThreshold={modal?.kind === "editPool" ? modal.threshold : 0.8}
        currentDistribute={modal?.kind === "editPool" ? modal.distribute : false}
        isDefault={modal?.kind === "editPool" ? modal.isDefault : false}
        onClose={() => setModal(null)}
        onDone={() => void refresh()}
      />
    </View>
  );
}

function Header({
  theme,
  env,
  loading,
  onRefresh,
  onNewPool,
  pad,
}: {
  theme: PluginTheme;
  env: StatusEnvelope | null;
  loading: boolean;
  onRefresh: () => void;
  onNewPool?: () => void;
  pad: number;
}) {
  const version = env?.status?.version;
  return (
    <View style={{ flexDirection: "row", alignItems: "center", justifyContent: "space-between", padding: pad, gap: 12 }}>
      <View style={{ flexDirection: "row", alignItems: "center", gap: 8, flexShrink: 1 }}>
        <Icon name="Gauge" size={18} color={theme.colors.foreground} />
        <Text style={{ color: theme.colors.foreground, fontSize: 16, fontWeight: "600" }}>Corrall</Text>
        {version ? <Text style={{ color: theme.colors.foregroundMuted, fontSize: 12 }}>v{version}</Text> : null}
      </View>
      <View style={{ flexDirection: "row", alignItems: "center", gap: 8 }}>
        {onNewPool ? <ActionButton theme={theme} icon="FolderPlus" label="New pool" onPress={onNewPool} /> : null}
        <Pressable
          accessibilityRole="button"
          accessibilityLabel="Refresh"
          onPress={onRefresh}
          style={{ padding: 6, borderRadius: 6, backgroundColor: theme.colors.surface2 }}
        >
          {loading ? (
            <ActivityIndicator size="small" color={theme.colors.foreground} />
          ) : (
            <Icon name="RefreshCw" size={16} color={theme.colors.foreground} />
          )}
        </Pressable>
      </View>
    </View>
  );
}

function PoolCard({
  theme,
  pool,
  index,
  now,
  onChanged,
  onAddAccount,
  onEditPool,
}: {
  theme: PluginTheme;
  pool: PoolStatus;
  index: number;
  now: number;
  onChanged: () => void;
  onAddAccount: (pool: string, opts?: { presetName?: string; relogin?: boolean }) => void;
  onEditPool: () => void;
}) {
  const accent = poolAccent(index);
  return (
    <View
      style={{
        gap: 10,
        // A hairline divider above every pool after the first, with extra top
        // padding, so consecutive pools read as clearly separate blocks.
        borderTopWidth: index > 0 ? 1 : 0,
        borderTopColor: theme.colors.border,
        paddingTop: index > 0 ? 20 : 0,
      }}
    >
      {/* Header banner: accent spine + name + settings, so each pool reads as a
          distinct block rather than another row of similar text. */}
      <View
        style={{
          borderLeftWidth: 3,
          borderLeftColor: accent,
          borderRadius: 8,
          backgroundColor: theme.colors.surface1,
          paddingVertical: 8,
          paddingHorizontal: 10,
          gap: 4,
        }}
      >
        <View style={{ flexDirection: "row", alignItems: "center", gap: 8, flexWrap: "wrap" }}>
          <View style={{ width: 9, height: 9, borderRadius: 5, backgroundColor: accent }} />
          <Text style={{ color: theme.colors.foreground, fontSize: 15, fontWeight: "700" }}>{pool.name}</Text>
          {pool.default ? <Chip theme={theme} label="default" /> : null}
          {pool.distribute_sessions ? <Chip theme={theme} label="distribute" accent={accent} /> : null}
        </View>
        <View style={{ flexDirection: "row", alignItems: "center", justifyContent: "space-between", gap: 8 }}>
          <Text style={{ color: theme.colors.foregroundMuted, fontSize: 12, flexShrink: 1 }}>
            {pool.accounts.length} account{pool.accounts.length === 1 ? "" : "s"} · {pool.sessions} session
            {pool.sessions === 1 ? "" : "s"} · {pool.requests} req · switch @ {Math.round(pool.switch_threshold * 100)}%
          </Text>
          <View style={{ flexDirection: "row", alignItems: "center", gap: 8 }}>
            <ActionButton theme={theme} icon="Pencil" label="Edit" onPress={onEditPool} />
            <ActionButton theme={theme} icon="UserPlus" label="Add account" onPress={() => onAddAccount(pool.name)} />
          </View>
        </View>
      </View>
      {/* Accounts grouped under an accent spine so it is obvious which pool they
          belong to. */}
      {pool.accounts.length === 0 ? (
        <Text style={{ color: theme.colors.foregroundMuted, fontSize: 12, paddingLeft: 12 }}>No accounts.</Text>
      ) : (
        <View style={{ borderLeftWidth: 2, borderLeftColor: accent, paddingLeft: 10, gap: 10 }}>
          {pool.accounts.map((acct) => (
            <AccountCard
              key={acct.id}
              theme={theme}
              pool={pool.name}
              acct={acct}
              now={now}
              onChanged={onChanged}
              onRelogin={() => onAddAccount(pool.name, { presetName: acct.name, relogin: true })}
            />
          ))}
        </View>
      )}
    </View>
  );
}

function AccountCard({
  theme,
  pool,
  acct,
  now,
  onChanged,
  onRelogin,
}: {
  theme: PluginTheme;
  pool: string;
  acct: AccountStatus;
  now: number;
  onChanged: () => void;
  onRelogin: () => void;
}) {
  const callEnable = useRpc(enableRpc);
  const callDisable = useRpc(disableRpc);
  const callPriority = useRpc(priorityRpc);
  const toast = useToast();
  const [busy, setBusy] = useState(false);

  const cooling = acct.state === "cooling";
  const needsLogin = acct.state === "needs_login";
  const servable = acct.state === "available" && !acct.disabled;
  const spend = acct.spend ? spendView(acct.spend) : null;

  const run = useCallback(
    async (label: string, fn: () => Promise<{ ok: boolean; message?: string }>) => {
      setBusy(true);
      try {
        const res = await fn();
        if (res.ok) {
          toast.show(`${label} ${acct.name}`, { variant: "success" });
        } else {
          toast.error(res.message ? `${label} failed: ${res.message}` : `${label} failed`);
        }
      } catch (err) {
        toast.error(err instanceof Error ? err.message : String(err));
      } finally {
        setBusy(false);
        onChanged();
      }
    },
    [toast, acct.name, onChanged],
  );

  return (
    <View
      style={{
        backgroundColor: theme.colors.surface1,
        borderColor: theme.colors.border,
        borderWidth: 1,
        borderRadius: 10,
        padding: 12,
        gap: 10,
      }}
    >
      <View style={{ flexDirection: "row", alignItems: "center", justifyContent: "space-between", gap: 8 }}>
        <View style={{ flexDirection: "row", alignItems: "center", gap: 8, flexShrink: 1 }}>
          <View
            style={{
              width: 8,
              height: 8,
              borderRadius: 4,
              backgroundColor: acct.serving
                ? theme.colors.statusSuccess
                : cooling
                  ? theme.colors.statusWarning
                  : acct.disabled
                    ? theme.colors.foregroundMuted
                    : theme.colors.border,
            }}
          />
          <Text style={{ color: theme.colors.foreground, fontSize: 14, fontWeight: "600" }}>{acct.name}</Text>
          <Chip theme={theme} label={stateLabel(acct.state, acct.disabled, acct.serving)} />
          <Text style={{ color: theme.colors.foregroundMuted, fontSize: 11 }}>
            #{acct.priority} · {acct.kind}
          </Text>
        </View>
        <FamilyGlyphs theme={theme} fableActive={acct.fable_active} sonnetActive={acct.sonnet_active} opusActive={acct.opus_active} servable={servable} />
      </View>

      <UtilizationBar theme={theme} label="Session (5h)" util={acct.utilization_5h} exhausted={cooling} sublabel={resetsIn(acct.reset_5h, now)} />
      <UtilizationBar theme={theme} label="Weekly (7d)" util={acct.utilization_7d} exhausted={cooling} sublabel={resetsIn(acct.reset_7d, now)} />
      {(acct.models ?? []).map((m, i) => (
        <UtilizationBar key={`${m.label}-${i}`} theme={theme} label={`7d ${m.label}`} util={m.utilization} exhausted={m.spent} sublabel={resetsIn(m.reset, now)} />
      ))}

      {spend ? (
        <Text
          style={{
            color: spend.tone === "billed" ? EXHAUSTED_RED : spend.tone === "warn" ? theme.colors.statusWarning : theme.colors.foregroundMuted,
            fontSize: 12,
          }}
        >
          {spend.tone === "off" ? "" : "⚠ "}
          {spend.label}
        </Text>
      ) : null}

      {acct.last_error ? (
        <Text style={{ color: EXHAUSTED_RED, fontSize: 11 }} numberOfLines={2}>
          {acct.last_error}
        </Text>
      ) : null}

      <View style={{ flexDirection: "row", alignItems: "center", gap: 8, flexWrap: "wrap" }}>
        <ActionButton theme={theme} icon="LogIn" label="Re-login" emphasize={needsLogin} disabled={busy} onPress={onRelogin} />
        {acct.disabled ? (
          <ActionButton
            theme={theme}
            icon="Play"
            label="Enable"
            disabled={busy}
            onPress={() => void run("Enabled", () => callEnable({ pool, id: acct.id }))}
          />
        ) : (
          <ActionButton
            theme={theme}
            icon="Pause"
            label="Disable"
            disabled={busy}
            onPress={() => void run("Disabled", () => callDisable({ pool, id: acct.id }))}
          />
        )}
        <View style={{ flexDirection: "row", alignItems: "center", gap: 4 }}>
          <ActionButton
            theme={theme}
            icon="ChevronUp"
            label="Higher"
            disabled={busy}
            onPress={() => void run("Reprioritized", () => callPriority({ pool, id: acct.id, priority: acct.priority - 1 }))}
          />
          <ActionButton
            theme={theme}
            icon="ChevronDown"
            label="Lower"
            disabled={busy}
            onPress={() => void run("Reprioritized", () => callPriority({ pool, id: acct.id, priority: acct.priority + 1 }))}
          />
        </View>
      </View>
    </View>
  );
}

function ActionButton({
  theme,
  icon,
  label,
  onPress,
  disabled,
  emphasize,
}: {
  theme: PluginTheme;
  icon: string;
  label: string;
  onPress: () => void;
  disabled?: boolean;
  // Draw attention (e.g. Re-login on a needs_login account).
  emphasize?: boolean;
}) {
  const fg = emphasize ? theme.colors.accentForeground : theme.colors.foreground;
  return (
    <Pressable
      accessibilityRole="button"
      accessibilityLabel={label}
      onPress={onPress}
      disabled={disabled}
      style={{
        flexDirection: "row",
        alignItems: "center",
        gap: 5,
        paddingVertical: 6,
        paddingHorizontal: 10,
        borderRadius: 7,
        backgroundColor: emphasize ? theme.colors.accent : theme.colors.surface2,
        opacity: disabled ? 0.5 : 1,
      }}
    >
      <Icon name={icon} size={14} color={fg} />
      <Text style={{ color: fg, fontSize: 12 }}>{label}</Text>
    </Pressable>
  );
}

function Chip({
  theme,
  label,
  accent,
}: {
  theme: PluginTheme;
  label: string;
  // When set, the chip is outlined in the accent colour instead of a flat fill —
  // used to carry a pool's hue onto its chips.
  accent?: string;
}) {
  return (
    <View
      style={{
        paddingVertical: 2,
        paddingHorizontal: 7,
        borderRadius: 999,
        backgroundColor: accent ? "transparent" : theme.colors.surface2,
        borderWidth: accent ? 1 : 0,
        borderColor: accent ?? "transparent",
      }}
    >
      <Text style={{ color: accent ?? theme.colors.foregroundMuted, fontSize: 11 }}>{label}</Text>
    </View>
  );
}

function Notice({
  theme,
  icon,
  text,
  tone,
}: {
  theme: PluginTheme;
  icon: string;
  text: string;
  tone?: "danger";
}) {
  const color = tone === "danger" ? theme.colors.statusDanger : theme.colors.foregroundMuted;
  return (
    <View
      style={{
        flexDirection: "row",
        alignItems: "center",
        gap: 10,
        padding: 16,
        borderRadius: 10,
        borderWidth: 1,
        borderColor: theme.colors.border,
        backgroundColor: theme.colors.surface1,
      }}
    >
      <Icon name={icon} size={18} color={color} />
      <Text style={{ color: theme.colors.foreground, fontSize: 13, flexShrink: 1 }}>{text}</Text>
    </View>
  );
}
