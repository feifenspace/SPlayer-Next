# Client 适配层整理记录

## 2026-09-11：统一播放连接

原 getClient() 与 installWebPolyfill() 各自 new HttpPlayerClient，而构造函数会启动 WebSocket。两套入口可产生重复连接，各自维护事件监听、协议状态和重连定时器。

现在二者通过 httpClientInstance.ts 的 getHttpPlayerClient() 获取同一实例。window.api.player 与 getClient().player 指向同一对象；兼容桥重复安装不会替换已有 API 对象。Electron 环境仍使用原生接口，不创建 HTTP 实例。

实例由页面入口持有；业务组件只解除自己的订阅，不应销毁共享实例。直接构造 HttpPlayerClient 仍可用于独立连接和测试，调用者负责销毁。跨页面生命周期和 HMR 回收机制不在本次改动范围。

回归覆盖：兼容桥先安装、业务入口先初始化、重复安装、原生 Electron 接口保护。新增测试使用构造替身确认实例数，不把它当作真实浏览器连接数观测。

## 后续边界

- 曲库、配置、在线服务接口继续经现有兼容桥提供，尚未宣称完成 IAppClient 全面收拢。
- createSafeProxy 的未知接口默认结果暂保留；需要逐项核对调用方、返回契约和界面能力提示后再改变。
- 真实浏览器播放、断线重连、页面关闭后服务端接续需在部署验收中检查。
- 保留维护者要求的打包前自动清理 target/debug 行为。
