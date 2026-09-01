/**
 * QM 用户基础资料模块
 *
 * 通过登录 Cookie 中的 uin 及 Web 官方 CGI 接口获取用户信息；
 * key 失效时先尝试刷新 musickey 再重试一次
 */

import { getQQMusicCookies, getQQMusicUin } from "../core/request";
import { coreLog } from "@main/utils/logger";
import type { QMModule } from "../core/types";

interface ProfileCreator {
  nick?: string;
  headpic?: string;
  icon?: string;
  vip?: number;
  vip_level?: number;
  is_vip?: number;
  is_super_vip?: number;
}

interface ProfileHomepageResp {
  code?: number;
  subcode?: number;
  data?: {
    creator?: ProfileCreator;
  };
}

const WEB_UA =
  "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";

/** QQ 系 CSRF token 算法（与网页端 g_tk 一致） */
const hash33 = (str: string, seed = 0): number => {
  let h = seed;
  for (const ch of str) h = (((h << 5) + h + ch.codePointAt(0)!) & 0xffffffff) >>> 0;
  return h & 2147483647;
};

/** 请求一次 profile 接口；成功返回 creator，失败抛错 */
const fetchProfile = async (uin: string): Promise<ProfileCreator | null> => {
  const cookies = getQQMusicCookies();
  const musickey = cookies.qm_keyst || cookies.qqmusic_key || "";
  const cookieEntries = Object.entries(cookies).filter(([_, v]) => !!v);
  const cookieStr = cookieEntries.map(([k, v]) => `${k}=${v}`).join("; ");

  const url = `https://c.y.qq.com/rsc/fcgi-bin/fcg_get_profile_homepage.fcg?g_tk=${hash33(
    musickey,
    5381,
  )}&cid=205360838&userid=${encodeURIComponent(uin)}&loginUin=${encodeURIComponent(
    uin,
  )}&hostUin=0&reqfrom=1&format=json&inCharset=utf8&outCharset=utf-8&notice=0&platform=yqq.json&needNewCode=0`;

  const res = await fetch(url, {
    method: "GET",
    headers: {
      Referer: "https://y.qq.com/",
      Origin: "https://y.qq.com",
      "User-Agent": WEB_UA,
      ...(cookieStr ? { Cookie: cookieStr } : {}),
    },
    signal: AbortSignal.timeout(8000),
  });

  const json = (await res.json()) as ProfileHomepageResp;
  if (json.code !== 0) {
    throw new Error(`Web 接口返回异常: code=${json.code}, subcode=${json.subcode}`);
  }
  return json.data?.creator ?? null;
};

/** 获取用户信息 */
const fetchCgiProfile = async (): Promise<ProfileCreator | null> => {
  const cookies = getQQMusicCookies();
  const uin = getQQMusicUin();
  const musickey = cookies.qm_keyst || cookies.qqmusic_key || "";
  const cookieEntries = Object.entries(cookies).filter(([_, v]) => !!v);
  const cookieStr = cookieEntries.map(([k, v]) => `${k}=${v}`).join("; ");
  const g_tk = hash33(musickey, 5381);

  const body = {
    comm: {
      uin,
      format: "json",
      ct: 24,
      cv: 4747474,
      platform: "yqq.json",
      chid: "0",
      g_tk,
      g_tk_new_20200303: g_tk,
      inCharset: "utf-8",
      outCharset: "utf-8",
      notice: 0,
      needNewCode: 1,
    },
    req_0: {
      module: "music.UserInfo.userInfoServer",
      method: "GetLoginUserInfo",
      param: {},
    },
  };

  try {
    const res = await fetch("https://u.y.qq.com/cgi-bin/musicu.fcg", {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        Cookie: cookieStr,
        Referer: "https://y.qq.com/",
        Origin: "https://y.qq.com",
        "User-Agent": WEB_UA,
      },
      body: JSON.stringify(body),
      signal: AbortSignal.timeout(8000),
    });
    const json = (await res.json()) as {
      code?: number;
      req_0?: {
        code?: number;
        data?: {
          userInfo?: {
            uin?: string;
            nick?: string;
            nickname?: string;
            name?: string;
            headurl?: string;
            avatar?: string;
            headpic?: string;
            logo?: string;
            isVip?: boolean;
            vipLevel?: number;
          };
          profile?: {
            nick?: string;
            headpic?: string;
            info?: {
              nick?: string;
              nickname?: string;
              logo?: string;
              headurl?: string;
              avatarUrl?: string;
              is_vip?: number;
              vip?: number;
              vip_level?: number;
            };
          };
        };
      };
    };

    const reqData = json.req_0?.data;
    if (json.code === 0 && json.req_0?.code === 0 && reqData) {
      const user = reqData.userInfo;
      const profile = reqData.profile;
      const info = profile?.info;

      const nick =
        user?.nick || user?.nickname || user?.name || profile?.nick || info?.nick || info?.nickname;
      const headpic =
        user?.headurl ||
        user?.headpic ||
        user?.avatar ||
        user?.logo ||
        profile?.headpic ||
        info?.logo ||
        info?.headurl ||
        info?.avatarUrl;
      const isVip = user?.isVip ? 1 : info?.is_vip || (info?.vip && info.vip > 0) ? 1 : 0;
      const vipLevel = user?.vipLevel ?? info?.vip_level ?? 0;

      if (nick || headpic) {
        return {
          nick,
          headpic,
          is_vip: isVip,
          vip_level: vipLevel,
        };
      }
    }
  } catch (err) {
    coreLog.warn("[qm-user-detail] GetLoginUserInfo 接口请求失败:", err);
  }
  return null;
};

const userDetail: QMModule = async (_params) => {
  const uin = getQQMusicUin();
  const cookies = getQQMusicCookies();

  const hasKey = !!(
    cookies.qm_keyst ||
    cookies.qqmusic_key ||
    cookies.pskey ||
    cookies.p_skey ||
    cookies.skey
  );

  if (!uin || uin === "0" || !hasKey) {
    return {
      code: 301,
      loggedIn: false,
      message: "未登录 QM 账号",
    };
  }

  let creator: ProfileCreator | null = null;
  try {
    creator = (await fetchCgiProfile()) ?? (await fetchProfile(uin));
  } catch {
    try {
      creator = await fetchProfile(uin);
    } catch {
      // 忽略
    }
  }

  const defaultAvatar = `https://q.qlogo.cn/headimg_dl?dst_uin=${uin}&spec=100`;
  const isWx =
    cookies.tmeLoginType === "1" || (cookies.qm_keyst && cookies.qm_keyst.startsWith("W_X"));
  return {
    code: 200,
    loggedIn: true,
    profile: {
      userId: uin,
      nickname: creator?.nick || (isWx ? `微信用户_${uin.slice(-4)}` : `QQ用户_${uin.slice(-4)}`),
      avatarUrl: creator?.headpic || defaultAvatar,
      isVip: !!(creator?.is_vip || creator?.is_super_vip || (creator?.vip && creator.vip > 0)),
      vipLevel: creator?.vip_level ?? 0,
    },
  };
};

export default userDetail;
