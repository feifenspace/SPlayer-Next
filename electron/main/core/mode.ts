/** 是否以 Linux 无头服务模式运行 */
export const isHeadless = (): boolean =>
  process.argv.includes("--headless") || process.env.SPLAYER_HEADLESS === "1";
