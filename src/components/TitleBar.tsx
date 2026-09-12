// Custom window frame: brand + drag region + Windows-style controls.
// Dragging and double-click maximize are handled by Tauri's built-in
// `data-tauri-drag-region` (granted by core:default); the three buttons call
// app commands so they never depend on capability grants. The maximize icon
// stays accurate even for Win+Arrow snap via onResized.
import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import { Logo } from "./Logo";
import { useApp } from "../store";

const win = getCurrentWebviewWindow();

function IconSidebar({ open }: { open: boolean }) {
  return (
    <svg width="13" height="13" viewBox="0 0 13 13" aria-hidden>
      <rect x="0.5" y="1.5" width="12" height="10" rx="1.5" fill="none" stroke="currentColor" />
      <line x1="4.5" y1="1.5" x2="4.5" y2="11.5" stroke="currentColor" />
      {open && <rect x="1" y="2" width="3" height="9" fill="currentColor" opacity="0.45" />}
    </svg>
  );
}

function IconMinimize() {
  return (
    <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden>
      <path d="M0 5 H10" stroke="currentColor" strokeWidth="1" />
    </svg>
  );
}
function IconMaximize({ restored }: { restored: boolean }) {
  if (restored) {
    return (
      <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden>
        <rect x="0.5" y="2.5" width="7" height="7" fill="none" stroke="currentColor" />
        <path d="M2.5 2.5 V0.5 H9.5 V7.5 H7.5" fill="none" stroke="currentColor" />
      </svg>
    );
  }
  return (
    <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden>
      <rect x="0.5" y="0.5" width="9" height="9" fill="none" stroke="currentColor" />
    </svg>
  );
}
function IconClose() {
  return (
    <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden>
      <path d="M0 0 L10 10 M10 0 L0 10" stroke="currentColor" strokeWidth="1" />
    </svg>
  );
}

export function TitleBar() {
  const [maximized, setMaximized] = useState(false);
  const sidebarOpen = useApp((s) => s.sidebarOpen);
  const toggleSidebar = useApp((s) => s.toggleSidebar);

  useEffect(() => {
    let unlisten: (() => void) | undefined;
    let alive = true;
    void win.isMaximized().then((v) => alive && setMaximized(v)).catch(() => {});
    void win
      .onResized(async () => {
        try {
          alive && setMaximized(await win.isMaximized());
        } catch {
          /* window gone */
        }
      })
      .then((f) => {
        // the subscription promise may resolve AFTER unmount ran the cleanup
        // — storing `f` then would leak the listener forever; unlisten now
        if (alive) unlisten = f;
        else f();
      })
      .catch(() => {});
    return () => {
      alive = false;
      unlisten?.();
    };
  }, []);

  return (
    <header className="titlebar">
      <div className="tb-brand" data-tauri-drag-region title="">
        <button
          className={`tb-btn tb-side ${sidebarOpen ? "" : "off"}`}
          title={sidebarOpen ? "收拢侧边栏" : "展开侧边栏"}
          aria-label={sidebarOpen ? "收拢侧边栏" : "展开侧边栏"}
          onClick={toggleSidebar}
        >
          <IconSidebar open={sidebarOpen} />
        </button>
        <Logo size={18} />
        <span className="tb-name">
          <span className="cc">CC</span>Harness
        </span>
      </div>
      <div className="tb-drag" data-tauri-drag-region />
      <div className="tb-controls">
        <button
          className="tb-btn"
          title="最小化"
          aria-label="最小化"
          onClick={() => void invoke("window_minimize")}
        >
          <IconMinimize />
        </button>
        <button
          className="tb-btn"
          title={maximized ? "还原" : "最大化"}
          aria-label={maximized ? "还原" : "最大化"}
          onClick={async () => setMaximized(await invoke<boolean>("window_toggle_maximize"))}
        >
          <IconMaximize restored={maximized} />
        </button>
        <button
          className="tb-btn close"
          title="关闭"
          aria-label="关闭"
          onClick={() => void invoke("window_close")}
        >
          <IconClose />
        </button>
      </div>
    </header>
  );
}
