import { md5 } from "../src/services/streaming/web/md5";

const serverArg = process.argv[2] || "https://music.mefun.org";
const username = process.argv[3] || "root";
const password = process.argv[4] || "audiosys";
const headlessUrl = process.argv[5] || "http://192.168.31.59:14558";

const baseUrl = serverArg.startsWith("http://") || serverArg.startsWith("https://")
  ? serverArg.replace(/\/+$/, "")
  : `http://${serverArg}:4533`;

console.log("\x1b[1;36m================================================================");
console.log("   SPlayer-Next 流媒体源完整链路在线诊断探测工具");
console.log(`   Navidrome 服务器: ${baseUrl}`);
console.log(`   测试账号: ${username}`);
console.log(`   SPlayer Headless 端口: ${headlessUrl}`);
console.log("================================================================\x1b[0m\n");

const generateSubsonicAuth = (pwd: string, isJson = true) => {
  const salt = Math.random().toString(36).substring(2, 10);
  const token = md5(pwd + salt);
  const params = new URLSearchParams({
    u: username,
    t: token,
    s: salt,
    v: "1.16.1",
    c: "SPlayer-Next",
  });
  if (isJson) params.set("f", "json");
  return params;
};

async function main() {

  console.log(`[步骤 1] 测试 Navidrome Ping 连通性 -> ${baseUrl}/rest/ping ...`);
  let pingOk = false;
  try {
    const authParams = generateSubsonicAuth(password, true);
    const res = await fetch(`${baseUrl}/rest/ping?${authParams.toString()}`, {
      signal: AbortSignal.timeout(5000),
    });
    const json = (await res.json()) as any;
    const status = json?.["subsonic-response"]?.status;
    if (status === "ok") {
      console.log(`  \x1b[32m✓ Ping 成功！服务端版本: ${json["subsonic-response"].serverVersion || json["subsonic-response"].version}\x1b[0m`);
      pingOk = true;
    } else {
      console.log(`  \x1b[31m✗ Ping 响应异常: ${JSON.stringify(json)}\x1b[0m`);
    }
  } catch (err) {
    console.log(`  \x1b[31m✗ 连接 Navidrome 失败: ${(err as Error).message}\x1b[0m`);
    console.log(`  \x1b[33m提示: 请确认 Navidrome 地址是否正确: ${baseUrl}\x1b[0m\n`);
  }

  if (!pingOk) {
    return;
  }

  console.log(`\n[步骤 2] 获取测试音轨 (getRandomSongs / search3) ...`);
  let testTrackId = "";
  let testTrackTitle = "";
  try {
    const authParams = generateSubsonicAuth(password, true);
    authParams.set("size", "3");
    const res = await fetch(`${baseUrl}/rest/getRandomSongs?${authParams.toString()}`);
    const json = (await res.json()) as any;
    const songs = json?.["subsonic-response"]?.randomSongs?.song || [];
    if (songs.length > 0) {
      testTrackId = songs[0].id;
      testTrackTitle = songs[0].title;
      console.log(`  \x1b[32m✓ 成功获取音轨: [${testTrackId}] "${testTrackTitle}" (歌手: ${songs[0].artist}, 格式: ${songs[0].suffix})\x1b[0m`);
    } else {
      console.log(`  \x1b[33m! getRandomSongs 返回空列表，尝试 search3("a") ...\x1b[0m`);
      const sParams = generateSubsonicAuth(password, true);
      sParams.set("query", "a");
      sParams.set("songCount", "3");
      const sRes = await fetch(`${baseUrl}/rest/search3?${sParams.toString()}`);
      const sJson = (await sRes.json()) as any;
      const sSongs = sJson?.["subsonic-response"]?.searchResult3?.song || [];
      if (sSongs.length > 0) {
        testTrackId = sSongs[0].id;
        testTrackTitle = sSongs[0].title;
        console.log(`  \x1b[32m✓ 成功搜索到音轨: [${testTrackId}] "${testTrackTitle}" (格式: ${sSongs[0].suffix})\x1b[0m`);
      }
    }
  } catch (err) {
    console.log(`  \x1b[31m✗ 获取音轨失败: ${(err as Error).message}\x1b[0m`);
  }

  if (!testTrackId) {
    console.log(`  \x1b[31m✗ 未能获取到任何可用测试音轨，诊断终止\x1b[0m`);
    return;
  }

  console.log(`\n[步骤 3] 测试直推音频流地址 (stream) ...`);
  const streamAuth = generateSubsonicAuth(password, false); // 不带 f=json
  streamAuth.set("id", testTrackId);
  const streamUrl = `${baseUrl}/rest/stream?${streamAuth.toString()}`;
  console.log(`  流地址: ${streamUrl}`);
  try {
    const streamRes = await fetch(streamUrl, {
      headers: { Range: "bytes=0-1024" },
      signal: AbortSignal.timeout(10000),
    });
    console.log(`  HTTP 状态: ${streamRes.status} ${streamRes.statusText}`);
    console.log(`  Content-Type: ${streamRes.headers.get("content-type")}`);
    console.log(`  Content-Length: ${streamRes.headers.get("content-length")}`);
    console.log(`  Content-Range: ${streamRes.headers.get("content-range")}`);

    const buf = Buffer.from(await streamRes.arrayBuffer());
    console.log(`  读取头部字节数: ${buf.length} bytes`);
    const hex = buf.subarray(0, 16).toString("hex");
    const text = buf.subarray(0, 16).toString("ascii").replace(/[^\x20-\x7E]/g, ".");
    console.log(`  Magic Bytes (Hex): ${hex} (${text})`);
    if (text.startsWith("fLaC") || text.startsWith("ID3") || hex.startsWith("fffb") || hex.startsWith("fffa") || text.includes("ftyp")) {
      console.log(`  \x1b[32m✓ 确认是合法的音频二进制文件头！\x1b[0m`);
    } else if (text.startsWith("{") || text.startsWith("<")) {
      console.log(`  \x1b[31m✗ 警告：服务端返回了文本/JSON/XML数据，而不是音频二进制流！内容: ${buf.toString("utf-8").slice(0, 100)}\x1b[0m`);
    } else {
      console.log(`  \x1b[32m✓ 二进制音频流返回成功\x1b[0m`);
    }
  } catch (err) {
    console.log(`  \x1b[31m✗ 下载音频流失败: ${(err as Error).message}\x1b[0m`);
  }

  console.log(`\n[步骤 4] 测试向 SPlayer-Next-Headless 发送加载播放请求 (POST /api/v1/player/load) ...`);
  let loadDone = false;
  for (const hUrl of [headlessUrl, "http://127.0.0.1:14558"]) {
    try {
      const loadPayload = {
        source: streamUrl,
        auto_play: false,
        meta: {
          id: `srv:${testTrackId}`,
          title: testTrackTitle,
          source: "streaming",
        },
      };
      console.log(`  正在请求: ${hUrl}/api/v1/player/load ...`);
      const loadRes = await fetch(`${hUrl}/api/v1/player/load`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(loadPayload),
        signal: AbortSignal.timeout(10000),
      });
      const loadJson = await loadRes.json();
      console.log(`  Headless 响应结果:`, JSON.stringify(loadJson, null, 2));
      if (loadRes.ok && loadJson.success) {
        console.log(`  \x1b[32m🎉 播放器加载成功！采样率: ${loadJson.data?.sample_rate || loadJson.data?.detail?.quality?.sampleRate}Hz, 声道: ${loadJson.data?.channels || loadJson.data?.detail?.quality?.channels}\x1b[0m`);
        loadDone = true;
        break;
      } else {
        console.log(`  \x1b[31m✗ 播放器加载返回错误: ${JSON.stringify(loadJson.error || loadJson)}\x1b[0m`);
      }
    } catch (err) {
      console.log(`  \x1b[33m! 请求 ${hUrl} 失败: ${(err as Error).message}\x1b[0m`);
    }
  }

  console.log("\n\x1b[1;36m================================================================");
  console.log("   诊断探测完成");
  console.log("================================================================\x1b[0m");
}

main().catch(console.error);
