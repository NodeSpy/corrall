import type { PluginClientContext } from "@getpaseo/plugin/client";

import { CorrallSurface } from "./client/surface";
import { registerUsagePill } from "./client/pill";

const SURFACE_ID = "corrall";

export default function contribute(client: PluginClientContext) {
  const cleanups: Array<() => void> = [];

  // Wrap every optional registration so one API that a given Paseo build does
  // not support cannot abort the whole contribution — the settings surface must
  // always come up.
  const tryRegister = (label: string, fn: () => (() => void) | void) => {
    try {
      const cleanup = fn();
      if (cleanup) cleanups.push(cleanup);
    } catch (err) {
      // Client console only; the plugin still loads without this affordance.
      console.warn(`[corrall] failed to register ${label}:`, err);
    }
  };

  // The surface is registered so it can be opened from the composer pill, the
  // command centre and the slash command; it lives under Settings rather than
  // as a permanent sidebar item.
  cleanups.push(client.addSurface(SURFACE_ID, CorrallSurface));
  tryRegister("settings-screen", () =>
    client.addSettingsScreen({
      id: SURFACE_ID,
      title: "Corrall",
      icon: "Gauge",
      Component: CorrallSurface,
    }),
  );

  // Composer pill on every agent's chat. The helper manages the per-agent
  // lifecycle and the host-shape difference between Paseo 0.7 and 0.8.
  tryRegister("composer-pill", () => registerUsagePill(client));

  // Command centre: open the usage surface from anywhere.
  tryRegister("command-center:open", () =>
    client.addCommandCenterItem({
      id: "corrall.open",
      title: "Open Corrall usage",
      icon: "Gauge",
      keywords: ["corrall", "usage", "quota", "limit", "proxy", "claude"],
      context: "global",
      onSelect({ openSurface }) {
        openSurface(SURFACE_ID);
      },
    }),
  );

  // Slash command in the composer: `/corrall` opens the surface.
  tryRegister("slash-command", () =>
    client.addSlashCommand({
      name: "corrall",
      description: "Show corrall account usage",
      argumentHint: "",
      context: "agent",
      onSubmit({ openSurface }) {
        openSurface(SURFACE_ID);
      },
    }),
  );

  return () => {
    for (const cleanup of cleanups) cleanup();
  };
}
