// Composer-rail usage pill, built on paseo-plugin-helper. The helper registers
// one pill per agent (subscribing to the agent list) and — crucially — knows
// that on Paseo 0.8 the pill body is host-rendered from a static label+icon
// string, so a custom pill component never mounts there. We render a compact
// read-only summary in the tap-through popover.

import { useEffect, useRef, useState } from "react";
import { Pressable, Text, View } from "react-native";

import type { PluginClientContext } from "@getpaseo/plugin/client";
import { useRpc } from "@getpaseo/plugin/client";
import { Icon, Modal, ScrollView, useToast } from "@getpaseo/plugin/client/react-native";
import {
  initClientHelpers,
  registerComposerPill,
  usePluginTheme,
  type ComposerPillRegistrar,
  type RenderModalProps,
} from "paseo-plugin-helper/client";

import type { StatusEnvelope } from "../shared/contracts";
import { statusRpc } from "../shared/contracts";
import { pct } from "./theme";

// Wire the host primitives the helper needs (once per module load).
initClientHelpers({ Icon, Modal, useRpc, useToast });

const POLL_MS = 10000;

// Compact read-only summary shown when the pill is tapped. On 0.8 this renders
// in a narrow anchored popover, so keep it vertically stacked.
function PillModal({ onOpenSettings, close }: RenderModalProps & { onOpenSettings: () => void }) {
  const { colors } = usePluginTheme();
  const callStatus = useRpc(statusRpc);
  const [env, setEnv] = useState<StatusEnvelope | null>(null);
  const mounted = useRef(true);

  useEffect(() => {
    mounted.current = true;
    const tick = async () => {
      try {
        const next = await callStatus({});
        if (mounted.current) setEnv(next);
      } catch {
        /* leave last value */
      }
    };
    void tick();
    const id = setInterval(() => void tick(), POLL_MS);
    return () => {
      mounted.current = false;
      clearInterval(id);
    };
  }, [callStatus]);

  // Keep a fixed height whether loading or loaded: a popover that doesn't
  // resize when data arrives won't get re-anchored below the pill by the host.
  if (!env || env.state !== "ok" || !env.status) {
    return (
      <View style={{ height: PANEL_H, justifyContent: "space-between", gap: 12 }}>
        <View style={{ flex: 1, justifyContent: "center", alignItems: "center" }}>
          <Text style={{ color: colors.foregroundMuted, fontSize: 13, textAlign: "center" }}>
            {env?.message ?? "Loading corrall usage…"}
          </Text>
        </View>
        <OpenSettingsButton colors={colors} onPress={() => { close(); onOpenSettings(); }} />
      </View>
    );
  }

  return (
    <ScrollView style={{ height: PANEL_H }} contentContainerStyle={{ gap: 16, paddingVertical: 2 }} showsVerticalScrollIndicator={false}>
      {env.status.pools.map((pool) => (
        <View key={pool.name} style={{ gap: 2 }}>
          {/* Pool header doubles as the column header, aligned to the value
              columns below. */}
          <View style={{ flexDirection: "row", alignItems: "flex-end", paddingBottom: 4 }}>
            <Text style={{ flex: 1, color: colors.foreground, fontSize: 13, fontWeight: "700" }}>{pool.name}</Text>
            <NumCell text="5h" muted colors={colors} />
            <NumCell text="7d" muted colors={colors} />
          </View>
          {pool.accounts.map((a, i) => (
            <View
              key={a.id}
              style={{
                flexDirection: "row",
                alignItems: "center",
                paddingVertical: 5,
                borderTopWidth: i === 0 ? 0 : 1,
                borderTopColor: colors.surface2,
              }}
            >
              <View style={{ width: 7, height: 7, borderRadius: 4, marginRight: 8, backgroundColor: dotColor(a, colors) }} />
              <Text style={{ flex: 1, color: colors.foreground, fontSize: 12 }} numberOfLines={1}>
                {a.name}
              </Text>
              <NumCell text={pct(a.utilization_5h)} colors={colors} tint={utilColor(a.utilization_5h, colors)} />
              <NumCell text={pct(a.utilization_7d)} colors={colors} tint={utilColor(a.utilization_7d, colors)} />
            </View>
          ))}
        </View>
      ))}
      <OpenSettingsButton colors={colors} onPress={() => { close(); onOpenSettings(); }} />
    </ScrollView>
  );
}

function OpenSettingsButton({
  colors,
  onPress,
}: {
  colors: { accent: string; accentForeground: string };
  onPress: () => void;
}) {
  return (
    <Pressable
      accessibilityRole="button"
      accessibilityLabel="Open Corrall settings"
      onPress={onPress}
      style={{
        flexDirection: "row",
        alignItems: "center",
        justifyContent: "center",
        gap: 6,
        paddingVertical: 9,
        borderRadius: 8,
        backgroundColor: colors.accent,
      }}
    >
      <Icon name="Settings" size={14} color={colors.accentForeground} />
      <Text style={{ color: colors.accentForeground, fontSize: 13, fontWeight: "600" }}>Manage accounts &amp; pools</Text>
    </Pressable>
  );
}

// Fixed popover body height so the anchored popover keeps a stable size (and
// stays above the pill) when the async data loads in.
const PANEL_H = 260;
const COL_W = 46;

// A fixed-width, right-aligned, tabular numeric cell so the % columns line up.
function NumCell({
  text,
  colors,
  muted,
  tint,
}: {
  text: string;
  colors: { foreground: string; foregroundMuted: string };
  muted?: boolean;
  tint?: string;
}) {
  return (
    <Text
      style={{
        width: COL_W,
        textAlign: "right",
        fontSize: muted ? 11 : 12,
        color: tint ?? (muted ? colors.foregroundMuted : colors.foreground),
        fontVariant: ["tabular-nums"],
      }}
    >
      {text}
    </Text>
  );
}

// Muted for normal usage; warns/reds as a window fills so hotspots stand out
// without needing a bar.
function utilColor(
  util: number,
  colors: { foreground: string; foregroundMuted: string; statusWarning: string; statusDanger: string },
): string {
  if (util < 0) return colors.foregroundMuted;
  if (util >= 0.9) return colors.statusDanger;
  if (util >= 0.7) return colors.statusWarning;
  return colors.foreground;
}

function dotColor(
  a: { serving: boolean; state: string; disabled: boolean },
  colors: { statusSuccess: string; statusWarning: string; foregroundMuted: string; border: string },
): string {
  if (a.serving) return colors.statusSuccess;
  if (a.state === "cooling") return colors.statusWarning;
  if (a.disabled || a.state === "needs_login") return colors.foregroundMuted;
  return colors.border;
}

// Registers the usage pill on every agent composer. The helper manages the
// per-agent lifecycle and the 0.7-vs-0.8 host-shape difference. Returns cleanup.
export function registerUsagePill(client: PluginClientContext): () => void {
  // The helper's registrar accepts both the 0.7 (Component) and 0.8 (button)
  // pill shapes and probes the host at runtime; our strict 0.8 client type only
  // advertises the button shape, so bridge the two here. A calm, static
  // launcher: the product name + a gauge icon. The live per-account detail lives
  // in the tap-through summary rather than as a scary percentage on the composer.
  return registerComposerPill(client as unknown as ComposerPillRegistrar, {
    id: "corrall",
    title: "corrall",
    compactTitle: "usage",
    icon: "Gauge",
    modalTitle: "corrall usage",
    modalIcon: "Gauge",
    renderModal: (props) => <PillModal {...props} onOpenSettings={() => client.openSettings("corrall")} />,
  });
}
