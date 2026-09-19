<script setup lang="ts">
defineOptions({ name: "Library" });

import type { PlaybackContext, Track } from "@shared/types/player";
import type { DropdownMenuItem } from "@/components/ui/SDropdownMenu.vue";
import { useLibraryStore } from "@/stores/library";
import SongList from "@/components/list/SongList.vue";
import { formatFileSize } from "@/utils/format";
import IconFolderOpen from "~icons/lucide/folder-open";
import IconRefreshCw from "~icons/lucide/refresh-cw";
import IconLucideListChecks from "~icons/lucide/list-checks";
import IconLucideX from "~icons/lucide/x";
import IconLucideTrash2 from "~icons/lucide/trash-2";
import * as player from "@/core/player";
import * as queue from "@/stores/queue";
import { useStatusStore } from "@/stores/status";
import { dialog } from "@/composables/useDialog";

const { t } = useI18n();
const libraryStore = useLibraryStore();
const { scanDirs, scanning, scanProgress } = storeToRefs(libraryStore);
const pageTracks = shallowRef<Track[]>([]);
const pageTotal = ref(0);
const pageHasMore = ref(false);
const pageLoading = ref(false);
const pageOffset = ref(0);
const pageCursor = ref<string | null>(null);
const PAGE_SIZE = 200;
let pageRequestId = 0;

const playbackContext = computed<PlaybackContext>(() => ({
  originId: "library",
  originType: "page",
  originName: t("library.title"),
}));

/** 搜索关键词 */
const searchQuery = ref("");
const songListRef = shallowRef<InstanceType<typeof SongList> | null>(null);

const totalSize = computed(() => {
  const bytes = pageTracks.value.reduce((sum, track) => sum + (track.fileSize ?? 0), 0);
  return bytes > 0 ? formatFileSize(bytes) : "";
});

const loadPage = async (reset = false, query = searchQuery.value): Promise<void> => {
  const requestId = ++pageRequestId;
  if (reset) {
    pageOffset.value = 0;
    pageCursor.value = null;
    pageHasMore.value = false;
    pageTracks.value = [];
  }
  pageLoading.value = true;
  try {
    const api = window.api.library.getTracksPage;
    if (!api) return;
    const res = await api(PAGE_SIZE, pageCursor.value ? 0 : pageOffset.value, query, {
      cursor: pageCursor.value ?? undefined,
      sort: "album",
      order: "asc",
    });
    if (requestId !== pageRequestId || !res.success || !res.data) return;
    pageTotal.value = res.data.total;
    pageOffset.value += res.data.items.length;
    pageCursor.value = res.data.nextCursor ?? null;
    pageHasMore.value = Boolean(res.data.hasMore && res.data.nextCursor);
    pageTracks.value = reset ? res.data.items : [...pageTracks.value, ...res.data.items];
  } finally {
    if (requestId === pageRequestId) pageLoading.value = false;
  }
};
const loadMore = (): void => {
  if (!pageLoading.value && pageHasMore.value) void loadPage(false);
};
let searchTimer: ReturnType<typeof setTimeout> | undefined;
watch(searchQuery, (query) => {
  if (searchTimer) clearTimeout(searchTimer);
  searchTimer = setTimeout(() => void loadPage(true, query), 250);
});
/** 新增目录（手动触发扫描，避免抢占播放资源） */
const handleFolderAdded = (): void => {
  // 添加后不自动扫描，由用户按需点击扫描
};

const handleQuickAddFolder = (): void => {
  folderDialogOpen.value = true;
};

// 播放全部：顺序模式在曲目结束时按页补充队列。
const handlePlayAll = async (): Promise<void> => {
  if (pageTracks.value.length === 0) return;
  let cursor = pageCursor.value;
  if (useStatusStore().shuffleMode === "on") {
    const all = [...pageTracks.value];
    const api = window.api.library.getTracksPage;
    while (cursor && api) {
      const res = await api(PAGE_SIZE, 0, searchQuery.value, {
        cursor,
        sort: "album",
        order: "asc",
      });
      if (!res.success || !res.data) break;
      all.push(...res.data.items);
      cursor = res.data.nextCursor ?? null;
    }
    player.playFrom(all, 0, playbackContext.value);
    return;
  }
  player.playFrom(pageTracks.value, 0, playbackContext.value);
  player.setLazyQueueLoader(async () => {
    if (!cursor) return false;
    const api = window.api.library.getTracksPage;
    if (!api) return false;
    const res = await api(PAGE_SIZE, 0, searchQuery.value, { cursor, sort: "album", order: "asc" });
    if (!res.success || !res.data || res.data.items.length === 0) {
      cursor = null;
      return false;
    }
    cursor = res.data.nextCursor ?? null;
    queue.appendToQueue(res.data.items, playbackContext.value);
    return true;
  });
};

// 扫描进度百分比
const scanPercent = computed(() => {
  if (!scanProgress.value || !scanProgress.value.total) return 0;
  return Math.min(100, Math.round((scanProgress.value.scanned / scanProgress.value.total) * 100));
});

// 目录管理弹窗
const folderDialogOpen = ref(false);

const moreMenuItems = computed<DropdownMenuItem[]>(() => {
  const items: DropdownMenuItem[] = [
    { key: "batchManage", label: t("songList.batch.manage"), icon: IconLucideListChecks },
    { key: "folders", label: t("library.folders"), icon: IconFolderOpen, separator: true },
  ];
  if (scanning.value) {
    items.push({
      key: "cancelScan",
      label: "取消正在进行的扫描",
      icon: IconLucideX,
    });
  } else {
    items.push(
      {
        key: "scanIncremental",
        label: "增量扫描 (推荐)",
        icon: IconRefreshCw,
        disabled: scanDirs.value.length === 0,
      },
      {
        key: "scanFull",
        label: t("library.scanAll") || "全量重新扫描",
        icon: IconRefreshCw,
        disabled: scanDirs.value.length === 0,
      },
    );
    if (scanDirs.value.length === 0 && pageTotal.value > 0) {
      items.push({
        key: "clearLibrary",
        label: "清理媒体库记录（不删文件）",
        icon: IconLucideTrash2,
        separator: true,
      });
    }
  }
  return items;
});

const clearLibraryRecords = async (): Promise<void> => {
  const clearLibrary = window.api.library.clearLibrary;
  if (!clearLibrary) return;
  const confirmed = await dialog.confirm({
    title: "清理媒体库记录",
    content: "将清空数据库中的曲目记录和播放列表曲目关联，但不会删除磁盘上的音乐文件。确定继续吗？",
    type: "warning",
  });
  if (!confirmed) return;
  const res = await clearLibrary();
  if (!res.success) return;
  pageTracks.value = [];
  pageTotal.value = 0;
  pageHasMore.value = false;
  pageOffset.value = 0;
  pageCursor.value = null;
  await loadPage(true);
};

// 更多菜单
const handleMoreMenu = (key: string): void => {
  switch (key) {
    // 批量管理
    case "batchManage":
      songListRef.value?.enterBatch();
      break;
    // 目录管理
    case "folders":
      folderDialogOpen.value = true;
      break;
    // 增量扫描
    case "scanIncremental":
      libraryStore.startScan(true);
      break;
    // 全量扫描
    case "scanFull":
      libraryStore.startScan(false);
      break;
    // 取消扫描
    case "cancelScan":
      libraryStore.cancelScan();
      break;
    case "clearLibrary":
      void clearLibraryRecords();
      break;
  }
};

// 进入页面时初始化（仅加载数据，绝不自动触发扫描）
onMounted(() => {
  libraryStore.subscribeScanProgress({ refreshTracks: false, onDone: () => void loadPage(true) });
  void (async () => {
    if (!libraryStore.initialized) await libraryStore.load(false);
    await loadPage(true);
  })();
});

onUnmounted(() => {
  libraryStore.unsubscribeScanProgress();
});
</script>

<template>
  <div class="flex flex-col h-full">
    <!-- 顶栏 -->
    <div class="shrink-0 px-5 pb-2">
      <div class="flex items-center justify-between mt-2 mb-4">
        <div class="flex items-baseline gap-4">
          <h1 class="text-3xl font-bold text-on-surface text-balance">{{ t("library.title") }}</h1>
          <!-- 统计或进度 -->
          <Transition name="fade" mode="out-in">
            <div
              v-if="scanning && scanProgress"
              key="progress"
              class="flex items-center gap-2 text-sm text-on-surface-variant/50"
            >
              <SLoading class="size-3.5 text-primary shrink-0" />
              <span class="tabular-nums">
                {{
                  scanProgress.total > 0
                    ? t("library.scanProgress", {
                        scanned: scanProgress.scanned,
                        total: scanProgress.total,
                      })
                    : "正在准备扫描"
                }}
              </span>
              <span class="text-on-surface-variant/40 font-mono">{{ scanPercent }}%</span>
              <button
                type="button"
                class="ml-1 text-xs text-error hover:underline cursor-pointer border-none bg-transparent"
                @click="libraryStore.cancelScan()"
              >
                取消
              </button>
            </div>
            <div
              v-else-if="pageTotal > 0"
              key="stats"
              class="flex items-center gap-3 text-sm text-on-surface-variant/50"
            >
              <span class="flex items-center gap-1">
                <IconLucideMusic class="size-3.5" />
                {{ t("common.totalSongs", { count: pageTotal }) }}
              </span>
              <span v-if="!pageHasMore && totalSize" class="flex items-center gap-1">
                <IconLucideHardDrive class="size-3.5" />
                {{ totalSize }}
              </span>
            </div>
          </Transition>
        </div>
      </div>
      <!-- 操作栏 -->
      <div class="flex items-center justify-between gap-4">
        <div class="flex items-center gap-2">
          <SButton
            type="primary"
            variant="secondary"
            round
            :disabled="pageTracks.length === 0"
            @click="handlePlayAll"
          >
            <template #icon>
              <IconLucidePlay />
            </template>
            {{ t("common.playAll") }}
          </SButton>
          <!-- 手动扫描按钮 (扫描中显示取消按钮) -->
          <SButton
            v-if="!scanning"
            variant="secondary"
            circle
            :disabled="scanDirs.length === 0"
            title="手动增量扫描曲库"
            @click="libraryStore.startScan(true)"
          >
            <template #icon>
              <IconLucideRefreshCw />
            </template>
          </SButton>
          <SButton
            v-else
            type="error"
            variant="secondary"
            circle
            title="取消扫描"
            @click="libraryStore.cancelScan()"
          >
            <template #icon>
              <IconLucideX class="size-4" />
            </template>
          </SButton>
          <SDropdownMenu :items="moreMenuItems" align="start" @select="handleMoreMenu">
            <template #trigger>
              <SButton variant="secondary" circle>
                <template #icon>
                  <IconLucideEllipsis />
                </template>
              </SButton>
            </template>
          </SDropdownMenu>
        </div>
        <SInput
          v-model="searchQuery"
          :placeholder="t('common.search')"
          clearable
          round
          class="w-40 focus-within:w-56"
          data-search-input
        >
          <template #prefix>
            <IconLucideSearch class="size-4 text-on-surface-variant/40 shrink-0" />
          </template>
        </SInput>
      </div>
    </div>
    <!-- 曲目列表 -->
    <div v-if="pageTracks.length > 0" class="flex-1 min-h-0">
      <SongList
        ref="songListRef"
        :items="pageTracks"
        search-query=""
        :playback-context="playbackContext"
        show-size
        :has-more="pageHasMore"
        :loading-more="pageLoading"
        @reach-bottom="loadMore"
      />
    </div>
    <!-- 空状态：无目录或无歌曲 -->
    <div v-else class="flex-1 flex items-center justify-center">
      <div class="text-center text-on-surface-variant/50">
        <IconLucideMusic class="size-12 mx-auto mb-3 opacity-30" />
        <div class="text-sm mb-1">{{ t("library.empty") }}</div>
        <div class="text-xs mb-4 opacity-70">{{ t("library.emptyHint") }}</div>
        <SButton type="primary" variant="secondary" @click="handleQuickAddFolder">
          <template #icon><IconLucideFolderPlus /></template>
          {{ t("library.addFolder") }}
        </SButton>
      </div>
    </div>
    <!-- 文件夹管理 -->
    <SDialog
      v-model:open="folderDialogOpen"
      :title="t('library.folders')"
      :description="t('library.foldersDescription')"
      width="480px"
    >
      <FolderManager :load-library="false" @added="handleFolderAdded" />
    </SDialog>
  </div>
</template>
