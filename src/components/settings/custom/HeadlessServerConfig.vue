from pathlib import Path
p = Path("src/components/settings/custom/HeadlessServerConfig.vue")
p.write_text(r'''<script setup lang="ts">
import {
  configureHeadlessServer,
  getHeadlessServer,
  getHeadlessServers,
  headlessRemote,
  removeHeadlessServer,
  selectHeadlessServer,
  type HeadlessServerProfile,
} from "@/services/headlessRemote";
import { toast } from "@/composables/useToast";

defineOptions({ inheritAttrs: false });

const servers = ref<HeadlessServerProfile[]>(getHeadlessServers());
const activeId = ref(getHeadlessServer()?.id ?? "");
const editingId = ref<string | null>(null);
const name = ref("");
const url = ref("");
const token = ref("");
const testingId = ref<string | null>(null);

const resetForm = (): void => {
  editingId.value = null;
  name.value = "";
  url.value = "";
  token.value = "";
};

const editServer = (server: HeadlessServerProfile): void => {
  editingId.value = server.id;
  name.value = server.name;
  url.value = server.baseUrl;
  token.value = server.token ?? "";
};

const saveServer = async (): Promise<void> => {
  const baseUrl = url.value.trim().replace(/\/+$/, "");
  if (!/^https?:\/\//i.test(baseUrl)) {
    toast.error("请输入有效的 Headless 地址");
    return;
  }
  const id = editingId.value || baseUrl;
  configureHeadlessServer({
    id,
    name: name.value.trim() || baseUrl,
    baseUrl,
    token: token.value.trim() || undefined,
  });
  servers.value = getHeadlessServers();
  activeId.value = id;
  try {
    testingId.value = id;
    await headlessRemote.connect();
    toast.success("Headless 服务器连接成功");
    resetForm();
  } catch (error) {
    toast.error(error instanceof Error ? error.message : "Headless 服务器连接失败");
  } finally {
    testingId.value = null;
  }
};

const activate = async (server: HeadlessServerProfile): Promise<void> => {
  try {
    selectHeadlessServer(server.id);
    activeId.value = server.id;
    testingId.value = server.id;
    await headlessRemote.connect();
    toast.success(`已切换到 ${server.name}`);
  } catch (error) {
    toast.error(error instanceof Error ? error.message : "Headless 服务器连接失败");
  } finally {
    testingId.value = null;
  }
};

const remove = (server: HeadlessServerProfile): void => {
  if (!window.confirm(`确定删除服务器“${server.name}”吗？`)) return;
  removeHeadlessServer(server.id);
  servers.value = getHeadlessServers();
  activeId.value = getHeadlessServer()?.id ?? "";
  if (editingId.value === server.id) resetForm();
};
</script>

<template>
  <div class="flex flex-col gap-4">
    <div class="text-sm text-on-surface-variant">
      手机只作为遥控器，音频由 Headless 服务器解码并输出到 Diretta。
    </div>

    <div v-if="servers.length" class="flex flex-col gap-2">
      <div
        v-for="server in servers"
        :key="server.id"
        class="flex items-center justify-between gap-3 rounded-lg border border-outline-variant p-3"
      >
        <div class="min-w-0">
          <div class="truncate font-medium">{{ server.name }}</div>
          <div class="truncate text-xs text-on-surface-variant">{{ server.baseUrl }}</div>
          <div v-if="server.id === activeId" class="text-xs text-green-500">当前服务器</div>
        </div>
        <div class="flex shrink-0 gap-2">
          <SButton
            size="small"
            type="primary"
            :loading="testingId === server.id"
            @click="activate(server)"
          >
            {{ server.id === activeId ? "重连" : "切换" }}
          </SButton>
          <SButton size="small" variant="secondary" @click="editServer(server)">编辑</SButton>
          <SButton size="small" variant="secondary" @click="remove(server)">删除</SButton>
        </div>
      </div>
    </div>

    <div class="flex flex-col gap-3 rounded-lg border border-outline-variant p-3">
      <div class="font-medium">{{ editingId ? "编辑 Headless 服务器" : "添加 Headless 服务器" }}</div>
      <SInput v-model="name" placeholder="服务器名称，例如客厅播放器" clearable />
      <SInput v-model="url" placeholder="http://192.168.31.59:14558" spellcheck="false" clearable />
      <SInput v-model="token" type="password" placeholder="API Token（可选）" clearable />
      <div class="flex gap-3">
        <SButton type="primary" :loading="testingId === (editingId || url)" @click="saveServer">
          保存并连接
        </SButton>
        <SButton v-if="editingId" variant="secondary" @click="resetForm">取消编辑</SButton>
      </div>
    </div>
  </div>
</template>
''')
print("updated server settings")
