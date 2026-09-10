import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";
import path from "node:path";

export default defineConfig({
  plugins: [react(), tailwindcss()],
  resolve: { alias: { "@": path.resolve(__dirname, "./src") } },
  server: {
    port: 5180,
    // The API this console speaks to: the contract server by default, a real
    // cell with `VELSTRA_API=https://host:8443 npm run dev` (self-signed is fine).
    proxy: { "/api": { target: process.env.VELSTRA_API ?? "http://127.0.0.1:18300", changeOrigin: true, secure: false, ws: true } },
  },
});
