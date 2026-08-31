/**
 * 流媒体平台登录态 Store（Qobuz / TIDAL / QQ 音乐 / 酷狗音乐）
 *
 * 与 user store（netease 专用）解耦：各平台凭证在后端 safeStorage 加密持久化，
 * 前端 store 只管理内存态 + 调用 API 查询/登录/登出。
 */

import { defineStore } from "pinia";
import * as qobuzApi from "@/apis/qobuz";
import * as tidalApi from "@/apis/tidal";
import { fetchQQMusicLoginStatus, logoutQQMusic as doLogoutQQMusic } from "@/apis/login/qqmusic";
import { fetchKugouLoginStatus, logoutKugou as doLogoutKugou } from "@/apis/login/kugou";

/** Qobuz 登录态 */
export interface QobuzAuthInfo {
  loggedIn: boolean;
  username?: string;
  userId?: string;
}

/** TIDAL 登录态 */
export interface TidalAuthInfo {
  loggedIn: boolean;
  username?: string;
}

/** QQ 音乐登录态 */
export interface QQMusicAuthInfo {
  loggedIn: boolean;
  uin?: string;
}

/** 酷狗音乐登录态 */
export interface KugouAuthInfo {
  loggedIn: boolean;
}

interface StreamingAuthState {
  qobuz: QobuzAuthInfo;
  tidal: TidalAuthInfo;
  qqmusic: QQMusicAuthInfo;
  kugou: KugouAuthInfo;
  /** 是否已完成首次状态查询（避免登录页闪烁） */
  initialized: boolean;
}

export const useStreamingAuthStore = defineStore("streamingAuth", {
  state: (): StreamingAuthState => ({
    qobuz: { loggedIn: false },
    tidal: { loggedIn: false },
    qqmusic: { loggedIn: false },
    kugou: { loggedIn: false },
    initialized: false,
  }),

  getters: {
    qobuzLoggedIn: (state) => state.qobuz.loggedIn,
    tidalLoggedIn: (state) => state.tidal.loggedIn,
    qqmusicLoggedIn: (state) => state.qqmusic.loggedIn,
    kugouLoggedIn: (state) => state.kugou.loggedIn,
  },

  actions: {
    /**
     * 查询 Qobuz 登录状态（应用启动时调用一次）
     * 后端从 safeStorage 加载持久化凭证，返回 loggedIn + username
     */
    async fetchQobuzStatus(): Promise<void> {
      try {
        const res = await qobuzApi.authStatus();
        this.qobuz = {
          loggedIn: !!res.loggedIn,
          username: res.username,
          userId: res.userId,
        };
      } catch {
        this.qobuz = { loggedIn: false };
      } finally {
        this.initialized = true;
      }
    },

    /**
     * 登录 Qobuz（user_id + user_auth_token）
     * 后端验证成功后 safeStorage 加密持久化，重启保持登录
     */
    async loginQobuz(userId: string, userAuthToken: string): Promise<void> {
      const res = await qobuzApi.login(userId, userAuthToken);
      if (res?.status !== "success" && !(res as any)?.user_id && !(res as any)?.user) {
        throw new Error("Qobuz 登录失败");
      }
      this.qobuz = {
        loggedIn: true,
        username: (res as any)?.display_name ?? res?.user?.login ?? userId,
        userId: res?.user?.id ? String(res.user.id) : (res as any)?.user_id ?? userId,
      };
    },

    /** 登出 Qobuz（清除后端持久化凭证） */
    async logoutQobuz(): Promise<void> {
      await qobuzApi.logout();
      this.qobuz = { loggedIn: false };
    },

    /** 查询 TIDAL 登录状态 */
    async fetchTidalStatus(): Promise<void> {
      try {
        const res = await tidalApi.authStatus();
        this.tidal = {
          loggedIn: !!res.loggedIn,
          username: res.username,
        };
      } catch {
        this.tidal = { loggedIn: false };
      } finally {
        this.initialized = true;
      }
    },

    /**
     * 第一步：发起 TIDAL 设备码授权
     * @param clientId 可选（用户自定义 Client ID）
     * @param clientSecret 可选（用户自定义 Client Secret）
     * @returns 设备码授权信息（含 userCode + verificationUriComplete）
     */
    async loginTidalDeviceCode(
      clientId?: string,
      clientSecret?: string,
    ): Promise<tidalApi.TidalDeviceAuthorizationResponse> {
      return await tidalApi.authDeviceAuthorization(clientId, clientSecret);
    },

    /**
     * 第二步：轮询 TIDAL 设备码 token
     * @param deviceCode auth_device_authorization 返回的 deviceCode
     * @param clientId 可选
     * @param clientSecret 可选
     * @returns 轮询状态：success/pending/expired/denied/timeout/error
     */
    async pollTidalToken(
      deviceCode: string,
      clientId?: string,
      clientSecret?: string,
    ): Promise<tidalApi.TidalTokenPollResponse> {
      return await tidalApi.authTokenPoll(deviceCode, clientId, clientSecret);
    },

    /**
     * 完成 TIDAL 登录（成功轮询后调用，更新 store 状态）
     * @param username TIDAL 用户名
     */
    completeTidalLogin(username: string): void {
      this.tidal = {
        loggedIn: true,
        username,
      };
    },

    /**
     * 发起 TIDAL 授权码登录（官方客户端，支持标准 LOSSLESS）
     * @param redirectBase 回调基础地址（window.location.origin）
     * @param clientId 可选
     * @returns 授权 URL 等信息
     */
    async loginTidalAuthorize(
      redirectBase?: string,
      clientId?: string,
    ): Promise<tidalApi.TidalAuthorizeResponse> {
      return await tidalApi.authAuthorize(redirectBase, clientId);
    },

    /**
     * 完成 TIDAL 授权码登录（官方客户端）
     * 用户把浏览器回调 URL 粘贴回来，前端解析出 code+state 后调用后端交换 token
     * @param code 回调 URL 中的 code
     * @param state 回调 URL 中的 state（可选）
     */
    async exchangeTidalCode(code: string, state?: string): Promise<void> {
      const res = await tidalApi.authExchange(code, state);
      if (res.status !== "success") {
        throw new Error((res as { message?: string }).message || "TIDAL 登录失败");
      }
      this.tidal = {
        loggedIn: true,
        username: res.username,
      };
    },

    /** 登出 TIDAL（清除后端持久化凭证） */
    async logoutTidal(): Promise<void> {
      await tidalApi.logout();
      this.tidal = { loggedIn: false };
    },

    // ============ QQ 音乐 ============

    /** 查询 QQ 音乐登录状态 */
    async fetchQQMusicStatus(): Promise<void> {
      try {
        const profile = await fetchQQMusicLoginStatus();
        this.qqmusic = {
          loggedIn: !!profile,
          uin: profile?.userId,
        };
      } catch {
        this.qqmusic = { loggedIn: false };
      } finally {
        this.initialized = true;
      }
    },

    /** 登出 QQ 音乐 */
    async logoutQQMusic(): Promise<void> {
      await doLogoutQQMusic();
      this.qqmusic = { loggedIn: false };
    },

    // ============ 酷狗音乐 ============

    /** 查询酷狗音乐登录状态 */
    async fetchKugouStatus(): Promise<void> {
      try {
        const profile = await fetchKugouLoginStatus();
        this.kugou = {
          loggedIn: !!profile,
        };
      } catch {
        this.kugou = { loggedIn: false };
      } finally {
        this.initialized = true;
      }
    },

    /** 登出酷狗音乐 */
    async logoutKugou(): Promise<void> {
      await doLogoutKugou();
      this.kugou = { loggedIn: false };
    },
  },
});
