import { fileURLToPath } from "node:url";
import { defineConfig } from "vite";
import { sveltekit } from "@sveltejs/kit/vite";
import tailwindcss from "@tailwindcss/vite";

// @ts-expect-error process is a nodejs global
const host = process.env.TAURI_DEV_HOST;

// https://vitejs.dev/config/
export default defineConfig(async () => ({
  plugins: [tailwindcss(), sveltekit()],

  resolve: {
    alias: Object.fromEntries(
      // Plugin frontends live outside ncrs-gui (../plugins via the $plugins
      // alias), where Node resolution can't find our node_modules during the
      // production Rollup build — pin their bare imports to this project's
      // copies. ("svelte" itself is already deduped by vite-plugin-svelte.)
      ["@tauri-apps/api", "@mdi/js"].map((pkg) => [
        pkg,
        fileURLToPath(new URL(`./node_modules/${pkg}`, import.meta.url)),
      ]),
    ),
  },

  // Vite options tailored for Tauri development and only applied in `tauri dev` or `tauri build`
  //
  // 1. prevent vite from obscuring rust errors
  clearScreen: false,
  // 2. tauri expects a fixed port, fail if that port is not available
  server: {
    port: 1420,
    strictPort: true,
    host: host || false,
    hmr: host
      ? {
          protocol: "ws",
          host,
          port: 1421,
        }
      : undefined,
    watch: {
      // 3. tell vite to ignore watching `src-tauri`
      ignored: ["**/src-tauri/**"],
    },
  },
}));
