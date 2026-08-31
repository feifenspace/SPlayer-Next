import type { PlatformProfile } from "@shared/types/platform";
import * as qobuzApi from "@/apis/qobuz";

/** 查询 Qobuz 登录状态 */
export const fetchQobuzLoginStatus = async (): Promise<PlatformProfile | null> => {
  try {
    const res = await qobuzApi.authStatus();
    if (res?.loggedIn) {
      return {
        userId: res.userId || "Qobuz",
        nickname: res.username || "Qobuz User",
        avatarUrl: "",
        isVip: (res as any).hasSubscription ?? true,
      };
    }
    return null;
  } catch (err) {
    console.warn("[qobuz] fetch login status failed:", err);
    return null;
  }
};

/** 登出 Qobuz */
export const logoutQobuz = async (): Promise<void> => {
  try {
    await qobuzApi.logout();
  } catch (err) {
    console.warn("[qobuz] logout failed:", err);
  }
};
