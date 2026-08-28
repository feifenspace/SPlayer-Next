<script setup lang="ts">
import { useLibraryStore } from "@/stores/library";
import { toast } from "@/composables/useToast";
import ServerDirPicker from "./ServerDirPicker.vue";
import IconLucideFolder from "~icons/lucide/folder";
import IconLucideFolderPlus from "~icons/lucide/folder-plus";
import IconLucideFolderSearch from "~icons/lucide/folder-search";
import IconLucideTrash2 from "~icons/lucide/trash-2";

const { t } = useI18n();
const libraryStore = useLibraryStore();
const { scanDirs } = storeToRefs(libraryStore);

const emit = defineEmits<{
  (e: "added"): void;
  (e: "removed", dir: string): void;
}>();

const folderName = (dir: string): string => {
  const parts = dir.replace(/\\/g, "/").split("/").filter(Boolean);
  return parts[parts.length - 1] || dir;
};

const adding = ref(false);
const inputPath = ref("");
const removingDir = ref<string | null>(null);
const removeConfirmOpen = ref(false);
const serverPickerOpen = ref(false);

const handleAdd = async (path?: string): Promise<void> => {
  if (adding.value) return;
  const targetPath = path ?? inputPath.value.trim();
  adding.value = true;
  try {
    const res = await libraryStore.addScanDir(targetPath || undefined);
    if (res.success) {
      inputPath.value = "";
      emit("added");
    } else if (res.error === "nested") {
      toast.warning(t("library.nestedHint"));
    } else if (res.error) {
      toast.error(res.error);
    }
  } finally {
    adding.value = false;
  }
};

const onServerDirSelected = (dir: string) => {
  void handleAdd(dir);
};

const confirmRemove = (dir: string): void => {
  removingDir.value = dir;
  removeConfirmOpen.value = true;
};

const handleRemove = async (): Promise<void> => {
  const dir = removingDir.value;
  if (!dir) return;
  await libraryStore.removeScanDir(dir);
  removeConfirmOpen.value = false;
  emit("removed", dir);
};

/** 进入时确保已同步后端目录列表 */
onMounted(() => {
  if (!libraryStore.initialized) libraryStore.load();
});
</script>

<template>
  <div class="flex flex-col gap-2">
    <!-- 已添加的目录列表 -->
    <div
      v-for="dir in scanDirs"
      :key="dir"
      class="flex items-center gap-3 px-3 py-2 rounded-lg bg-on-surface/4"
    >
      <IconLucideFolder class="size-4 text-on-surface-variant shrink-0" />
      <div class="flex-1 min-w-0">
        <div class="text-sm truncate text-on-surface">{{ folderName(dir) }}</div>
        <div class="text-xs truncate text-on-surface-variant/60 font-mono">{{ dir }}</div>
      </div>
      <SButton variant="ghost" size="small" @click="confirmRemove(dir)">
        <template #icon><IconLucideTrash2 /></template>
      </SButton>
    </div>

    <div v-if="scanDirs.length === 0" class="py-4 text-center text-on-surface-variant/50 text-sm">
      {{ t("library.emptyHint") }}
    </div>

    <!-- 浏览与添加操作区域 -->
    <div class="mt-2 flex flex-col gap-2">
      <!-- 浏览选择服务器文件夹主按钮 -->
      <SButton
        type="primary"
        variant="secondary"
        block
        @click="serverPickerOpen = true"
      >
        <template #icon><IconLucideFolderSearch /></template>
        浏览并选择服务端目录
      </SButton>

      <!-- 或手动输入路径 -->
      <div class="flex items-center gap-2">
        <SInput
          v-model="inputPath"
          placeholder="或手动输入路径 (如 /home/songlian/Music)"
          class="flex-1"
          @keydown.enter="handleAdd(inputPath)"
        />
        <SButton
          variant="secondary"
          :loading="adding"
          :disabled="!inputPath.trim()"
          @click="handleAdd(inputPath)"
        >
          <template #icon><IconLucideFolderPlus /></template>
          添加
        </SButton>
      </div>
    </div>

    <!-- 服务端目录浏览器弹窗 -->
    <ServerDirPicker
      v-model:open="serverPickerOpen"
      @select="onServerDirSelected"
    />

    <!-- 移除确认对话框 -->
    <SDialog v-model:open="removeConfirmOpen" :title="t('library.removeFolder')">
      <template #default>
        <p class="text-sm text-on-surface-variant">{{ t("library.removeFolderConfirm") }}</p>
        <p class="text-xs text-on-surface-variant/60 mt-2 break-all font-mono">{{ removingDir }}</p>
      </template>
      <template #footer="{ close }">
        <SButton variant="secondary" @click="close">
          {{ t("common.cancel") }}
        </SButton>
        <SButton type="error" @click="handleRemove">
          {{ t("common.confirm") }}
        </SButton>
      </template>
    </SDialog>
  </div>
</template>
