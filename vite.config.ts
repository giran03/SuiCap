import { defineConfig } from "vite";
import { svelte } from "@sveltejs/vite-plugin-svelte";

// Tauri expects a fixed port and the page sources in ui/.
export default defineConfig({
  root: "ui",
  plugins: [svelte()],
  clearScreen: false,
  server: { port: 1421, strictPort: true, watch: { ignored: ["**/src-tauri/**", "**/target/**"] } },
  build: { outDir: "../dist", emptyOutDir: true, target: "es2022" },
});
