import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Vite config tuned for Tauri development:
// - fixed port so tauri.conf.json devUrl stays valid
// - strictPort avoids silently rebinding when 5173 is taken
// - src-tauri/** is excluded from the watcher: Rust rebuild locks target/
//   artifacts (EBUSY on Windows) and Rust-side reloads are driven by
//   `tauri dev` itself, not by Vite
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    port: 5173,
    strictPort: true,
    watch: {
      ignored: ["**/src-tauri/**"],
    },
  },
  build: {
    target: "es2021",
    minify: "esbuild",
    sourcemap: false,
  },
});
