import type { ConfigApi } from "@shared/types/settings";

/** 浏览器文件操作遵循共享配置契约，备份结构由调用方负责。 */
export const webConfigFiles: Pick<ConfigApi, "exportToFile" | "importFromFile"> = {
  exportToFile: async (payload) => {
    let url: string | undefined;
    try {
      const blob = new Blob([JSON.stringify(payload, null, 2)], {
        type: "application/json",
      });
      url = URL.createObjectURL(blob);
      const link = document.createElement("a");
      link.href = url;
      link.download = `splayer-config-${Date.now()}.json`;
      link.click();
      return { ok: true };
    } catch {
      return { ok: false, reason: "writeFailed" };
    } finally {
      if (url) URL.revokeObjectURL(url);
    }
  },
  importFromFile: () =>
    new Promise((resolve) => {
      const input = document.createElement("input");
      input.type = "file";
      input.accept = ".json";
      input.oncancel = () => resolve({ ok: false, reason: "canceled" });
      input.onchange = () => {
        const file = input.files?.[0];
        if (!file) {
          resolve({ ok: false, reason: "canceled" });
          return;
        }
        const reader = new FileReader();
        reader.onerror = () => resolve({ ok: false, reason: "readFailed" });
        reader.onabort = () => resolve({ ok: false, reason: "readFailed" });
        reader.onload = () => {
          try {
            resolve({ ok: true, data: JSON.parse(reader.result as string) });
          } catch {
            resolve({ ok: false, reason: "parseFailed" });
          }
        };
        try {
          reader.readAsText(file);
        } catch {
          resolve({ ok: false, reason: "readFailed" });
        }
      };
      try {
        input.click();
      } catch {
        resolve({ ok: false, reason: "readFailed" });
      }
    }),
};
