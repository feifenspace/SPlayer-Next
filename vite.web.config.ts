import { resolve } from "node:path";
import { defineConfig } from "vite";

/** 复用上游 renderer 配置，仅构建 Headless 控制台入口。 */
export default defineConfig(async () => {
  process.env.SPLAYER_WEB_BUILD = "1";
  const { default: electronConfig } = await import("./electron.vite.config");
  const renderer = electronConfig.renderer ?? {};
  return {
    ...renderer,
    base: "/",
    build: {
      ...renderer.build,
      outDir: "out/renderer",
      emptyOutDir: true,
      rollupOptions: {
        ...renderer.build?.rollupOptions,
        input: resolve("index.html"),
      },
    },
  };
});
