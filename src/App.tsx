import { useEffect, useState, type CSSProperties } from "react";
import { listen } from "@tauri-apps/api/event";
import { useApp } from "./store";
import * as api from "./lib/api";
import { TitleBar } from "./components/TitleBar";
import { Sidebar } from "./components/Sidebar";
import { Toasts } from "./components/Toast";
import { CommandPalette } from "./components/CommandPalette";
import { ApprovalCards } from "./components/ApprovalCards";
import { CloseAskDialog } from "./components/Dialog";
import { ChatView } from "./views/ChatView";
import { ArenaView } from "./views/ArenaView";
import { ModelsView } from "./views/ModelsView";
import { McpView } from "./views/McpView";
import { MarketView } from "./views/MarketView";
import { TelemetryView } from "./views/TelemetryView";
import { BenchView } from "./views/BenchView";
import { ReviewView } from "./views/ReviewView";
import { SettingsView } from "./views/SettingsView";
import { PreviewPanel } from "./components/PreviewPanel";
import { SubagentsView } from "./views/SubagentsView";
import { WorkflowView } from "./views/WorkflowView";

export function App() {
  // per-field selectors: the ROOT component re-rendering on every stream
  // delta cascades through TitleBar and every unmemoized child (P1 —
  // ChatView/Sidebar/ArenaView were split earlier; this was the holdout)
  const view = useApp((s) => s.view);
  const bootstrap = useApp((s) => s.bootstrap);
  const sidebarOpen = useApp((s) => s.sidebarOpen);
  const runUpdateCheck = useApp((s) => s.runUpdateCheck);
  const previewOpen = useApp((s) => s.previewOpen);
  const panelW = useApp((s) => s.panelW);
  const [closeAsk, setCloseAsk] = useState(false);

  useEffect(() => {
    void bootstrap();
  }, [bootstrap]);

  // silent startup update check, throttled to once per day; a found
  // update only lights the sidebar badge + a toast (manual download by design)
  useEffect(() => {
    if (localStorage.getItem("cc.autoUpdateCheck") === "off") return;
    const CHECK_INTERVAL = 24 * 60 * 60 * 1000;
    const last = Number(localStorage.getItem("cc.lastUpdateCheck") ?? 0);
    if (Date.now() - last < CHECK_INTERVAL) return;
    localStorage.setItem("cc.lastUpdateCheck", String(Date.now()));
    void runUpdateCheck(false);
  }, [runUpdateCheck]);

  // Rust intercepts the close button when settings.close_action === "ask"
  // and emits this event — show the tray-or-quit dialog.
  useEffect(() => {
    const unlisten = listen("close-ask", () => setCloseAsk(true));
    return () => {
      void unlisten.then((f) => f());
    };
  }, []);

  return (
    <div className="app">
      <TitleBar />
      <div
        className={`app-body ${previewOpen ? "panel-open" : ""} ${sidebarOpen ? "" : "sb-collapsed"}`}
        style={{ "--panel-w": `${panelW}px` } as CSSProperties}
      >
        <Sidebar />
        <main className="main fade-in" key={view}>
          {view === "chat" && <ChatView />}
          {view === "arena" && <ArenaView />}
          {view === "models" && <ModelsView />}
          {view === "subagents" && <SubagentsView />}
          {view === "workflows" && <WorkflowView />}
          {view === "mcp" && <McpView />}
          {view === "market" && <MarketView />}
          {view === "telemetry" && <TelemetryView />}
          {view === "bench" && <BenchView />}
          {view === "review" && <ReviewView />}
          {view === "settings" && <SettingsView />}
        </main>
        <PreviewPanel />
      </div>
      <CommandPalette />
      <ApprovalCards />
      <Toasts />
      {closeAsk && (
        <CloseAskDialog
          onTray={() => {
            setCloseAsk(false);
            void api.hideToTray();
          }}
          onQuit={() => {
            setCloseAsk(false);
            void api.quitApp();
          }}
          onCancel={() => setCloseAsk(false)}
        />
      )}
    </div>
  );
}
