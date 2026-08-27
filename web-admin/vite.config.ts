import { defineConfig } from "vite"
import react from "@vitejs/plugin-react"
import tailwindcss from "@tailwindcss/vite"

export default defineConfig({
  // Served under /admin/ so its hashed assets cannot collide with a theme's.
  base: "/admin/",
  plugins: [react(), tailwindcss()],
  resolve: { alias: { "@": new URL("./src", import.meta.url).pathname } },
  build: { chunkSizeWarningLimit: 900 },
  server: { proxy: { "/api": { target: "http://127.0.0.1:9911", ws: true } } },
})
