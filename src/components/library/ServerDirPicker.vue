<script setup lang="ts">
import { playerClient } from "@/services/client";
import IconLucideFolder from "~icons/lucide/folder";
import IconLucideFolderOpen from "~icons/lucide/folder-open";
import IconLucideChevronRight from "~icons/lucide/chevron-right";
import IconLucideArrowUp from "~icons/lucide/arrow-up";
import IconLucideMusic from "~icons/lucide/music";
import IconLucideRefreshCw from "~icons/lucide/refresh-cw";
import IconLucideHome from "~icons/lucide/home";
import IconLucideHardDrive from "~icons/lucide/hard-drive";
import IconLucideCheck from "~icons/lucide/check";

const props = defineProps<{
  open: boolean;
}>();

const emit = defineEmits<{
  (e: "update:open", val: boolean): void;
  (e: "select", path: string): void;
}>();

const { t } = useI18n();
const loading = ref(false);
const currentPath = ref("/");
const parentPath = ref<string | null>(null);
const dirs = ref<Array<{ name: string; path: string; has_children?: boolean }>>([]);
const audioCount = ref(0);
const selectedPath = ref("");

// 面包屑路径计算
const breadcrumbs = computed(() => {
  const p = currentPath.value;
  if (!p || p === "/") return [{ name: "根目录", path: "/" }];
  const parts = p.split("/").filter(Boolean);
  const crumbs = [{ name: "根目录", path: "/" }];
  let accum = "";
  for (const part of parts) {
    accum += `/${part}`;
    crumbs.push({ name: part, path: accum });
  }
  return crumbs;
});

const loadPath = async (targetPath?: string) => {
  loading.value = true;
  try {
    const res = await playerClient.browseFs(targetPath);
    if (res.success && res.data) {
      currentPath.value = res.data.current_path;
      parentPath.value = res.data.parent_path;
      dirs.value = res.data.dirs || [];
      audioCount.value = res.data.audio_count || 0;
      selectedPath.value = res.data.current_path;
    }
  } catch (err) {
    console.error("Failed to browse server fs:", err);
  } finally {
    loading.value = false;
  }
};

const navigateTo = (path: string) => {
  void loadPath(path);
};

const goUp = () => {
  if (parentPath.value) {
    void loadPath(parentPath.value);
  } else if (currentPath.value !== "/") {
    void loadPath("/");
  }
};

const handleConfirm = () => {
  const chosen = selectedPath.value.trim() || currentPath.value.trim();
  if (chosen && chosen !== "/") {
    emit("select", chosen);
    emit("update:open", false);
  }
};

watch(
  () => props.open,
  (isOpen) => {
    if (isOpen) {
      void loadPath(currentPath.value || "/");
    }
  },
  { immediate: true },
);
</script>

<template>
  <SDialog
    :open="open"
    title="浏览并选择服务端音乐目录"
    width="540px"
    @update:open="emit('update:open', $event)"
  >
    <div class="flex flex-col gap-3 text-xs select-none">
      <!-- 快捷跳转 -->
      <div class="flex items-center gap-1.5 flex-wrap pb-1">
        <span class="text-[11px] text-on-surface-variant/60 font-medium mr-1">快捷位置:</span>
        <button
          type="button"
          class="px-2 py-1 rounded-md bg-on-surface/5 hover:bg-primary/10 hover:text-primary transition-colors flex items-center gap-1 cursor-pointer border-none text-[11px] text-on-surface"
          @click="navigateTo('/')"
        >
          <IconLucideHardDrive class="size-3" />
          <span>根目录 /</span>
        </button>
        <button
          type="button"
          class="px-2 py-1 rounded-md bg-on-surface/5 hover:bg-primary/10 hover:text-primary transition-colors flex items-center gap-1 cursor-pointer border-none text-[11px] text-on-surface"
          @click="navigateTo('/home')"
        >
          <IconLucideHome class="size-3" />
          <span>/home</span>
        </button>
        <button
          type="button"
          class="px-2 py-1 rounded-md bg-on-surface/5 hover:bg-primary/10 hover:text-primary transition-colors flex items-center gap-1 cursor-pointer border-none text-[11px] text-on-surface"
          @click="navigateTo('/media')"
        >
          <IconLucideHardDrive class="size-3" />
          <span>/media</span>
        </button>
        <button
          type="button"
          class="px-2 py-1 rounded-md bg-on-surface/5 hover:bg-primary/10 hover:text-primary transition-colors flex items-center gap-1 cursor-pointer border-none text-[11px] text-on-surface"
          @click="navigateTo('/mnt')"
        >
          <IconLucideHardDrive class="size-3" />
          <span>/mnt</span>
        </button>
        <button
          type="button"
          class="px-2 py-1 rounded-md bg-on-surface/5 hover:bg-primary/10 hover:text-primary transition-colors flex items-center gap-1 cursor-pointer border-none text-[11px] text-on-surface"
          @click="navigateTo('/data')"
        >
          <IconLucideHardDrive class="size-3" />
          <span>/data</span>
        </button>
      </div>

      <!-- 当前路径面包屑导航 -->
      <div class="flex items-center gap-1.5 px-3 py-2 rounded-lg bg-field border border-solid border-on-surface/10 overflow-hidden">
        <SButton
          variant="ghost"
          circle
          size="tiny"
          :disabled="!parentPath && currentPath === '/'"
          title="返回上一层"
          @click="goUp"
        >
          <template #icon><IconLucideArrowUp class="size-3.5" /></template>
        </SButton>
        <div class="flex-1 min-w-0 flex items-center gap-1 overflow-x-auto whitespace-nowrap scrollbar-none font-mono text-[11px]">
          <template v-for="(crumb, idx) in breadcrumbs" :key="crumb.path">
            <span
              class="cursor-pointer hover:text-primary hover:underline transition-colors shrink-0"
              :class="idx === breadcrumbs.length - 1 ? 'font-bold text-on-surface' : 'text-on-surface-variant/70'"
              @click="navigateTo(crumb.path)"
            >
              {{ crumb.name }}
            </span>
            <IconLucideChevronRight
              v-if="idx < breadcrumbs.length - 1"
              class="size-3 text-on-surface-variant/30 shrink-0"
            />
          </template>
        </div>
        <SButton
          variant="ghost"
          circle
          size="tiny"
          :loading="loading"
          title="刷新目录"
          @click="loadPath(currentPath)"
        >
          <template #icon><IconLucideRefreshCw class="size-3.5" /></template>
        </SButton>
      </div>

      <!-- 子文件夹列表 -->
      <div class="h-64 overflow-y-auto rounded-lg border border-solid border-on-surface/8 p-1.5 space-y-1 bg-surface-variant/10">
        <div v-if="loading" class="h-full flex items-center justify-center text-on-surface-variant/60 gap-2">
          <IconLucideRefreshCw class="animate-spin size-4" />
          <span>正在读取服务器目录...</span>
        </div>

        <template v-else-if="dirs.length > 0">
          <div
            v-for="d in dirs"
            :key="d.path"
            class="flex items-center justify-between px-3 py-2 rounded-lg cursor-pointer transition-colors group hover:bg-on-surface/6 text-on-surface"
            @click="navigateTo(d.path)"
          >
            <div class="flex items-center gap-2.5 truncate min-w-0 flex-1">
              <IconLucideFolder class="size-4 text-primary shrink-0 group-hover:scale-110 transition-transform" />
              <span class="truncate text-xs">{{ d.name }}</span>
            </div>
            <IconLucideChevronRight class="size-3.5 text-on-surface-variant/40 shrink-0 group-hover:translate-x-0.5 transition-transform" />
          </div>
        </template>

        <div v-else class="h-full flex flex-col items-center justify-center text-center text-on-surface-variant/50 p-4">
          <IconLucideFolderOpen class="size-8 opacity-30 mb-2" />
          <span>此目录下没有子文件夹</span>
          <span v-if="audioCount > 0" class="text-emerald-500 font-medium mt-1">
            发现 {{ audioCount }} 首音频文件
          </span>
        </div>
      </div>

      <!-- 当前选中路径与曲目数概览 -->
      <div class="flex items-center justify-between px-2 pt-1">
        <div class="flex items-center gap-2 min-w-0 flex-1 text-xs">
          <span class="text-on-surface-variant shrink-0">当前选中:</span>
          <span class="font-mono text-primary truncate font-medium">{{ currentPath }}</span>
        </div>
        <div v-if="audioCount > 0" class="flex items-center gap-1 text-emerald-500 text-[11px] shrink-0 font-medium pl-2">
          <IconLucideMusic class="size-3.5" />
          <span>包含 {{ audioCount }} 首歌曲</span>
        </div>
      </div>
    </div>

    <template #footer="{ close }">
      <SButton variant="secondary" @click="close">
        {{ t("common.cancel") }}
      </SButton>
      <SButton
        type="primary"
        :disabled="!currentPath || currentPath === '/'"
        @click="handleConfirm"
      >
        <template #icon><IconLucideCheck /></template>
        选择此目录添加至曲库
      </SButton>
    </template>
  </SDialog>
</template>
