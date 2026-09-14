// Presentational bits: the gradient utilization bar and the F/S/O family glyphs.

import { useMemo } from "react";
import { Text, View } from "react-native";

import type { PluginTheme } from "@getpaseo/plugin";
import { EXHAUSTED_RED, gradientColorAt, pct } from "./theme";

const CELLS = 20;

export interface UtilizationBarProps {
  theme: PluginTheme;
  label: string;
  util: number; // 0..1, -1 unknown
  // Exhausted paints the whole bar solid red: the account is cooling (5h/7d) or
  // this model-scoped window is spent.
  exhausted?: boolean;
  sublabel?: string | null;
}

export function UtilizationBar({ theme, label, util, exhausted, sublabel }: UtilizationBarProps) {
  const unknown = util < 0;
  const fill = unknown ? 0 : Math.max(0, Math.min(1, util));
  const cells = useMemo(() => {
    const out: string[] = [];
    for (let i = 0; i < CELLS; i++) {
      const filled = i / CELLS < fill;
      if (!filled) {
        out.push(theme.colors.surface2);
      } else if (exhausted) {
        out.push(EXHAUSTED_RED);
      } else {
        out.push(gradientColorAt(i / (CELLS - 1)));
      }
    }
    return out;
  }, [fill, exhausted, theme.colors.surface2]);

  return (
    <View style={{ gap: 3 }}>
      <View style={{ flexDirection: "row", justifyContent: "space-between" }}>
        <Text style={{ color: theme.colors.foregroundMuted, fontSize: 12 }}>{label}</Text>
        <Text
          style={{
            color: exhausted ? EXHAUSTED_RED : theme.colors.foreground,
            fontSize: 12,
            fontVariant: ["tabular-nums"],
          }}
        >
          {pct(util)}
        </Text>
      </View>
      <View style={{ flexDirection: "row", gap: 2, height: 8 }}>
        {cells.map((color, i) => (
          <View key={i} style={{ flex: 1, backgroundColor: color, borderRadius: 1 }} />
        ))}
      </View>
      {sublabel ? <Text style={{ color: theme.colors.foregroundMuted, fontSize: 11 }}>{sublabel}</Text> : null}
    </View>
  );
}

export interface FamilyGlyphsProps {
  theme: PluginTheme;
  // *Active: the family's own weekly window is not spent and the account can
  // serve it now. servable: the account can serve anything at all (available,
  // enabled, logged in) — when false every glyph dims.
  fableActive: boolean;
  sonnetActive: boolean;
  opusActive: boolean;
  servable: boolean;
}

export function FamilyGlyphs({ theme, fableActive, sonnetActive, opusActive, servable }: FamilyGlyphsProps) {
  const glyph = (label: string, active: boolean) => {
    // Green ✓ = servable now; red ✗ = family window spent; dim ✗ = account
    // cannot serve anything for an unrelated reason.
    const ok = servable && active;
    const color = ok ? theme.colors.statusSuccess : servable ? theme.colors.statusDanger : theme.colors.foregroundMuted;
    return (
      <Text style={{ color, fontSize: 12, fontVariant: ["tabular-nums"] }}>
        {label}
        {ok ? "✓" : "✗"}
      </Text>
    );
  };
  return (
    <View style={{ flexDirection: "row", gap: 8 }}>
      {glyph("F", fableActive)}
      {glyph("S", sonnetActive)}
      {glyph("O", opusActive)}
    </View>
  );
}
