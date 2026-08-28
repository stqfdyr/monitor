import { defineConfig } from "vite"
import react from "@vitejs/plugin-react"
import tailwindcss from "@tailwindcss/vite"

export default defineConfig({
  // Served under /admin/ so its hashed assets cannot collide with a theme's.
  base: "/admin/",
  plugins: [react(), tailwindcss()],
  // import.meta.dirname, not new URL(...).pathname: the latter is URL-encoded,
  // so a checkout under a path with a space or a non-ASCII name resolves to
  // %20 and the alias silently points nowhere.
  resolve: { alias: { "@": import.meta.dirname + "/src" } },
  build: { chunkSizeWarningLimit: 900 },
  server: { proxy: { "/api": { target: "http://127.0.0.1:9911", ws: true } } },
})
