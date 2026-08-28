/**
 * 外部 API 门禁中间件
 * - externalApi.enabled   总开关：关闭时 /api/* 与 /ws 全部 403
 * - externalApi.wsEnabled 子开关：关闭时 /ws 单独 403，REST 不受影响
 * - externalApi.bypassToken 可选 Token：前端通过 Authorization: Bearer <token> 传递，携带则跳过门禁检查
 */

import type { MiddlewareHandler, Context } from "hono";
import { store } from "@main/store";

const DEFAULT_ALLOWED_ORIGINS = [
  "http://localhost:5173",
  "http://127.0.0.1:5173",
];

/**
 * 生产环境通过 SPLAYER_CORS_ORIGINS 配置 Origin，多个值使用逗号分隔。
 * 例如：SPLAYER_CORS_ORIGINS=https://music.example.com,https://admin.example.com
 */
const getAllowedOrigins = (): string[] => {
  const configured = process.env.SPLAYER_CORS_ORIGINS
    ?.split(",")
    .map((origin) => origin.trim())
    .filter(Boolean);
  return [...DEFAULT_ALLOWED_ORIGINS, ...(configured ?? [])];
};

/**
 * 解析前端请求附带的 Bearer Token
 * 放在设置 `externalApi.bypassToken` 中（选填）
 * 前端调用时记得在请求头添加 `Authorization: Bearer <token>`
 */
export const getBypassToken = (): string => {
  return store.get("externalApi.bypassToken") || "";
};

/**
 * 校验请求是否携带有效的 bypass token
 * @returns true -> 免除门禁检查；false -> 继续走标准开关路径
 */
export const validateBypassToken = (header: string | undefined): boolean => {
  const token = getBypassToken();
  if (!token) return false;
  if (!header) return false;
  // 支持 "Bearer xxx" 或直接 "xxx"
  const presented = header.replace(/^Bearer\s+/i, "").trim();
  return presented === token;
};

/** CORS 响应头键值对 */
type CorsHeaders = Record<string, string>;

/** 构建 CORS 头部，遵循：先匹配精确 origin、再匹配包含 */
export const buildCorsHeaders = (origin: string | undefined): CorsHeaders => {
  const headers: CorsHeaders = {};
  if (!origin) return headers;

  const origins = getAllowedOrigins();
  const matched = origins.find((o) => o === origin);

  if (matched) {
    headers["Access-Control-Allow-Origin"] = matched;
    headers["Access-Control-Allow-Methods"] = "GET, POST, OPTIONS";
    headers["Access-Control-Allow-Headers"] = "Content-Type, Authorization, x-bypass-token";
    headers["Access-Control-Max-Age"] = "86400";
  }
  return headers;
};

/** 为 CORS 头部添加凭证（生产用） */
const addCorsForPreflight = (c: Context, origin: string | undefined): void => {
  const headers = buildCorsHeaders(origin);
  for (const [k, v] of Object.entries(headers)) {
    c.header(k, v);
  }
  // 生产环境若需凭证可开启：
  // c.header("Access-Control-Allow-Credentials", "true");
};

/** 总开关 + 可选 Token 免校验 */
export const externalControlGate: MiddlewareHandler = async (c: Context, next) => {
  const origin = c.req.header("origin");

  // 总开关优先于 Token，避免关闭 API 后仍可通过 Token 访问。
  if (!store.get("externalApi.enabled")) {
    addCorsForPreflight(c, origin);
    return c.json({ error: "external API disabled" }, 403);
  }

  // 预检请求必须在业务处理前返回，并携带完整 CORS 头。
  if (c.req.method === "OPTIONS") {
    addCorsForPreflight(c, origin);
    return c.text("OK", 200);
  }

  // 配置 Token 后要求请求携带正确的 Token；未配置时保持原有开关行为。
  const configuredToken = getBypassToken();
  if (configuredToken && !validateBypassToken(c.req.header("authorization"))) {
    addCorsForPreflight(c, origin);
    return c.json({ error: "unauthorized" }, 401);
  }

  // Token 只负责鉴权，不跳过 CORS 响应头处理。
  await next();

  // 无论是否携带 Token，都为响应补充 CORS 头。
  addCorsForPreflight(c, origin);
  return;
};

/** WS 子开关 + 可选 Token 免校验 */
export const wsGate: MiddlewareHandler = async (c: Context, next) => {
  if (!store.get("externalApi.enabled")) {
    return c.json({ error: "external API disabled" }, 403);
  }

  if (!store.get("externalApi.wsEnabled")) {
    return c.json({ error: "WebSocket disabled" }, 403);
  }

  await next();
  return;
};