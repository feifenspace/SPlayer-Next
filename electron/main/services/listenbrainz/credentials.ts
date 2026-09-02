import fs from "node:fs";
import path from "node:path";
import { safeStorage } from "electron";
import { writeFileSync as atomicWriteSync } from "atomically";
import { listenbrainzLog } from "@main/utils/logger";
import { configDir } from "@main/utils/paths";

/** 凭证文件 */
const STORAGE_FILE = path.join(configDir, "listenbrainz.json");

/** 解密后的凭证 */
export interface ListenBrainzCredentials {
  account: string;
  token: string;
}

/** 持久化形态 */
interface PersistedCredentials {
  account: string;
  encryptedToken: string;
}

/** 加密 Token */
const encrypt = (plain: string): string => {
  if (!plain) return "";
  if (!safeStorage.isEncryptionAvailable()) {
    listenbrainzLog.warn("safeStorage 不可用，token 将以 base64 明文落盘");
    return Buffer.from(plain, "utf-8").toString("base64");
  }
  return safeStorage.encryptString(plain).toString("base64");
};

/** 解密 Token */
const decrypt = (encrypted: string): string => {
  if (!encrypted) return "";
  try {
    const buf = Buffer.from(encrypted, "base64");
    if (!safeStorage.isEncryptionAvailable()) {
      return buf.toString("utf-8");
    }
    return safeStorage.decryptString(buf);
  } catch {
    return "";
  }
};

/**
 * 读取本地凭证
 * @returns 凭证；不存在或损坏时返回 null
 */
export const load = (): ListenBrainzCredentials | null => {
  try {
    const raw = JSON.parse(fs.readFileSync(STORAGE_FILE, "utf-8")) as PersistedCredentials;
    const token = decrypt(raw.encryptedToken);
    if (!raw.account || !token) return null;
    return { account: raw.account, token };
  } catch {
    return null;
  }
};

/**
 * 保存凭证到本地
 */
export const save = (creds: ListenBrainzCredentials): boolean => {
  const payload: PersistedCredentials = {
    account: creds.account,
    encryptedToken: encrypt(creds.token),
  };
  try {
    atomicWriteSync(STORAGE_FILE, JSON.stringify(payload, null, 2), { encoding: "utf-8" });
    return true;
  } catch (err) {
    listenbrainzLog.error("写入 listenbrainz.json 失败:", err);
    return false;
  }
};

/**
 * 清除本地凭证
 */
export const clear = (): void => {
  try {
    if (fs.existsSync(STORAGE_FILE)) fs.unlinkSync(STORAGE_FILE);
  } catch (err) {
    listenbrainzLog.error("删除 listenbrainz.json 失败:", err);
  }
};
