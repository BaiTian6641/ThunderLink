import { defineConfig } from "vite";

export default defineConfig({
  // Carbon web components + lit ship as ES modules; let Vite pre-bundle.
  build: { target: "es2022" },
  server: { port: 5173, strictPort: true },
});
