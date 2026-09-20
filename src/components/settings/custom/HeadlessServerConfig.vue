<script setup lang="ts">
import { headlessRemote, getHeadlessServer } from "@/services/headlessRemote";
import { toast } from "@/composables/useToast";

defineOptions({ inheritAttrs: false });

const url = ref("");
const name = ref("");
const token = ref("");
const connected = ref(false);
const testing = ref(false);

const current = getHeadlessServer();
if (current) {
  url.value = current.baseUrl;
  name.value = current.name;
  token.value = current.token ?? "";
}

const testConnection = async (): Promise<void> => {
  const baseUrl = url.value.trim().replace(/\/+$/, "");
  if (!/^https?:\/\//i.test(baseUrl)) {
    toast.error("请输入有效的 Headless 地址");
    return;
  }
  testing.value = true;
  try {
    headlessRemote.configure({
      name: name.value.trim() || baseUrl,
      baseUrl,
      token: token.value.trim() || undefined,
    });
    await headlessRemote.connect();
    connected.value = true;
    toast.success("Headless 服务器连接成功");
  } catch (error) {
    connected.value = false;
    toast.error(error instanceof Error ? error.message : "Headless 服务器连接失败");
  } finally {
    testing.value = false;
  }
};
</script>

<template>
  <div class="flex flex-col gap-3">
    <div class="text-sm text-on-surface-variant">
      手机只作为遥控器，音频由 Headless 服务器解码并输出到 Diretta。
    </div>
    <SInput v-model="name" placeholder="服务器名称，例如客厅播放器" clearable />
    <SInput v-model="url" placeholder="http://192.168.31.59:14558" spellcheck="false" clearable />
    <SInput v-model="token" type="password" placeholder="API Token（可选）" clearable />
    <div class="flex items-center gap-3">
      <SButton type="primary" :loading="testing" @click="testConnection">
        {{ testing ? "连接中..." : "保存并连接" }}
      </SButton>
      <span v-if="connected" class="text-sm text-green-500">已连接</span>
    </div>
  </div>
</template>
