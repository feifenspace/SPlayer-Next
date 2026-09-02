<script setup lang="ts">
import type { ListenBrainzStatus } from "@shared/types/listenbrainz";
import { toast } from "@/composables/useToast";
import IconLucideHeadphones from "~icons/lucide/headphones";
import IconLucideUnplug from "~icons/lucide/unplug";

defineOptions({ inheritAttrs: false });

const { t } = useI18n();
const status = ref<ListenBrainzStatus>({
  enabled: false,
  sendNowPlaying: true,
  linked: false,
  account: null,
  state: "disabled",
  pending: 0,
  dead: 0,
  lastError: null,
  processActive: false,
});
const token = ref("");
const busy = ref(false);

const refresh = async (): Promise<void> => {
  status.value = await window.api.listenbrainz.getStatus();
};

onMounted(refresh);

const handleLink = async (): Promise<void> => {
  const value = token.value.trim();
  if (!value) return;
  busy.value = true;
  try {
    const res = await window.api.listenbrainz.link(value);
    if (!res.ok) {
      toast.error(
        t("settings.listenbrainz.toast.failed", {
          error: res.error || "连接失败",
        }),
      );
    } else {
      token.value = "";
      await refresh();
      toast.success(t("settings.listenbrainz.toast.connected", { name: res.account ?? "" }));
    }
  } catch (error) {
    toast.error(
      t("settings.listenbrainz.toast.failed", {
        error: error instanceof Error ? error.message : String(error),
      }),
    );
  } finally {
    busy.value = false;
  }
};

const handleUnlink = async (): Promise<void> => {
  busy.value = true;
  try {
    await window.api.listenbrainz.unlink();
    await refresh();
    toast.success(t("settings.listenbrainz.toast.disconnected"));
  } finally {
    busy.value = false;
  }
};
</script>

<template>
  <div class="flex flex-col gap-3">
    <div
      class="flex items-center justify-between gap-4 rounded-xl bg-surface-panel border border-solid border-outline-variant/15 px-4 py-3.5"
    >
      <div class="flex items-center gap-3 min-w-0 flex-1">
        <div
          class="size-10 rounded-xl bg-on-surface/6 flex items-center justify-center text-on-surface-variant shrink-0"
        >
          <IconLucideHeadphones class="size-5" />
        </div>
        <div class="min-w-0 flex-1">
          <div class="text-sm font-medium text-on-surface truncate">
            {{
              status.linked
                ? t("settings.listenbrainz.connectedAs", { name: status.account ?? "" })
                : t("settings.listenbrainz.notConnected")
            }}
          </div>
          <div class="text-xs text-on-surface-variant/60 mt-0.5">
            {{ t(`settings.listenbrainz.state.${status.state}`) }}
          </div>
          <div v-if="status.lastError" class="text-xs text-error mt-1 break-words">
            {{ status.lastError }}
          </div>
        </div>
      </div>

      <SButton
        v-if="status.linked"
        variant="secondary"
        size="small"
        type="error"
        :loading="busy"
        @click="handleUnlink"
      >
        <template #icon><IconLucideUnplug class="size-4" /></template>
        {{ t("settings.listenbrainz.disconnect") }}
      </SButton>
    </div>

    <div v-if="!status.linked" class="flex items-center gap-2">
      <SInput
        v-model="token"
        type="password"
        autocomplete="new-password"
        :placeholder="t('settings.listenbrainz.tokenPlaceholder')"
        @keydown.enter="handleLink"
      />
      <SButton type="primary" :loading="busy" :disabled="!token.trim()" @click="handleLink">
        {{ t("settings.listenbrainz.connect") }}
      </SButton>
    </div>
  </div>
</template>
