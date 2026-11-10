import { rm } from "node:fs/promises";
import { fileURLToPath, URL } from "node:url";
import { defineConfig, type Plugin } from "vite";
import vue from "@vitejs/plugin-vue";

const srcDir = fileURLToPath(new URL("./src", import.meta.url));
// `VITE_MOCK=1` swaps the IPC bridge for the in-memory double. Doing it with an
// alias (instead of a runtime ternary) keeps mock fixtures out of release builds.
const mock = process.env.VITE_MOCK === "1";

// `public/dev-screen.json` is a mock-review aid (see src/store/app.ts); keep it
// out of the release bundle instead of shipping a stray file.
function dropDevScreen(): Plugin {
  return {
    name: "drop-dev-screen",
    apply: "build",
    closeBundle: async () => {
      await rm(fileURLToPath(new URL("./dist/dev-screen.json", import.meta.url)), { force: true });
    },
  };
}

// Tauri expects a fixed port and no clearing of the screen.
export default defineConfig({
  plugins: [vue(), dropDevScreen()],
  clearScreen: false,
  resolve: {
    alias: [
      ...(mock
        ? [
            {
              find: /^@\/lib\/backend$/,
              replacement: fileURLToPath(new URL("./src/lib/backend.mock.ts", import.meta.url)),
            },
          ]
        : []),
      { find: "@", replacement: srcDir },
    ],
  },
  server: {
    port: 5183,
    strictPort: true,
    watch: { ignored: ["**/src-tauri/**"] },
  },
  build: {
    target: "esnext",
    minify: "esbuild",
    cssMinify: "esbuild",
  },
});
