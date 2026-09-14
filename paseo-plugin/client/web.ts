import { Linking, Platform } from "react-native";

// Opens a URL in the user's real browser. On web that is a new tab; on the
// desktop/native shell it hands off to the OS. Used for the OAuth sign-in link.
declare const window: { open(url: string, target: string, features: string): unknown };

export async function openExternal(url: string): Promise<void> {
  if (Platform.OS === "web") {
    window.open(url, "_blank", "noopener,noreferrer");
    return;
  }
  await Linking.openURL(url);
}
