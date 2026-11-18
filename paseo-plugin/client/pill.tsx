// Composer-rail usage pill. One pill per agent composer, collapsed to a live
// mini-gauge icon; tapping it opens a popover laid out like `corrall attach`:
// per account, a 10-cell bar for each window (5h, 7d, 7d per model) with the
// percentage and the reset countdown, and ► on the account currently serving.

import { useEffect, useMemo, useState, useSyncExternalStore } from "react";
import { Pressable, Text, View } from "react-native";

import type { PluginTheme } from "@getpaseo/plugin";
import type {
  PluginButton,
  PluginButtonContentProps,
  PluginButtonIconProps,
  PluginButtonRegistration,
  PluginClientContext,
} from "@getpaseo/plugin/client";
import { Icon } from "@getpaseo/plugin/client/ui";

import type { AccountStatus, StatusEnvelope, StatusResponse } from "../shared/contracts";
import { statusRpc } from "../shared/contracts";
import { EXHAUSTED_RED, gradientColorAt, pct, stateLabel } from "./theme";

const POLL_MS = 10000;

// Paseo always prints a composer pill's label (falling back to the title) and
// rejects a blank one. A word joiner survives its trim but renders nothing, so
// the pill collapses to just the icon; `title` stays the accessible name.
const ICON_ONLY_LABEL = "⁠";

// ── Shared status poller ─────────────────────────────────────────────────────
// Every pill icon and popover reads one poller, so N open composers cost one
// request per interval, and it only runs while something is mounted.

type Fetch = () => Promise<StatusEnvelope>;

function createStatusStore(fetch: Fetch) {
  let env: StatusEnvelope | null = null;
  let timer: ReturnType<typeof setInterval> | null = null;
  const listeners = new Set<() => void>();

  const tick = async () => {
    try {
      env = await fetch();
    } catch (err) {
      env = { state: "error", message: err instanceof Error ? err.message : String(err) };
    }
    for (const l of listeners) l();
  };

  return {
    subscribe(listener: () => void) {
      listeners.add(listener);
      if (!timer) {
        void tick();
        timer = setInterval(() => void tick(), POLL_MS);
      }
      return () => {
        listeners.delete(listener);
        if (listeners.size === 0 && timer) {
          clearInterval(timer);
          timer = null;
        }
      };
    },
    get: () => env,
    stop() {
      listeners.clear();
      if (timer) clearInterval(timer);
      timer = null;
    },
  };
}

type StatusStore = ReturnType<typeof createStatusStore>;

function useStatus(store: StatusStore): StatusEnvelope | null {
  return useSyncExternalStore(store.subscribe, store.get, store.get);
}

// The account the header of `corrall attach` calls "current": the serving
// account of the default pool, else any serving account.
function currentAccount(status: StatusResponse): AccountStatus | null {
  const def = status.pools.find((p) => p.default) ?? status.pools[0];
  return (
    def?.accounts.find((a) => a.serving) ??
    status.pools.flatMap((p) => p.accounts).find((a) => a.serving) ??
    null
  );
}

// "2h8m" / "6d18h" / "45m" / "-", the terse countdown `corrall attach` prints.
function shortReset(iso: string | undefined, now: number): string {
  if (!iso) return "-";
  const t = Date.parse(iso);
  if (Number.isNaN(t)) return "-";
  const secs = Math.round((t - now) / 1000);
  if (secs <= 0) return "now";
  const d = Math.floor(secs / 86400);
  const h = Math.floor((secs % 86400) / 3600);
  const m = Math.floor((secs % 3600) / 60);
  if (d > 0) return h ? `${d}d${h}h` : `${d}d`;
  if (h > 0) return `${h}h${m}m`;
  if (m > 0) return `${m}m`;
  return `${secs}s`;
}

// ── Pill icon: a two-bar live gauge ─────────────────────────────────────────

// Two vertical bars filling from the bottom: the current account's 5h and 7d
// windows, coloured along the attach gradient. Falls back to a plain gauge
// glyph until data arrives or when Corrall is unreachable.
function GaugeIcon({ store, size, color, theme }: PluginButtonIconProps & { store: StatusStore }) {
  const env = useStatus(store);
  const acct = env?.state === "ok" && env.status ? currentAccount(env.status) : null;
  if (!acct) return <Icon name="Gauge" size={size} color={color} />;

  const exhausted = acct.state === "cooling";
  const barW = Math.max(3, Math.round(size * 0.3));
  const bar = (util: number) => {
    const f = util < 0 ? 0 : Math.max(0.08, Math.min(1, util));
    return (
      <View
        style={{
          width: barW,
          height: size,
          borderRadius: 1.5,
          backgroundColor: theme.colors.surface2,
          justifyContent: "flex-end",
          overflow: "hidden",
        }}
      >
        <View
          style={{
            height: Math.round(size * f),
            backgroundColor: exhausted ? EXHAUSTED_RED : util < 0 ? color : gradientColorAt(f),
          }}
        />
      </View>
    );
  };
  return (
    <View style={{ width: size, height: size, flexDirection: "row", justifyContent: "center", gap: 2 }}>
      {bar(acct.utilization_5h)}
      {bar(acct.utilization_7d)}
    </View>
  );
}

// ── Popover: an attach-style table ───────────────────────────────────────────
// One aligned row per account with the window headings printed once, so every
// figure is there without a label on each bar. A reset countdown replaces the
// percentage only where it matters: a window 70%+ full, or an account cooling.

const BAR_W = 36;
const NUM_W = 38;
const CELL_W = BAR_W + 6 + NUM_W;
const HOT = 0.7;
// Keeps the name column readable: the popover sizes to its content.
const NAME_MIN_W = 104;

function SlimBar({ util, exhausted, theme }: { util: number; exhausted: boolean; theme: PluginTheme }) {
  const f = util < 0 ? 0 : Math.max(0, Math.min(1, util));
  return (
    <View style={{ width: BAR_W, height: 4, borderRadius: 2, backgroundColor: theme.colors.surface2, overflow: "hidden" }}>
      <View
        style={{
          width: `${f > 0 ? Math.max(8, f * 100) : 0}%`,
          height: 4,
          backgroundColor: exhausted ? EXHAUSTED_RED : gradientColorAt(f),
        }}
      />
    </View>
  );
}

function Cell({
  util,
  reset,
  exhausted,
  now,
  theme,
}: {
  util: number;
  reset?: string;
  exhausted: boolean;
  now: number;
  theme: PluginTheme;
}) {
  const hot = exhausted || util >= HOT;
  const text = hot && reset ? `↻${shortReset(reset, now)}` : pct(util);
  const color = exhausted ? EXHAUSTED_RED : hot ? theme.colors.statusWarning : theme.colors.foregroundMuted;
  return (
    <View style={{ width: CELL_W, flexDirection: "row", alignItems: "center", gap: 6 }}>
      <SlimBar util={util} exhausted={exhausted} theme={theme} />
      <Text style={{ width: NUM_W, color, fontSize: 11, textAlign: "right", fontVariant: ["tabular-nums"] }} numberOfLines={1}>
        {text}
      </Text>
    </View>
  );
}

function Row({
  a,
  modelLabel,
  now,
  theme,
}: {
  a: AccountStatus;
  modelLabel: string | null;
  now: number;
  theme: PluginTheme;
}) {
  const cooling = a.state === "cooling";
  const off = a.disabled || a.state === "needs_login";
  const note = cooling || off ? stateLabel(a.state, a.disabled, a.serving) : null;
  const model = modelLabel ? a.models?.find((m) => m.label === modelLabel) : undefined;
  return (
    <View style={{ flexDirection: "row", alignItems: "center", gap: 6, paddingVertical: 5, opacity: off ? 0.5 : 1 }}>
      <Text style={{ width: 10, color: theme.colors.statusSuccess, fontSize: 10 }}>{a.serving ? "►" : ""}</Text>
      <Text
        style={{ flex: 1, minWidth: NAME_MIN_W, color: theme.colors.foreground, fontSize: 12, fontWeight: a.serving ? "600" : "400" }}
        numberOfLines={1}
      >
        {shortName(a.name)}
        {note ? <Text style={{ color: cooling ? theme.colors.statusWarning : theme.colors.foregroundMuted, fontWeight: "400" }}>{`  ${note}`}</Text> : null}
      </Text>
      <Cell util={a.utilization_5h} reset={a.reset_5h} exhausted={cooling} now={now} theme={theme} />
      <Cell util={a.utilization_7d} reset={a.reset_7d} exhausted={cooling} now={now} theme={theme} />
      {modelLabel ? (
        model ? (
          <Cell util={model.utilization} reset={model.reset} exhausted={cooling || model.spent} now={now} theme={theme} />
        ) : (
          <View style={{ width: CELL_W }} />
        )
      ) : null}
    </View>
  );
}

// "daniel.claude.2@ednition.com" → "daniel.claude.2"; the pool heading already
// says which fleet it belongs to.
function shortName(name: string): string {
  const at = name.indexOf("@");
  return at > 0 ? name.slice(0, at) : name;
}

function UsagePopover({
  store,
  theme,
  close,
  onOpenSettings,
}: PluginButtonContentProps & { store: StatusStore; onOpenSettings: () => void }) {
  const env = useStatus(store);
  // Re-render the countdowns between polls.
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    setNow(Date.now());
    const id = setInterval(() => setNow(Date.now()), 30000);
    return () => clearInterval(id);
  }, [env]);

  const status = env?.state === "ok" ? env.status : undefined;
  const muted = theme.colors.foregroundMuted;
  // The per-model weekly column, like attach's "7d fable": the first model
  // window any account reports.
  const modelLabel = useMemo(
    () => status?.pools.flatMap((p) => p.accounts).find((a) => a.models?.length)?.models?.[0]?.label ?? null,
    [status],
  );
  const heading = (text: string, align: "left" | "center" = "center", width?: number) => (
    <Text
      style={{
        width,
        flex: width ? undefined : 1,
        minWidth: width ? undefined : NAME_MIN_W,
        color: muted,
        fontSize: 10,
        fontWeight: "600",
        letterSpacing: 0.5,
        textTransform: "uppercase",
        textAlign: align,
      }}
      numberOfLines={1}
    >
      {text}
    </Text>
  );

  return (
    <View style={{ gap: 12 }}>
      {status ? (
        status.pools.map((pool, i) => (
          <View key={pool.name}>
            <View style={{ flexDirection: "row", alignItems: "center", gap: 6, paddingBottom: 2 }}>
              <View style={{ width: 10 }} />
              {heading(pool.name, "left")}
              {heading(i === 0 ? "5h" : "", "center", CELL_W)}
              {heading(i === 0 ? "7d" : "", "center", CELL_W)}
              {modelLabel ? heading(i === 0 ? modelLabel : "", "center", CELL_W) : null}
            </View>
            {pool.accounts.map((a) => (
              <Row key={a.id} a={a} modelLabel={modelLabel} now={now} theme={theme} />
            ))}
          </View>
        ))
      ) : (
        <Text style={{ color: muted, fontSize: 13, textAlign: "center", paddingVertical: 24 }}>
          {env?.message ?? "Loading corrall usage…"}
        </Text>
      )}
      <Pressable
        accessibilityRole="button"
        accessibilityLabel="Open Corrall settings"
        onPress={() => {
          close();
          onOpenSettings();
        }}
        style={{ flexDirection: "row", alignItems: "center", justifyContent: "center", gap: 6, paddingVertical: 4 }}
      >
        <Icon name="Settings" size={12} color={muted} />
        <Text style={{ color: muted, fontSize: 11 }}>Manage accounts &amp; pools</Text>
      </Pressable>
    </View>
  );
}

// ── Registration ─────────────────────────────────────────────────────────────

// Registers the usage pill on every agent composer, following the agent list.
// Returns cleanup.
export function registerUsagePill(client: PluginClientContext, settingsId: string): () => void {
  const store = createStatusStore(() => client.rpc(statusRpc, {}));
  // agentId → pill and the workspace it was registered in.
  const pills = new Map<string, { pill: PluginButtonRegistration<string>; workspaceId: string }>();
  const lifetime = new AbortController();

  // Stable component identities: the host remounts on a new component type.
  const PillIcon = (props: PluginButtonIconProps) => <GaugeIcon {...props} store={store} />;
  const Content = (props: PluginButtonContentProps) => (
    <UsagePopover {...props} store={store} onOpenSettings={() => client.openSettings(settingsId)} />
  );
  const button: PluginButton<string> = {
    title: "Corrall usage",
    label: ICON_ONLY_LABEL,
    icon: PillIcon,
    behavior: { kind: "popover", Content },
  };

  const register = (agent: { id: string; workspaceId?: string | null }) => {
    if (lifetime.signal.aborted || !agent.workspaceId) return;
    // Agent updates arrive constantly; only (re)register when the agent is new
    // or moved workspace, so the pill doesn't remount on every status change.
    if (pills.get(agent.id)?.workspaceId === agent.workspaceId) return;
    drop(agent.id);
    const pill = client.addComposerPill({ id: "corrall", workspaceId: agent.workspaceId, agentId: agent.id, button });
    pills.set(agent.id, { pill, workspaceId: agent.workspaceId });
  };
  const drop = (agentId: string) => {
    pills.get(agentId)?.pill.remove();
    pills.delete(agentId);
  };
  const removeAll = () => {
    for (const { pill } of pills.values()) pill.remove();
    pills.clear();
  };

  void client.paseo.agents
    .list({ subscribe: {}, signal: lifetime.signal })
    .then(({ subscription }) => {
      subscription.subscribe({
        snapshot: ({ entries }) => {
          const live = new Set(entries.map(({ agent }) => agent.id));
          for (const id of [...pills.keys()]) if (!live.has(id)) drop(id);
          for (const { agent } of entries) register(agent);
        },
        update: (message) => {
          if (message.type !== "agent_update") return;
          const update = message.payload;
          if (update.kind === "upsert") register(update.agent);
          else drop(update.agentId);
        },
      });
      return undefined;
    })
    .catch((error) => {
      if (!lifetime.signal.aborted) console.warn("[corrall] agent observation failed", error);
    });

  return () => {
    lifetime.abort();
    removeAll();
    store.stop();
  };
}
