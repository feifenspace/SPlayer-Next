import localforage from "localforage";
import type {
  StreamingLibrarySnapshot,
  StreamingRuntimeConfig,
  StreamingServerConfig,
} from "@shared/types/streaming";

const store = localforage.createInstance({
  name: "splayer",
  storeName: "streaming",
});

const SERVERS_KEY = "streaming_servers";
const ACTIVE_SERVER_KEY = "streaming_active_server_id";
const SNAPSHOT_PREFIX = "streaming_snapshot_";

export interface PersistedServerRecord {
  id: string;
  name: string;
  type: StreamingServerConfig["type"];
  url: string;
  username: string;
  password: string;
  lastConnected?: number;
}

export const loadPersistedServers = async (): Promise<PersistedServerRecord[]> => {
  try {
    const list = await store.getItem<PersistedServerRecord[]>(SERVERS_KEY);
    return Array.isArray(list) ? list : [];
  } catch {
    return [];
  }
};

export const savePersistedServers = async (servers: PersistedServerRecord[]): Promise<void> => {
  try {
    await store.setItem(SERVERS_KEY, servers);
  } catch (err) {
    console.error("[StreamingWebStorage] savePersistedServers error:", err);
  }
};

export const loadActiveServerId = async (): Promise<string | null> => {
  try {
    const id = await store.getItem<string>(ACTIVE_SERVER_KEY);
    return typeof id === "string" ? id : null;
  } catch {
    return null;
  }
};

export const saveActiveServerId = async (id: string | null): Promise<void> => {
  try {
    if (id) {
      await store.setItem(ACTIVE_SERVER_KEY, id);
    } else {
      await store.removeItem(ACTIVE_SERVER_KEY);
    }
  } catch (err) {
    console.error("[StreamingWebStorage] saveActiveServerId error:", err);
  }
};

export const loadServerSnapshot = async (
  serverId: string,
): Promise<StreamingLibrarySnapshot | null> => {
  try {
    const snapshot = await store.getItem<StreamingLibrarySnapshot>(`${SNAPSHOT_PREFIX}${serverId}`);
    return snapshot ?? null;
  } catch {
    return null;
  }
};

export const saveServerSnapshot = async (
  serverId: string,
  snapshot: StreamingLibrarySnapshot,
): Promise<void> => {
  try {
    await store.setItem(`${SNAPSHOT_PREFIX}${serverId}`, snapshot);
  } catch (err) {
    console.error("[StreamingWebStorage] saveServerSnapshot error:", err);
  }
};

export const removeServerSnapshot = async (serverId: string): Promise<void> => {
  try {
    await store.removeItem(`${SNAPSHOT_PREFIX}${serverId}`);
  } catch {}
};

export const toPublicConfig = (record: PersistedServerRecord): StreamingServerConfig => ({
  id: record.id,
  name: record.name,
  type: record.type,
  url: record.url,
  username: record.username,
  hasPassword: Boolean(record.password),
  lastConnected: record.lastConnected,
});

export const toRuntimeConfig = (record: PersistedServerRecord): StreamingRuntimeConfig => ({
  id: record.id,
  name: record.name,
  type: record.type,
  url: record.url,
  username: record.username,
  password: record.password || "",
  hasPassword: Boolean(record.password),
  lastConnected: record.lastConnected,
});
