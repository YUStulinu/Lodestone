/// <reference types="vitest/config" />
import { defineConfig } from "vite";

// In development the viewer's API is lodestone-pg's HTTP side.
export default defineConfig({
  server: { proxy: { "/api": "http://127.0.0.1:8088" } },
  test: { environment: "node" },
});
