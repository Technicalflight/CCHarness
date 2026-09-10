// System notifications (tauri-plugin-notification): used by the store to
// surface turn completion / approval requests while the window is hidden.
import { isPermissionGranted, requestPermission, sendNotification } from "@tauri-apps/plugin-notification";

let cached: boolean | null = null;

async function granted(): Promise<boolean> {
  if (cached !== null) return cached;
  try {
    let ok = await isPermissionGranted();
    if (!ok) {
      const req = await requestPermission();
      ok = req === "granted";
    }
    cached = ok;
  } catch {
    cached = false;
  }
  return cached;
}

/** Fire a system notification; silently no-ops without permission. */
export async function notify(title: string, body: string): Promise<void> {
  if (!(await granted())) return;
  try {
    sendNotification({ title, body });
  } catch {
    /* best-effort */
  }
}

/** Only notify when the window is hidden — a focused app doesn't need it. */
export function notifyIfHidden(title: string, body: string): void {
  if (document.hidden) void notify(title, body);
}
