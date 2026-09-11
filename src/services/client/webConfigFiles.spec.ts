import { afterEach, describe, expect, it, vi } from "vitest";
import { webConfigFiles } from "./webConfigFiles";

afterEach(() => vi.restoreAllMocks());

const selectFile = (text: string) => {
  vi.spyOn(HTMLInputElement.prototype, "click").mockImplementation(function (
    this: HTMLInputElement,
  ) {
    Object.defineProperty(this, "files", { value: [new File([text], "backup.json")] });
    this.dispatchEvent(new Event("change"));
  });
};

describe("Web 配置文件契约", () => {
  it("导入保留备份类型及主进程、前端设置，不重复包装 main", async () => {
    const payload = { type: "backup", main: { volume: 0.5 }, renderer: { settings: {} } };
    selectFile(JSON.stringify(payload));
    expect(await webConfigFiles.importFromFile()).toEqual({ ok: true, data: payload });
  });

  it("无效 JSON 返回解析失败", async () => {
    selectFile("{invalid");
    expect(await webConfigFiles.importFromFile()).toEqual({ ok: false, reason: "parseFailed" });
  });

  it("取消文件选择返回取消", async () => {
    vi.spyOn(HTMLInputElement.prototype, "click").mockImplementation(function (
      this: HTMLInputElement,
    ) {
      this.dispatchEvent(new Event("cancel"));
    });
    expect(await webConfigFiles.importFromFile()).toEqual({ ok: false, reason: "canceled" });
  });

  it("读取异常返回失败而非一直等待", async () => {
    selectFile("{}");
    vi.spyOn(FileReader.prototype, "readAsText").mockImplementation(() => {
      throw new Error("read failed");
    });
    expect(await webConfigFiles.importFromFile()).toEqual({ ok: false, reason: "readFailed" });
  });

  it("导出触发下载并返回调用方需要的 ok", async () => {
    vi.spyOn(URL, "createObjectURL").mockReturnValue("blob:backup");
    const revoke = vi.spyOn(URL, "revokeObjectURL").mockImplementation(() => {});
    const click = vi.spyOn(HTMLAnchorElement.prototype, "click").mockImplementation(() => {});
    expect(await webConfigFiles.exportToFile({ main: {} })).toEqual({ ok: true });
    expect(click).toHaveBeenCalledOnce();
    expect(revoke).toHaveBeenCalledWith("blob:backup");
  });

  it("下载触发失败也释放临时 URL", async () => {
    vi.spyOn(URL, "createObjectURL").mockReturnValue("blob:backup");
    const revoke = vi.spyOn(URL, "revokeObjectURL").mockImplementation(() => {});
    vi.spyOn(HTMLAnchorElement.prototype, "click").mockImplementation(() => {
      throw new Error("download failed");
    });
    expect(await webConfigFiles.exportToFile({})).toEqual({ ok: false, reason: "writeFailed" });
    expect(revoke).toHaveBeenCalledWith("blob:backup");
  });
});
