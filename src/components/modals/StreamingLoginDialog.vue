<script setup lang="ts">
/**
 * 流媒体平台登录对话框（Qobuz / TIDAL）
 *
 * Qobuz：user_id + user_auth_token 表单登录
 *   - 用户需从 play.qobuz.com F12 → Network → api.json → Query String Parameters 获取
 *   - 后端 safeStorage 加密持久化，重启保持登录
 *
 * TIDAL：设备码授权（OAuth2 Device Code Flow）
 *   - 第一步：调用 auth_device_authorization 获取 userCode + verificationUriComplete
 *   - 用户在浏览器打开 link.tidal.com 输入 userCode 完成授权
 *   - 第二步：调用 auth_token_poll 轮询，直到 success/expired/denied
 *   - 也支持用户自定义 Client ID / Client Secret（推荐）
 */

import { useStreamingAuthStore } from "@/stores/streamingAuth";
import { toast } from "@/composables/useToast";
import type { TidalDeviceAuthorizationResponse } from "@/apis/tidal";
import STabs from "@/components/ui/STabs.vue";
import SAlert from "@/components/ui/SAlert.vue";
import IconLucideChevronDown from "~icons/lucide/chevron-down";

const props = defineProps<{
  open: boolean;
  tab?: "qobuz" | "tidal";
}>();
const emit = defineEmits<{
  "update:open": [value: boolean];
  "update:tab": [value: "qobuz" | "tidal"];
}>();

const authStore = useStreamingAuthStore();

type Tab = "qobuz" | "tidal";
const activeTab = ref<Tab>(props.tab ?? "qobuz");

// Tab 列表（驱动 STabs）
const tabs: { key: Tab; label: string }[] = [
  { key: "qobuz", label: "Qobuz" },
  { key: "tidal", label: "TIDAL" },
];

// 当外部设置 tab 变化时同步
watch(
  () => props.tab,
  (newTab) => {
    if (newTab) activeTab.value = newTab;
  },
);
watch(activeTab, (newTab) => emit("update:tab", newTab));

// Qobuz 表单
const qobuzUserId = ref("");
const qobuzAuthToken = ref("");
const qobuzLoading = ref(false);

// TIDAL 设备码授权状态
const tidalClientId = ref("");
const tidalClientSecret = ref("");
const tidalAdvancedOpen = ref(false);
const tidalDeviceInfo = ref<TidalDeviceAuthorizationResponse | null>(null);
const tidalPollStatus = ref<"idle" | "polling" | "success" | "expired" | "denied" | "error">("idle");
const tidalPollMessage = ref<string>("");
const tidalRequestLoading = ref(false);
const tidalPollTimer = ref<ReturnType<typeof setTimeout> | null>(null);

/** TIDAL 轮询状态对应的语义化提示样式 */
const tidalAlertType = computed<"info" | "success" | "error">(() => {
  switch (tidalPollStatus.value) {
    case "success":
      return "success";
    case "expired":
    case "denied":
    case "error":
      return "error";
    default:
      return "info";
  }
});

/** 清理轮询定时器 */
const clearTidalPollTimer = (): void => {
  if (tidalPollTimer.value) {
    clearTimeout(tidalPollTimer.value);
    tidalPollTimer.value = null;
  }
};

/** 打开时刷新登录状态 + 切到未登录的 tab + 重置 TIDAL 状态 */
watch(
  () => props.open,
  async (open) => {
    if (!open) {
      clearTidalPollTimer();
      return;
    }
    await Promise.all([
      authStore.fetchQobuzStatus(),
      authStore.fetchTidalStatus(),
    ]);
    qobuzUserId.value = "";
    qobuzAuthToken.value = "";
    // 保留最近一次的 deviceInfo 但重置轮询状态
    tidalPollStatus.value = "idle";
    tidalPollMessage.value = "";
  },
  { immediate: true },
);

onBeforeUnmount(() => {
  clearTidalPollTimer();
});

/** Qobuz 登录 */
const onQobuzLogin = async (): Promise<void> => {
  const userId = qobuzUserId.value.trim();
  const token = qobuzAuthToken.value.trim();
  if (!userId || !token) {
    toast.error("请填写 user_id 和 user_auth_token");
    return;
  }
  qobuzLoading.value = true;
  try {
    await authStore.loginQobuz(userId, token);
    toast.success("Qobuz 登录成功");
    emit("update:open", false);
  } catch (err) {
    toast.error(err instanceof Error ? err.message : "Qobuz 登录失败");
  } finally {
    qobuzLoading.value = false;
  }
};

/** Qobuz 登出 */
const onQobuzLogout = async (): Promise<void> => {
  try {
    await authStore.logoutQobuz();
    toast.success("已退出 Qobuz 登录");
  } catch (err) {
    toast.error(err instanceof Error ? err.message : "登出失败");
  }
};


// ============ TIDAL 授权码登录（官方客户端，无损） ============
// 官方移动 client 的回调地址 com.player.tidal/auth 在桌面不可达，
// 因此采用「手动粘贴回填」：前端打开授权页 → 用户登录 → 把浏览器地址栏 URL 粘回 → 解析 code 调后端交换 token。
const tidalPkceStatus = ref<"idle" | "waiting" | "success" | "error">("idle");
const tidalPkceMessage = ref<string>("");
const tidalCallbackUrl = ref<string>("");
const tidalExchanging = ref<boolean>(false);

/** PKCE 阶段提示的语义化样式 */
const tidalPkceType = computed<"info" | "success" | "error">(() => {
  if (tidalPkceStatus.value === "success") return "success";
  if (tidalPkceStatus.value === "error") return "error";
  return "info";
});

/** 用官方移动客户端发起授权码登录（PKCE），支持标准 LOSSLESS */
const onTidalAuthorize = async (): Promise<void> => {
  tidalPkceStatus.value = "waiting";
  tidalPkceMessage.value = "正在打开 TIDAL 授权页…";
  tidalCallbackUrl.value = "";
  try {
    const info = await authStore.loginTidalAuthorize();
    window.open(info.url, "_blank", "noopener");
    tidalPkceMessage.value =
      "请在打开的 TIDAL 页面完成授权；授权后浏览器会跳到 com.player.tidal/auth 报错页（正常现象），请复制地址栏完整 URL 粘贴到下方。";
  } catch (err) {
    tidalPkceStatus.value = "error";
    tidalPkceMessage.value = err instanceof Error ? err.message : "发起授权失败";
    toast.error(tidalPkceMessage.value);
  }
};

/** 从回调 URL / 自定义 scheme 中解析出 code 与 state */
const parseTidalCallback = (raw: string): { code?: string; state?: string } => {
  const text = raw.trim();
  if (!text) return {};
  let url: URL | null = null;
  try {
    url = new URL(text);
  } catch {
    try {
      url = new URL(text.replace("com.player.tidal://", "https://com.player.tidal/"));
    } catch {
      return {};
    }
  }
  return {
    code: url.searchParams.get("code") ?? undefined,
    state: url.searchParams.get("state") ?? undefined,
  };
};

/** 用户粘贴回调 URL 后，解析 code 并调用后端完成 token 交换 */
const onTidalCompleteLogin = async (): Promise<void> => {
  const { code, state } = parseTidalCallback(tidalCallbackUrl.value);
  if (!code) {
    tidalPkceStatus.value = "error";
    tidalPkceMessage.value = "未能从 URL 中解析出 code，请确认粘贴的是完整的回调地址。";
    return;
  }
  tidalExchanging.value = true;
  try {
    await authStore.exchangeTidalCode(code, state);
    tidalPkceStatus.value = "success";
    tidalPkceMessage.value = "登录成功，已获得无损 FLAC 权限";
    toast.success("TIDAL 登录成功（官方客户端）");
    setTimeout(() => emit("update:open", false), 800);
  } catch (err) {
    tidalPkceStatus.value = "error";
    tidalPkceMessage.value = err instanceof Error ? err.message : "兑换失败";
    toast.error(tidalPkceMessage.value);
  } finally {
    tidalExchanging.value = false;
  }
};

// ============ TIDAL 设备码授权 ============

/** 重置 TIDAL 状态 */
const cleanTidalState = (): void => {
  clearTidalPollTimer();
  tidalDeviceInfo.value = null;
  tidalPollStatus.value = "idle";
  tidalPollMessage.value = "";
};

/** 第一步：发起设备码授权 */
const onTidalRequestDeviceCode = async (): Promise<void> => {
  cleanTidalState();
  tidalRequestLoading.value = true;
  try {
    const info = await authStore.loginTidalDeviceCode(
      tidalClientId.value.trim() || undefined,
      tidalClientSecret.value.trim() || undefined,
    );
    tidalDeviceInfo.value = info;
    tidalPollStatus.value = "polling";
    tidalPollMessage.value = "请在浏览器中完成授权…";
    scheduleNextPoll();
  } catch (err) {
    tidalPollStatus.value = "error";
    tidalPollMessage.value = err instanceof Error ? err.message : "获取设备码失败";
    toast.error(tidalPollMessage.value);
  } finally {
    tidalRequestLoading.value = false;
  }
};

/** 调度下次轮询 */
const scheduleNextPoll = (): void => {
  const info = tidalDeviceInfo.value;
  if (!info) return;
  const intervalSec = Math.max(2, info.interval || 2);
  tidalPollTimer.value = setTimeout(() => {
    onTidalPoll();
  }, intervalSec * 1000);
};

/** 第二步：轮询 token */
const onTidalPoll = async (): Promise<void> => {
  const info = tidalDeviceInfo.value;
  if (!info) return;
  try {
    const res = await authStore.pollTidalToken(
      info.deviceCode,
      tidalClientId.value.trim() || undefined,
      tidalClientSecret.value.trim() || undefined,
    );
    if (res.status === "success") {
      tidalPollStatus.value = "success";
      tidalPollMessage.value = `登录成功：${res.username ?? "已登录"}`;
      authStore.completeTidalLogin(res.username ?? "tidal_user");
      toast.success("TIDAL 登录成功");
      clearTidalPollTimer();
      setTimeout(() => emit("update:open", false), 800);
      return;
    }
    if (res.status === "expired") {
      tidalPollStatus.value = "expired";
      tidalPollMessage.value = "设备码已过期，请重新获取";
      clearTidalPollTimer();
      return;
    }
    if (res.status === "denied") {
      tidalPollStatus.value = "denied";
      tidalPollMessage.value = "用户拒绝授权";
      clearTidalPollTimer();
      return;
    }
    if (res.status === "timeout") {
      tidalPollStatus.value = "expired";
      tidalPollMessage.value = "轮询超时，请重新获取";
      clearTidalPollTimer();
      return;
    }
    // pending / slow_down → 继续轮询
    tidalPollStatus.value = "polling";
    tidalPollMessage.value = "等待用户在浏览器中完成授权…";
    scheduleNextPoll();
  } catch (err) {
    tidalPollStatus.value = "error";
    tidalPollMessage.value = err instanceof Error ? err.message : "轮询失败";
    clearTidalPollTimer();
  }
};

/** 复制 userCode 到剪贴板 */
const copyTidalUserCode = async (): Promise<void> => {
  const code = tidalDeviceInfo.value?.userCode;
  if (!code) return;
  try {
    await navigator.clipboard.writeText(code);
    toast.success("已复制设备码");
  } catch {
    toast.error("复制失败，请手动选中复制");
  }
};

/** 打开浏览器访问 TIDAL 授权链接 */
const openTidalAuthUrl = (): void => {
  const rawUrl = tidalDeviceInfo.value?.verificationUriComplete
    ?? tidalDeviceInfo.value?.verificationUri
    ?? "https://link.tidal.com";
  const url = /^https?:\/\//i.test(rawUrl) ? rawUrl : `https://${rawUrl}`;
  window.open(url, "_blank", "noopener");
};

/** TIDAL 登出 */
const onTidalLogout = async (): Promise<void> => {
  try {
    await authStore.logoutTidal();
    cleanTidalState();
    toast.success("已退出 TIDAL 登录");
  } catch (err) {
    toast.error(err instanceof Error ? err.message : "登出失败");
  }
};

const onOpenUpdate = (value: boolean): void => {
  if (!value) clearTidalPollTimer();
  emit("update:open", value);
};
</script>

<template>
  <SDialog
    :open="open"
    title="流媒体平台登录"
    width="480px"
    @update:open="onOpenUpdate"
  >
    <STabs v-model="activeTab" :tabs="tabs" type="segment" class="mb-1">
      <!-- Qobuz Tab -->
      <template #qobuz>
        <div class="flex flex-col gap-3 pt-3">
          <!-- 已登录状态 -->
          <div
            v-if="authStore.qobuzLoggedIn"
            class="flex items-center justify-between rounded-lg bg-surface-variant/10 px-3 py-2"
          >
            <div class="flex flex-col">
              <span class="text-sm font-medium text-on-surface">已登录</span>
              <span class="text-xs text-on-surface-variant">{{
                authStore.qobuz.username || authStore.qobuz.userId
              }}</span>
            </div>
            <SButton variant="secondary" size="small" type="error" @click="onQobuzLogout">登出</SButton>
          </div>

          <!-- 登录表单 -->
          <template v-else>
            <div class="flex flex-col gap-1.5">
              <label class="text-xs text-on-surface-variant">User ID</label>
              <SInput
                v-model="qobuzUserId"
                placeholder="Qobuz 用户 ID（一串数字）"
                :disabled="qobuzLoading"
              />
            </div>
            <div class="flex flex-col gap-1.5">
              <label class="text-xs text-on-surface-variant">User Auth Token</label>
              <SInput
                v-model="qobuzAuthToken"
                type="password"
                placeholder="Qobuz 认证 Token"
                :disabled="qobuzLoading"
              />
            </div>
            <SButton :loading="qobuzLoading" type="primary" @click="onQobuzLogin">登录</SButton>
            <div class="rounded-lg bg-surface-variant/5 px-3 py-2 text-xs leading-relaxed text-on-surface-variant">
              <p class="mb-1 font-medium text-on-surface">如何获取凭证？</p>
              <p>1. 浏览器登录 play.qobuz.com</p>
              <p>2. F12 打开开发者工具 → Network</p>
              <p>3. 播放任意曲目，找到 api.json 请求</p>
              <p>4. Query String Parameters 中复制 user_id 和 user_auth_token</p>
            </div>
          </template>
        </div>
      </template>

      <!-- TIDAL Tab -->
      <template #tidal>
        <div class="flex flex-col gap-3 pt-3">
          <!-- 已登录状态 -->
          <div
            v-if="authStore.tidalLoggedIn"
            class="flex items-center justify-between rounded-lg bg-surface-variant/10 px-3 py-2"
          >
            <div class="flex flex-col">
              <span class="text-sm font-medium text-on-surface">已登录 TIDAL</span>
              <span class="text-xs text-on-surface-variant">{{ authStore.tidal.username }}</span>
            </div>
            <SButton variant="secondary" size="small" type="error" @click="onTidalLogout">登出</SButton>
          </div>

          <!-- 设备码授权流程 -->
          <template v-else>
                        <!-- 官方客户端授权码登录（无损，推荐） -->
            <div class="flex flex-col gap-2">
              <SButton
                type="primary"
                :disabled="tidalPkceStatus === 'waiting'"
                :loading="tidalPkceStatus === 'waiting'"
                @click="onTidalAuthorize"
              >
                用官方客户端登录（无损音质）
              </SButton>
              <SAlert v-if="tidalPkceStatus !== 'idle'" :type="tidalPkceType">
                {{ tidalPkceMessage }}
              </SAlert>
              <template v-if="tidalPkceStatus === 'waiting'">
                <div class="flex flex-col gap-1.5">
                  <label class="text-xs text-on-surface-variant">粘贴回调 URL</label>
                  <SInput
                    v-model="tidalCallbackUrl"
                    placeholder="https://com.player.tidal/auth?code=...&state=..."
                    :disabled="tidalExchanging"
                  />
                </div>
                <SButton
                  type="primary"
                  :loading="tidalExchanging"
                  :disabled="!tidalCallbackUrl.trim()"
                  @click="onTidalCompleteLogin"
                >
                  完成登录
                </SButton>
                <div class="rounded-lg bg-surface-variant/5 px-3 py-2 text-xs leading-relaxed text-on-surface-variant">
                  <p class="mb-1 font-medium text-on-surface">操作步骤</p>
                  <p>1. 点击上方按钮，在打开的 TIDAL 页面登录并授权</p>
                  <p>2. 浏览器会跳到 com.player.tidal/auth 报错页（正常现象）</p>
                  <p>3. 复制地址栏完整 URL，粘贴到上方输入框，点「完成登录」</p>
                </div>
              </template>
            </div>
            <div class="my-2 border-t border-surface-variant/10"></div>

<!-- 高级选项（自定义 Client ID / Secret） -->
            <div class="flex flex-col gap-1">
              <SButton
                variant="ghost"
                size="tiny"
                class="self-start text-on-surface-variant"
                @click="tidalAdvancedOpen = !tidalAdvancedOpen"
              >
                <template #icon>
                  <IconLucideChevronDown
                    class="size-3.5 transition-transform duration-200"
                    :class="tidalAdvancedOpen ? 'rotate-180' : ''"
                  />
                </template>
                自定义 Client ID / Secret（可选）
              </SButton>
              <div v-if="tidalAdvancedOpen" class="flex flex-col gap-2 mt-1">
                <div class="flex flex-col gap-1.5">
                  <label class="text-xs text-on-surface-variant">Client ID</label>
                  <SInput
                    v-model="tidalClientId"
                    placeholder="可选，未填则使用客户端内置默认值"
                    :disabled="tidalRequestLoading || tidalPollStatus === 'polling'"
                  />
                </div>
                <div class="flex flex-col gap-1.5">
                  <label class="text-xs text-on-surface-variant">Client Secret</label>
                  <SInput
                    v-model="tidalClientSecret"
                    type="password"
                    placeholder="可选"
                    :disabled="tidalRequestLoading || tidalPollStatus === 'polling'"
                  />
                </div>
              </div>
            </div>

            <!-- 初始状态：点击获取设备码 -->
            <div v-if="!tidalDeviceInfo" class="flex flex-col gap-2">
              <SButton
                :loading="tidalRequestLoading"
                type="primary"
                @click="onTidalRequestDeviceCode"
              >
                获取设备码
              </SButton>
              <div class="rounded-lg bg-surface-variant/5 px-3 py-2 text-xs leading-relaxed text-on-surface-variant">
                <p class="mb-1 font-medium text-on-surface">设备码授权说明</p>
                <p>1. 点击下方按钮生成一次性设备码</p>
                <p>2. 在打开的 TIDAL 页面输入设备码完成授权</p>
                <p>3. 授权后本应用自动获取登录态并保持登录</p>
              </div>
            </div>

            <!-- 轮询中：显示 userCode + 链接 -->
            <div v-else class="flex flex-col gap-3">
              <div class="flex flex-col gap-1.5 rounded-lg bg-surface-variant/10 px-3 py-3">
                <span class="text-xs text-on-surface-variant">设备码（userCode）</span>
                <div class="flex items-center gap-2">
                  <code class="flex-1 text-base font-mono font-semibold text-on-surface select-all break-all">
                    {{ tidalDeviceInfo.userCode }}
                  </code>
                  <SButton variant="secondary" size="small" @click="copyTidalUserCode">复制</SButton>
                </div>
              </div>

              <SButton
                type="primary"
                :disabled="tidalPollStatus === 'success'"
                @click="openTidalAuthUrl"
              >
                在浏览器中打开 TIDAL 授权
              </SButton>

              <!-- 状态消息（语义化 SAlert，替代原先的硬编码绿/红配色） -->
              <SAlert :type="tidalAlertType">
                {{ tidalPollMessage }}
              </SAlert>

              <!-- 失败时重新获取 -->
              <SButton
                v-if="tidalPollStatus === 'expired' || tidalPollStatus === 'denied' || tidalPollStatus === 'error'"
                variant="secondary"
                size="small"
                @click="onTidalRequestDeviceCode"
              >
                重新获取设备码
              </SButton>
              <SButton
                v-else-if="tidalPollStatus === 'polling'"
                variant="secondary"
                size="small"
                @click="cleanTidalState"
              >
                取消
              </SButton>
            </div>
          </template>
        </div>
      </template>
    </STabs>
  </SDialog>
</template>
