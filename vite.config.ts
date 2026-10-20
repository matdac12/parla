import { defineConfig } from "vite";

// The pages live in src/ but build to a flat dist/ (dist/app.html, ...),
// because Rust loads them by name with WebviewUrl::App. src/tests/ is served
// in dev only: it is not a build input.
export default defineConfig({
  clearScreen: false,
  root: "src",
  build: {
    outDir: "../dist",
    emptyOutDir: true,
    rollupOptions: {
      input: {
        main: "src/index.html",
        app: "src/app.html",
        bench: "src/bench.html",
        flyout: "src/flyout.html",
        overlay: "src/overlay.html",
      },
    },
  },
  server: {
    port: 1420,
    strictPort: true,
    watch: { ignored: ["**/src-tauri/**"] },
  },
});
