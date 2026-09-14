// Modals for account/pool management: a manual OAuth "add / re-login account"
// flow, plus "new pool" and "edit pool" forms. Both are controlled (open/onClose)
// and call onDone so the surface can refresh.
//
// Corrall has no single "balancing strategy": a pool's rotation is governed by
// its switch threshold, whether sessions are distributed, and per-account
// priority. So the pool forms edit the threshold + distribute directly.

import { type ReactNode, useCallback, useRef, useState } from "react";
import { ActivityIndicator, Pressable, Text, View } from "react-native";

import type { PluginTheme } from "@getpaseo/plugin";
import { useRpc } from "@getpaseo/plugin/client";
import { Modal, TextInput, copyText, useToast } from "@getpaseo/plugin/client/react-native";

import { loginCancelRpc, loginStartRpc, loginSubmitRpc, poolCreateRpc, poolUpdateRpc } from "../shared/contracts";
import { openExternal } from "./web";

function Field({ children }: { theme?: PluginTheme; children: ReactNode }) {
  return <View style={{ gap: 6 }}>{children}</View>;
}

function Label({ theme, text }: { theme: PluginTheme; text: string }) {
  return <Text style={{ color: theme.colors.foregroundMuted, fontSize: 12 }}>{text}</Text>;
}

function Btn({
  theme,
  label,
  onPress,
  disabled,
  variant = "surface",
  busy,
}: {
  theme: PluginTheme;
  label: string;
  onPress: () => void;
  disabled?: boolean;
  variant?: "surface" | "accent";
  busy?: boolean;
}) {
  const bg = variant === "accent" ? theme.colors.accent : theme.colors.surface2;
  const fg = variant === "accent" ? theme.colors.accentForeground : theme.colors.foreground;
  return (
    <Pressable
      accessibilityRole="button"
      accessibilityLabel={label}
      onPress={onPress}
      disabled={disabled || busy}
      style={{
        flexDirection: "row",
        alignItems: "center",
        justifyContent: "center",
        gap: 6,
        paddingVertical: 9,
        paddingHorizontal: 12,
        borderRadius: 8,
        backgroundColor: bg,
        opacity: disabled || busy ? 0.5 : 1,
      }}
    >
      {busy ? <ActivityIndicator size="small" color={fg} /> : null}
      <Text style={{ color: fg, fontSize: 13, fontWeight: "600" }}>{label}</Text>
    </Pressable>
  );
}

const input = (theme: PluginTheme) => ({
  color: theme.colors.foreground,
  backgroundColor: theme.colors.surface2,
  borderColor: theme.colors.border,
  borderWidth: 1,
  borderRadius: 8,
  paddingVertical: 8,
  paddingHorizontal: 10,
  fontSize: 13,
});

// Parse a "switch at N%" text field into a 0..1 fraction, or undefined when the
// field is blank or unparseable (meaning "leave unchanged" / "daemon default").
function parsePercent(text: string): number | undefined {
  const n = Number.parseInt(text.trim(), 10);
  if (!Number.isFinite(n)) return undefined;
  return Math.min(100, Math.max(1, n)) / 100;
}

export function AddAccountModal({
  theme,
  open,
  pool,
  presetName,
  relogin,
  onClose,
  onDone,
}: {
  theme: PluginTheme;
  open: boolean;
  pool: string;
  presetName?: string;
  relogin?: boolean;
  onClose: () => void;
  onDone: () => void;
}) {
  const callStart = useRpc(loginStartRpc);
  const callSubmit = useRpc(loginSubmitRpc);
  const callCancel = useRpc(loginCancelRpc);
  const toast = useToast();

  const [name, setName] = useState(presetName ?? "");
  const [flowId, setFlowId] = useState<string | null>(null);
  const [url, setUrl] = useState<string | null>(null);
  const [code, setCode] = useState("");
  const [starting, setStarting] = useState(false);
  const [submitting, setSubmitting] = useState(false);

  const reset = useCallback(() => {
    setName(presetName ?? "");
    setFlowId(null);
    setUrl(null);
    setCode("");
    setStarting(false);
    setSubmitting(false);
  }, [presetName]);

  const close = useCallback(() => {
    if (flowId) void callCancel({ flowId });
    reset();
    onClose();
  }, [flowId, callCancel, reset, onClose]);

  const getLink = useCallback(async () => {
    setStarting(true);
    try {
      const r = await callStart({ pool, name: name.trim() || undefined });
      if (r.ok && r.flowId && r.authorizeUrl) {
        setFlowId(r.flowId);
        setUrl(r.authorizeUrl);
      } else {
        toast.error(r.message ?? "Could not start sign-in");
      }
    } catch (err) {
      toast.error(err instanceof Error ? err.message : String(err));
    } finally {
      setStarting(false);
    }
  }, [callStart, pool, name, toast]);

  const submit = useCallback(async () => {
    if (!flowId || !code.trim()) return;
    setSubmitting(true);
    try {
      const r = await callSubmit({ flowId, code: code.trim() });
      if (r.ok) {
        toast.show(
          r.created === false ? `Updated ${r.name ?? "account"}` : `Added ${r.name ?? "account"} to ${pool}`,
          { variant: "success" },
        );
        reset();
        onDone();
        onClose();
      } else {
        toast.error(r.message ?? "Sign-in failed");
      }
    } catch (err) {
      toast.error(err instanceof Error ? err.message : String(err));
    } finally {
      setSubmitting(false);
    }
  }, [flowId, code, callSubmit, pool, toast, reset, onDone, onClose]);

  const title = relogin ? `Re-login ${presetName ?? "account"}` : `Add account to ${pool}`;

  return (
    <Modal title={title} open={open} onOpenChange={(o) => (o ? undefined : close())}>
      <Modal.Content>
        <View style={{ gap: 14 }}>
          {!relogin ? (
            <Field theme={theme}>
              <Label theme={theme} text="Account name (optional — defaults to the email)" />
              <TextInput
                value={name}
                onChangeText={setName}
                placeholder="e.g. work-max"
                autoCapitalize="none"
                editable={!flowId}
                style={input(theme)}
                placeholderTextColor={theme.colors.foregroundMuted}
              />
            </Field>
          ) : (
            <Text style={{ color: theme.colors.foregroundMuted, fontSize: 12 }}>
              Sign in again as {presetName ?? "this account"} to refresh its tokens.
            </Text>
          )}

          {!url ? (
            <Btn theme={theme} label="Get sign-in link" variant="accent" busy={starting} onPress={() => void getLink()} />
          ) : (
            <>
              <Field theme={theme}>
                <Label theme={theme} text="1. Open the sign-in page and approve" />
                <View style={{ flexDirection: "row", gap: 8 }}>
                  <View style={{ flex: 1 }}>
                    <Btn theme={theme} label="Open sign-in page" variant="accent" onPress={() => void openExternal(url)} />
                  </View>
                  <Btn
                    theme={theme}
                    label="Copy link"
                    onPress={() => {
                      void copyText(url);
                      toast.show("Link copied", { variant: "info" });
                    }}
                  />
                </View>
              </Field>
              <Field theme={theme}>
                <Label theme={theme} text="2. Paste the code shown after approving (code#state)" />
                <TextInput
                  value={code}
                  onChangeText={setCode}
                  placeholder="paste code#state here"
                  autoCapitalize="none"
                  style={input(theme)}
                  placeholderTextColor={theme.colors.foregroundMuted}
                />
              </Field>
              <Btn
                theme={theme}
                label={relogin ? "Re-login" : "Add account"}
                variant="accent"
                busy={submitting}
                disabled={!code.trim()}
                onPress={() => void submit()}
              />
            </>
          )}

          <Btn theme={theme} label="Cancel" onPress={close} />
        </View>
      </Modal.Content>
    </Modal>
  );
}

function ThresholdField({ theme, value, onChange }: { theme: PluginTheme; value: string; onChange: (v: string) => void }) {
  return (
    <Field theme={theme}>
      <Label theme={theme} text="Switch threshold — rotate off an account once a window passes this %" />
      <TextInput
        value={value}
        onChangeText={onChange}
        placeholder="e.g. 85"
        keyboardType="number-pad"
        autoCapitalize="none"
        style={input(theme)}
        placeholderTextColor={theme.colors.foregroundMuted}
      />
    </Field>
  );
}

function DistributeToggle({ theme, value, onChange }: { theme: PluginTheme; value: boolean; onChange: (v: boolean) => void }) {
  return (
    <Pressable
      accessibilityRole="switch"
      accessibilityState={{ checked: value }}
      onPress={() => onChange(!value)}
      style={{
        flexDirection: "row",
        alignItems: "center",
        gap: 10,
        paddingVertical: 8,
        paddingHorizontal: 10,
        borderRadius: 8,
        borderWidth: 1,
        borderColor: value ? theme.colors.accent : theme.colors.border,
        backgroundColor: value ? theme.colors.surface2 : "transparent",
      }}
    >
      <View
        style={{
          width: 16,
          height: 16,
          borderRadius: 4,
          borderWidth: 2,
          borderColor: value ? theme.colors.accent : theme.colors.foregroundMuted,
          backgroundColor: value ? theme.colors.accent : "transparent",
        }}
      />
      <View style={{ flexShrink: 1 }}>
        <Text style={{ color: theme.colors.foreground, fontSize: 13 }}>Distribute sessions</Text>
        <Text style={{ color: theme.colors.foregroundMuted, fontSize: 11 }}>
          Spread new sessions across equal-priority accounts instead of reusing one.
        </Text>
      </View>
    </Pressable>
  );
}

export function NewPoolModal({
  theme,
  open,
  onClose,
  onDone,
}: {
  theme: PluginTheme;
  open: boolean;
  onClose: () => void;
  onDone: () => void;
}) {
  const callCreate = useRpc(poolCreateRpc);
  const toast = useToast();
  const [name, setName] = useState("");
  const [threshold, setThreshold] = useState("");
  const [distribute, setDistribute] = useState(false);
  const [busy, setBusy] = useState(false);

  const close = useCallback(() => {
    setName("");
    setThreshold("");
    setDistribute(false);
    setBusy(false);
    onClose();
  }, [onClose]);

  const create = useCallback(async () => {
    if (!name.trim()) return;
    setBusy(true);
    try {
      const r = await callCreate({ name: name.trim(), switchThreshold: parsePercent(threshold), distributeSessions: distribute });
      if (r.ok) {
        toast.show(`Created pool ${name.trim()}`, { variant: "success" });
        close();
        onDone();
      } else {
        toast.error(r.message ?? "Could not create pool");
      }
    } catch (err) {
      toast.error(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  }, [name, threshold, distribute, callCreate, toast, close, onDone]);

  return (
    <Modal title="New pool" open={open} onOpenChange={(o) => (o ? undefined : close())}>
      <Modal.Content>
        <View style={{ gap: 14 }}>
          <Field theme={theme}>
            <Label theme={theme} text="Pool name (lowercase letters, digits, hyphens)" />
            <TextInput
              value={name}
              onChangeText={setName}
              placeholder="e.g. work"
              autoCapitalize="none"
              style={input(theme)}
              placeholderTextColor={theme.colors.foregroundMuted}
            />
          </Field>
          <ThresholdField theme={theme} value={threshold} onChange={setThreshold} />
          <DistributeToggle theme={theme} value={distribute} onChange={setDistribute} />
          <Btn theme={theme} label="Create pool" variant="accent" busy={busy} disabled={!name.trim()} onPress={() => void create()} />
          <Btn theme={theme} label="Cancel" onPress={close} />
        </View>
      </Modal.Content>
    </Modal>
  );
}

export function EditPoolModal({
  theme,
  open,
  pool,
  currentThreshold,
  currentDistribute,
  isDefault,
  onClose,
  onDone,
}: {
  theme: PluginTheme;
  open: boolean;
  pool: string;
  currentThreshold: number;
  currentDistribute: boolean;
  isDefault: boolean;
  onClose: () => void;
  onDone: () => void;
}) {
  const callUpdate = useRpc(poolUpdateRpc);
  const toast = useToast();
  const seededThreshold = String(Math.round(currentThreshold * 100));
  const [name, setName] = useState(pool);
  const [threshold, setThreshold] = useState(seededThreshold);
  const [distribute, setDistribute] = useState(currentDistribute);
  const [busy, setBusy] = useState(false);

  // Re-seed when the target pool changes.
  const seed = `${pool}:${seededThreshold}:${currentDistribute}`;
  const seededRef = useRef(seed);
  if (seededRef.current !== seed) {
    seededRef.current = seed;
    setName(pool);
    setThreshold(seededThreshold);
    setDistribute(currentDistribute);
  }

  const close = useCallback(() => {
    setBusy(false);
    onClose();
  }, [onClose]);

  const save = useCallback(async () => {
    const renamed = !isDefault && name.trim() && name.trim() !== pool;
    const nextThreshold = parsePercent(threshold);
    const changedThreshold = nextThreshold != null && nextThreshold !== currentThreshold;
    const changedDistribute = distribute !== currentDistribute;
    if (!renamed && !changedThreshold && !changedDistribute) {
      close();
      return;
    }
    setBusy(true);
    try {
      const r = await callUpdate({
        name: pool,
        switchThreshold: changedThreshold ? nextThreshold : undefined,
        distributeSessions: changedDistribute ? distribute : undefined,
        newName: renamed ? name.trim() : undefined,
      });
      if (r.ok) {
        toast.show(`Updated pool ${renamed ? name.trim() : pool}`, { variant: "success" });
        onDone();
        onClose();
      } else {
        toast.error(r.message ?? "Could not update pool");
      }
    } catch (err) {
      toast.error(err instanceof Error ? err.message : String(err));
    } finally {
      setBusy(false);
    }
  }, [isDefault, name, pool, threshold, distribute, currentThreshold, currentDistribute, callUpdate, toast, onDone, onClose, close]);

  return (
    <Modal title={`Edit pool ${pool}`} open={open} onOpenChange={(o) => (o ? undefined : close())}>
      <Modal.Content>
        <View style={{ gap: 14 }}>
          {isDefault ? (
            <Text style={{ color: theme.colors.foregroundMuted, fontSize: 12 }}>
              The default pool can't be renamed, but you can change its threshold and distribution.
            </Text>
          ) : (
            <Field theme={theme}>
              <Label theme={theme} text="Pool name (lowercase letters, digits, hyphens)" />
              <TextInput
                value={name}
                onChangeText={setName}
                placeholder={pool}
                autoCapitalize="none"
                style={input(theme)}
                placeholderTextColor={theme.colors.foregroundMuted}
              />
              <Text style={{ color: theme.colors.foregroundMuted, fontSize: 11 }}>
                Renaming resets this pool's live sessions and quota history; update any ANTHROPIC_BASE_URL that pins /pool/{pool}.
              </Text>
            </Field>
          )}
          <ThresholdField theme={theme} value={threshold} onChange={setThreshold} />
          <DistributeToggle theme={theme} value={distribute} onChange={setDistribute} />
          <Btn theme={theme} label="Save changes" variant="accent" busy={busy} onPress={() => void save()} />
          <Btn theme={theme} label="Cancel" onPress={close} />
        </View>
      </Modal.Content>
    </Modal>
  );
}
