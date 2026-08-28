<script setup lang="ts">
import { useStatusStore } from "@/stores/status";
import { useSettingsStore } from "@/stores/settings";
import { refreshDevices, switchDevice } from "@/core/player";
import { playerClient } from "@/services/client";
import type { DirettaTarget, DirettaTargetCapabilities } from "@/services/client/types";
import IconCast from "~icons/lucide/cast";
import IconCheck from "~icons/lucide/check";
import IconRefresh from "~icons/lucide/rotate-cw";
import IconInfo from "~icons/lucide/info";
import IconLaptop from "~icons/lucide/laptop";
import IconRadio from "~icons/lucide/radio";

const props = withDefaults(
  defineProps<{
    cover?: boolean;
  }>(),
  { cover: false },
);

const { t } = useI18n();
const status = useStatusStore();
const settings = useSettingsStore();

const SYSTEM_DEFAULT = "system-default";
const currentDevice = computed(() => settings.player.outputDevice ?? SYSTEM_DEFAULT);
const isDirettaActive = computed(() =>
  Boolean(settings.player.outputDevice?.startsWith("diretta:") || settings.player.outputDevice?.startsWith("diretta@")),
);

const popoverOpen = ref(false);
const scanning = ref(false);
const targets = ref<DirettaTarget[]>([]);
const capsModalOpen = ref(false);
const currentCaps = ref<DirettaTargetCapabilities | null>(null);
const loadingCaps = ref(false);

const buttonType = computed<"default" | "cover">(() => (props.cover ? "cover" : "default"));
const mutedClass = computed(() => (props.cover ? "text-cover/50" : "text-on-surface-variant"));

const scanTargets = async () => {
  scanning.value = true;
  try {
    const res = await playerClient.scanDirettaTargets();
    if (res.success && Array.isArray(res.data)) {
      targets.value = res.data;
    }
  } catch (e) {
    console.error("Failed to scan Diretta targets:", e);
  } finally {
    scanning.value = false;
  }
};

const handleSelect = async (devValue: string) => {
  const target = devValue === SYSTEM_DEFAULT ? null : devValue;
  await switchDevice(target);
};

const showTargetCaps = async (target: DirettaTarget) => {
  loadingCaps.value = true;
  capsModalOpen.value = true;
  try {
    const targetAddr = target.full_addr || target.ipv6_addr;
    const res = await playerClient.getDirettaTargetInfo(targetAddr);
    if (res.success && res.data) {
      currentCaps.value = res.data;
    }
  } catch (e) {
    console.error("Failed to load target capabilities:", e);
  } finally {
    loadingCaps.value = false;
  }
};

watch(popoverOpen, (open) => {
  if (open) void scanTargets();
});

onMounted(() => {
  if (status.outputDevices.length === 0) void refreshDevices();
  void scanTargets();
});
</script>

<template>
  <SPopover
    v-model:open="popoverOpen"
    trigger="click"
    side="top"
    :side-offset="12"
    :cover="cover"
    content-class="!p-0 w-80 max-h-[min(70vh,560px)] overflow-hidden flex flex-col backdrop-blur-md"
  >
    <template #trigger>
      <SButton
        :type="isDirettaActive ? 'primary' : buttonType"
        :variant="isDirettaActive ? 'tertiary' : 'ghost'"
        circle
        size="large"
        :class="isDirettaActive ? 'text-primary' : mutedClass"
        title="音频输出与 Diretta 投送"
      >
        <template #icon>
          <div class="relative flex items-center justify-center">
            <IconCast class="text-base" />
            <span
              v-if="isDirettaActive"
              class="absolute -top-1 -right-1 size-2 rounded-full bg-emerald-500 animate-pulse"
            />
          </div>
        </template>
      </SButton>
    </template>

    <!-- 弹层主体 -->
    <div class="flex flex-col h-full select-none text-xs">
      <!-- 头部 -->
      <div
        class="flex items-center justify-between px-3.5 py-2.5 border-b border-b-solid"
        :class="cover ? 'border-b-white/10 text-cover' : 'border-b-on-surface/8 text-on-surface'"
      >
        <div class="flex items-center gap-1.5 font-medium text-sm">
          <IconRadio class="text-primary text-base" />
          <span>音频输出设备</span>
        </div>
        <SButton
          variant="text"
          circle
          size="small"
          :title="t('common.refresh')"
          :loading="scanning"
          @click="scanTargets"
        >
          <template #icon>
            <IconRefresh :class="['text-xs', scanning && 'animate-spin']" />
          </template>
        </SButton>
      </div>

      <!-- 设备列表区域 -->
      <div class="flex-1 overflow-y-auto p-2 space-y-3">
        <!-- 本地设备分组 -->
        <div>
          <div
            class="px-2 py-1 text-[11px] font-semibold uppercase tracking-wider"
            :class="cover ? 'text-cover/40' : 'text-on-surface-variant/60'"
          >
            本地输出
          </div>
          <div class="mt-1 space-y-1">
            <!-- 系统默认 -->
            <div
              class="flex items-center justify-between px-2.5 py-2 rounded-lg cursor-pointer transition-colors"
              :class="[
                currentDevice === SYSTEM_DEFAULT
                  ? 'bg-primary/12 text-primary font-medium'
                  : cover
                    ? 'hover:bg-white/8 text-cover/80'
                    : 'hover:bg-on-surface/6 text-on-surface',
              ]"
              @click="handleSelect(SYSTEM_DEFAULT)"
            >
              <div class="flex items-center gap-2 truncate">
                <IconLaptop class="text-sm shrink-0" />
                <span class="truncate">{{ t("settings.outputDevice.default") }}</span>
              </div>
              <IconCheck v-if="currentDevice === SYSTEM_DEFAULT" class="text-sm shrink-0" />
            </div>

            <!-- 本地硬件声卡 -->
            <div
              v-for="dev in status.outputDevices"
              :key="dev.name"
              class="flex items-center justify-between px-2.5 py-2 rounded-lg cursor-pointer transition-colors"
              :class="[
                currentDevice === dev.name
                  ? 'bg-primary/12 text-primary font-medium'
                  : cover
                    ? 'hover:bg-white/8 text-cover/80'
                    : 'hover:bg-on-surface/6 text-on-surface',
              ]"
              @click="handleSelect(dev.name)"
            >
              <div class="flex items-center gap-2 truncate">
                <IconLaptop class="text-sm shrink-0" />
                <span class="truncate">{{ dev.name }}</span>
              </div>
              <IconCheck v-if="currentDevice === dev.name" class="text-sm shrink-0" />
            </div>
          </div>
        </div>

        <!-- Diretta 网络 DAC 分组 -->
        <div>
          <div
            class="flex items-center justify-between px-2 py-1 text-[11px] font-semibold uppercase tracking-wider"
            :class="cover ? 'text-cover/40' : 'text-on-surface-variant/60'"
          >
            <span>Diretta Target (网络 DAC)</span>
            <span v-if="targets.length > 0" class="text-emerald-500 font-mono">
              {{ targets.length }} 在线
            </span>
          </div>

          <div class="mt-1 space-y-1">
            <div
              v-for="target in targets"
              :key="target.full_addr || target.ipv6_addr"
              class="flex items-center justify-between px-2.5 py-2 rounded-lg cursor-pointer transition-colors"
              :class="[
                currentDevice === `diretta:${target.full_addr || target.ipv6_addr}`
                  ? 'bg-emerald-500/15 text-emerald-600 dark:text-emerald-400 font-medium'
                  : cover
                    ? 'hover:bg-white/8 text-cover/80'
                    : 'hover:bg-on-surface/6 text-on-surface',
              ]"
              @click="handleSelect(`diretta:${target.full_addr || target.ipv6_addr}`)"
            >
              <div class="flex items-center gap-2 truncate flex-1 min-w-0 pr-2">
                <span class="size-2 rounded-full bg-emerald-500 shrink-0" />
                <div class="flex flex-col truncate">
                  <span class="truncate text-xs">
                    {{ target.target_name || target.model_name || "Diretta Target" }}
                  </span>
                  <span class="text-[10px] opacity-60 font-mono truncate">
                    {{ target.ipv6_addr }}
                  </span>
                </div>
              </div>

              <div class="flex items-center gap-1 shrink-0">
                <SButton
                  variant="ghost"
                  circle
                  size="tiny"
                  title="查看 DAC 硬件能力"
                  @click.stop="showTargetCaps(target)"
                >
                  <template #icon><IconInfo class="text-xs" /></template>
                </SButton>
                <IconCheck
                  v-if="currentDevice === `diretta:${target.full_addr || target.ipv6_addr}`"
                  class="text-sm text-emerald-500 shrink-0"
                />
              </div>
            </div>

            <!-- 无设备提示 -->
            <div
              v-if="targets.length === 0"
              class="px-3 py-4 text-center rounded-lg border border-dashed text-xs"
              :class="cover ? 'border-white/15 text-cover/50' : 'border-on-surface/10 text-on-surface-variant/60'"
            >
              <div v-if="scanning" class="flex items-center justify-center gap-2">
                <IconRefresh class="animate-spin text-sm" />
                <span>正在扫描局域网 Diretta 设备...</span>
              </div>
              <div v-else>
                <span>未发现 Diretta Target</span>
                <p class="text-[10px] mt-1 opacity-70">请确保 Target 已开机并与本机处于同一子网</p>
              </div>
            </div>
          </div>
        </div>
      </div>
    </div>
  </SPopover>

  <!-- DAC 硬件解码能力详情弹窗 -->
  <SDialog
    v-model:open="capsModalOpen"
    title="Diretta Target 硬件解码规格"
    width="440px"
  >
    <div v-if="loadingCaps" class="py-8 text-center text-sm opacity-70">
      <IconRefresh class="animate-spin inline-block mr-2 text-base" />
      <span>正在查询目标 DAC 芯片能力...</span>
    </div>
    <div v-else-if="currentCaps" class="space-y-3 text-xs">
      <div class="p-3 rounded-lg bg-primary/8 border border-primary/15 space-y-1">
        <div class="font-medium text-sm text-primary">Diretta 发烧级音频流协议</div>
        <div class="text-[11px] opacity-75 font-mono truncate">
          目标地址: {{ currentCaps.target_address }}
        </div>
      </div>

      <div class="space-y-2">
        <div class="flex justify-between py-1.5 border-b border-on-surface/6">
          <span class="text-on-surface-variant">PCM 高清能力</span>
          <span class="font-medium">{{ currentCaps.pcm_format_desc }}</span>
        </div>
        <div class="flex justify-between py-1.5 border-b border-on-surface/6">
          <span class="text-on-surface-variant">DSD 原生能力</span>
          <span class="font-medium text-emerald-600 dark:text-emerald-400">
            {{ currentCaps.dsd_format_desc }}
          </span>
        </div>
        <div class="flex justify-between py-1.5 border-b border-on-surface/6">
          <span class="text-on-surface-variant">传输微周期模式</span>
          <span class="font-mono">{{ currentCaps.transmission_mode }}</span>
        </div>
        <div class="flex justify-between py-1.5 border-b border-on-surface/6">
          <span class="text-on-surface-variant">实测网络 MTU</span>
          <span class="font-mono">{{ currentCaps.mtu }} Bytes</span>
        </div>
        <div class="flex justify-between py-1.5">
          <span class="text-on-surface-variant">Bit-Perfect 纯净直通</span>
          <span class="text-emerald-500 font-medium">支持 (DSP 自动避让)</span>
        </div>
      </div>
    </div>
  </SDialog>
</template>
