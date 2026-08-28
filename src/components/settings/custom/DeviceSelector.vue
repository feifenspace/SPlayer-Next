<script setup lang="ts">
import { useStatusStore } from "@/stores/status";
import { useSettingsStore } from "@/stores/settings";
import { refreshDevices, switchDevice } from "@/core/player";
import { playerClient } from "@/services/client";
import type { DirettaTarget } from "@/services/client/types";
import IconRefresh from "~icons/lucide/rotate-cw";

defineOptions({ inheritAttrs: false });

const { t } = useI18n();
const status = useStatusStore();
const settings = useSettingsStore();

// 系统默认
const SYSTEM_DEFAULT = "system-default";

const current = computed(() => settings.player.outputDevice ?? SYSTEM_DEFAULT);
const direttaTargets = ref<DirettaTarget[]>([]);
const scanningDiretta = ref(false);

const scanDiretta = async () => {
  scanningDiretta.value = true;
  try {
    const res = await playerClient.scanDirettaTargets();
    if (res.success && Array.isArray(res.data)) {
      direttaTargets.value = res.data;
    }
  } catch (e) {
    console.error("Failed to scan Diretta targets:", e);
  } finally {
    scanningDiretta.value = false;
  }
};

const options = computed(() => {
  const defaultName = status.outputDevices.find((d) => d.isDefault)?.name;
  const defaultLabel = defaultName
    ? `${t("settings.outputDevice.default")}（${defaultName}）`
    : t("settings.outputDevice.default");

  const opts = [
    { value: SYSTEM_DEFAULT, label: defaultLabel },
    ...status.outputDevices.map((d) => ({ value: d.name, label: d.name })),
  ];

  // 加入扫描到的 Diretta 网络 DAC 设备
  if (direttaTargets.value.length > 0) {
    for (const target of direttaTargets.value) {
      const displayName = target.target_name || target.model_name || target.output_name || "Target";
      const targetVal = `diretta:${target.full_addr || target.ipv6_addr}`;
      opts.push({
        value: targetVal,
        label: `Diretta: ${displayName}`,
      });
    }
  }

  return opts;
});

const onChange = (value: string | number | boolean) => {
  switchDevice(value === SYSTEM_DEFAULT ? null : String(value));
};

onMounted(() => {
  if (status.outputDevices.length === 0) refreshDevices();
  void scanDiretta();
});
</script>

<template>
  <div class="flex items-center gap-1.5 w-full min-w-0">
    <div class="flex-1 min-w-0">
      <SSelect
        :model-value="current"
        :options="options"
        :placeholder="t('settings.outputDevice.default')"
        @update:model-value="onChange"
      />
    </div>
    <SButton
      variant="secondary"
      size="small"
      :loading="scanningDiretta"
      title="扫描 Diretta Target 网络设备"
      class="shrink-0"
      @click="scanDiretta"
    >
      <IconRefresh class="text-sm" />
    </SButton>
  </div>
</template>
