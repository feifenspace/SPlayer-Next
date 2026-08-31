import type { PlatformProfile } from "@shared/types/platform";
import * as tidalApi from "@/apis/tidal";

/** 查询 TIDAL 登录状态 */
export const fetchTidalLoginStatus = async (): Promise<PlatformProfile | null> => {
  try {
    const res = await tidalApi.authStatus();
    if (res?.loggedIn) {
      return {
        userId: res.userId || "TIDAL",
        nickname: res.username || "TIDAL User",
        avatarUrl: "",
        isVip: true,
      };
    }
    return null;
  } catch (err) {
    console.warn("[tidal] fetch login status failed:", err);
    return null;
  }
};

/** 登出 TIDAL */
export const logoutTidal = async (): Promise<void> => {
  try {
    await tidalApi.logout();
  } catch (err) {
    console.warn("[tidal] logout failed:", err);
  }
};
