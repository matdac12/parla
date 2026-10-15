import { defineConfig } from "vite";

export default defineConfig({
  clearScreen: false,
  build: {
    rollupOptions: {
      input: {
        main: "index.html",
        settings: "settings.html",
        history: "history.html",
        bench: "bench.html",
        overlay: "overlay.html",
      },
    },
  },
  server: {
    port: 1420,
    strictPort: true,
    watch: { ignored: ["**/src-tauri/**"] },
  },
});
