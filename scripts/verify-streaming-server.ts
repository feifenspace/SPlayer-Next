import { md5 } from "../src/services/streaming/web/md5";

interface ServerOption {
  type: "subsonic" | "jellyfin";
  url: string;
  username: string;
  password: string;
}

const host = process.argv[2] || "192.168.31.59";
const username = process.argv[3] || "root";
const password = process.argv[4] || "audiosys";
const query = process.argv[5] || "a"; // 默认搜索关键词

console.log("\x1b[1;36m================================================================");
console.log(`   SPlayer-Next 流媒体实时在线联调探测工具`);
console.log(`   目标服务器: ${host}`);
console.log(`   测试账号: ${username}`);
console.log(`   测试搜索词: "${query}"`);
console.log("================================================================\x1b[0m\n");

const generateSubsonicAuth = (pwd: string) => {
  const salt = Math.random().toString(36).substring(2, 10);
  const token = md5(pwd + salt);
  return { salt, token };
};

const testSubsonic = async (baseUrl: string) => {
  console.log(`\x1b[1;33m[探测] 正在尝试 Subsonic / Navidrome 协议 -> ${baseUrl} ...\x1b[0m`);
  const { salt, token } = generateSubsonicAuth(password);
  const params = new URLSearchParams({
    u: username,
    t: token,
    s: salt,
    v: "1.16.1",
    c: "SPlayer-Next",
    f: "json",
  });

  try {
    // 1. Ping 测试
    const pingUrl = `${baseUrl.replace(/\/+$/, "")}/rest/ping?${params.toString()}`;
    const pingRes = await fetch(pingUrl, { signal: AbortSignal.timeout(5000) });
    if (!pingRes.ok) {
      console.log(`  \x1b[31m✗ HTTP 状态码: ${pingRes.status}\x1b[0m`);
      return false;
    }
    const pingData = (await pingRes.json()) as any;
    const subResp = pingData["subsonic-response"];
    if (subResp?.status !== "ok") {
      console.log(`  \x1b[31m✗ Subsonic 鉴权失败 / 状态异常: ${JSON.stringify(subResp?.error || subResp)}\x1b[0m`);
      return false;
    }
    console.log(`  \x1b[32m✓ 连通性测试 (Ping) 成功！服务端版本: ${subResp.serverVersion || subResp.version}\x1b[0m`);

    // 2. 搜索接口 (search3) 测试
    const searchParams = new URLSearchParams(params);
    searchParams.set("query", query);
    searchParams.set("songCount", "10");
    searchParams.set("albumCount", "5");
    searchParams.set("artistCount", "5");
    const searchUrl = `${baseUrl.replace(/\/+$/, "")}/rest/search3?${searchParams.toString()}`;
    const searchRes = await fetch(searchUrl, { signal: AbortSignal.timeout(8000) });
    const searchData = (await searchRes.json()) as any;
    const searchResult = searchData["subsonic-response"]?.searchResult3;

    const songCount = searchResult?.song?.length || 0;
    const albumCount = searchResult?.album?.length || 0;
    const artistCount = searchResult?.artist?.length || 0;

    console.log(`  \x1b[32m✓ 搜索接口 (search3) 请求成功！\x1b[0m`);
    console.log(`    - 检索到单曲: ${songCount} 首`);
    if (songCount > 0) {
      searchResult.song.slice(0, 3).forEach((s: any, idx: number) => {
        console.log(`      [${idx + 1}] 《${s.title}》 - ${s.artist} (专辑: ${s.album || "未知"}) [ID: ${s.id}]`);
      });
    }
    console.log(`    - 检索到专辑: ${albumCount} 张`);
    console.log(`    - 检索到歌手: ${artistCount} 位`);

    // 3. 歌曲取流播放与封面 URL 测试
    if (songCount > 0) {
      const firstSong = searchResult.song[0];
      const streamUrl = `${baseUrl.replace(/\/+$/, "")}/rest/stream?id=${firstSong.id}&format=raw&${params.toString()}`;
      const coverUrl = firstSong.coverArt
        ? `${baseUrl.replace(/\/+$/, "")}/rest/getCoverArt?id=${firstSong.coverArt}&size=300&${params.toString()}`
        : "无封面";
      console.log(`  \x1b[32m✓ 播放流 URL 验证:\x1b[0m ${streamUrl}`);
      console.log(`  \x1b[32m✓ 封面图 URL 验证:\x1b[0m ${coverUrl}`);
    }

    return true;
  } catch (err) {
    console.log(`  \x1b[31m✗ 连接失败: ${err instanceof Error ? err.message : String(err)}\x1b[0m`);
    return false;
  }
};

const testJellyfin = async (baseUrl: string) => {
  console.log(`\n\x1b[1;33m[探测] 正在尝试 Jellyfin / Emby 协议 -> ${baseUrl} ...\x1b[0m`);
  try {
    const authHeader = `MediaBrowser Client="SPlayer-Next", Device="SPlayer Web", DeviceId="splayer-verify", Version="1.0.0"`;
    const authRes = await fetch(`${baseUrl.replace(/\/+$/, "")}/Users/AuthenticateByName`, {
      method: "POST",
      headers: {
        "Content-Type": "application/json",
        Authorization: authHeader,
      },
      body: JSON.stringify({ Username: username, Pw: password }),
      signal: AbortSignal.timeout(5000),
    });

    if (!authRes.ok) {
      console.log(`  \x1b[31m✗ 鉴权失败: HTTP ${authRes.status}\x1b[0m`);
      return false;
    }

    const authData = (await authRes.json()) as any;
    const token = authData.AccessToken;
    const userId = authData.User?.Id;
    console.log(`  \x1b[32m✓ Jellyfin 用户登录成功！UserId: ${userId}\x1b[0m`);

    // 搜索
    const searchUrl = `${baseUrl.replace(/\/+$/, "")}/Users/${userId}/Items?searchTerm=${encodeURIComponent(query)}&IncludeItemTypes=Audio,MusicAlbum,MusicArtist&Recursive=true&Limit=10`;
    const searchRes = await fetch(searchUrl, {
      headers: {
        Authorization: `${authHeader}, Token="${token}"`,
      },
      signal: AbortSignal.timeout(8000),
    });
    const searchData = (await searchRes.json()) as any;
    const items = searchData.Items || [];
    console.log(`  \x1b[32m✓ Jellyfin 搜索接口请求成功！检索到 ${items.length} 个条目\x1b[0m`);
    items.slice(0, 3).forEach((item: any, idx: number) => {
      console.log(`    [${idx + 1}] (${item.Type}) ${item.Name} [ID: ${item.Id}]`);
    });
    return true;
  } catch (err) {
    console.log(`  \x1b[31m✗ 连接失败: ${err instanceof Error ? err.message : String(err)}\x1b[0m`);
    return false;
  }
};

const run = async () => {
  const candidateUrls = [
    `http://${host}:4533`,  // Navidrome 默认端口
    `http://${host}:4040`,  // Subsonic 默认端口
    `http://${host}:8096`,  // Jellyfin / Emby 默认端口
    `http://${host}`,       // 80 端口
  ];

  let success = false;

  for (const url of candidateUrls) {
    const isSubsonic = await testSubsonic(url);
    if (isSubsonic) {
      success = true;
      console.log(`\n\x1b[1;32m🎉 成功定位到 Navidrome / Subsonic 服务：${url}\x1b[0m`);
      break;
    }
  }

  if (!success) {
    for (const url of candidateUrls) {
      const isJellyfin = await testJellyfin(url);
      if (isJellyfin) {
        success = true;
        console.log(`\n\x1b[1;32m🎉 成功定位到 Jellyfin / Emby 服务：${url}\x1b[0m`);
        break;
      }
    }
  }

  console.log("\n\x1b[1;36m================================================================");
  if (success) {
    console.log("   联调结果: 流媒体服务与搜索接口完全正常！");
  } else {
    console.log("   联调提示: 未能在候选端口上自动连通，请检查端口是否开放或指定端口。");
  }
  console.log("================================================================\x1b[0m\n");
};

run();
