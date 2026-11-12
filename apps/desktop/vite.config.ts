import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

// Tauri loads the built files from dist, and the development server serves
// the same files on a fixed port that tauri.conf.json names.
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: { port: 4472, strictPort: true, host: "localhost" },
  build: { target: "safari16", outDir: "dist", emptyOutDir: true },
});
