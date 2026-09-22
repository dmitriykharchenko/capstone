import { defineConfig } from "vite";
import { svelte } from "@sveltejs/vite-plugin-svelte";
import tailwindcss from "@tailwindcss/vite";
import { pdfjsAssetsPlugin } from "./scripts/pdfjs-assets.mjs";

export default defineConfig({
  envDir: false,
  envPrefix: [],
  plugins: [tailwindcss(), svelte(), pdfjsAssetsPlugin()],
  resolve: {
    alias: {
      $lib: decodeURIComponent(new URL("./src/lib", import.meta.url).pathname),
    },
  },
  clearScreen: false,
  server: {
    port: 5173,
    strictPort: true,
    host: "127.0.0.1",
    fs: {
      strict: true,
      allow: [decodeURIComponent(new URL(".", import.meta.url).pathname)],
      deny: [".env", ".env.*", "*.{crt,pem,key,p12,pfx,cer,der}", ".npmrc", ".yarnrc.yml",
        "**/.git/**", "**/backend/**", "**/electron/**", "**/scripts/**"],
    },
    watch: {
      ignored: ["**/backend/**"],
    },
  },
});
