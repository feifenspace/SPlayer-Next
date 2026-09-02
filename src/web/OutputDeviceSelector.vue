<script setup lang="ts">
import { refreshDevices, switchDevice } from "@/core/player";
import { useSettingsStore } from "@/stores/settings";
import { useStatusStore } from "@/stores/status";

const { t } = useI18n();
const status = useStatusStore();
const settings = useSettingsStore();

/**
 * Headless Web 不展示指向用户态 PipeWire 的 alsa:default，只暴露可直接打开的硬件 PCM 与 Diretta。
 */
const options = computed(() =>
  status.outputDevices
    .filter(
      (device) =>
        device.id.startsWith("diretta:") ||
        /^alsa:hw:CARD=[A-Za-z_][^,]*,DEV=\d+$/.test(device.id) ||
        !device.id.startsWith("alsa:"),
    )
    .map((device) => ({
      value: device.id,
      label: device.id.startsWith("alsa:hw:") ? `ALSA · ${device.name}` : device.name,
    })),
);

const current = computed(() => settings.player.outputDevice ?? "");

const onChange = (value: string | number | boolean): void => {
  void switchDevice(String(value));
};

onMounted(() => {
  if (status.outputDevices.length === 0) void refreshDevices();
});
</script>

<template>
  <SSelect
    :model-value="current"
    :options="options"
    :placeholder="t('settings.outputDevice.label')"
    @update:model-value="onChange"
  />
</template>
