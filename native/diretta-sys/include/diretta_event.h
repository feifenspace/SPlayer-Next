// diretta_event.h — Diretta C Shim 事件回调定义
//
// 来源：HIFI_REFACTORING.md §11.3（回调事件类型）
//
// 本头文件定义 Diretta C Shim 层向 Rust 侧推送事件的回调签名与事件类型常量。
// 5 个事件类型对应 §11.3 表格：
//   CONNECTED / DISCONNECTED / UNDERRUN / FORMAT_OK / ERROR
//
// 设计要点：
// - 仅 C 接口，无 C++ 依赖（可被 Rust extern "C" 直接绑定）
// - 回调签名遵守 D18：回调内只允许 try_send()，不允许阻塞
//   （由 Rust 侧保证，C Shim 仅传递 event_type/error_code/user_data 三参）
// - user_data 由 Rust 侧在 sync_open 时传入，C Shim 不解释其语义
//
// 关联任务：P3.1.2（基础层声明，sync 推流在 P3.1.3 实现）

#pragma once

#include <cstdint>

#ifdef __cplusplus
extern "C" {
#endif

// 事件类型常量（§11.3）。
// 取值从 1 开始，0 保留给"无事件"。
#define DIRETTA_EVENT_CONNECTED    1
#define DIRETTA_EVENT_DISCONNECTED 2
#define DIRETTA_EVENT_UNDERRUN     3
#define DIRETTA_EVENT_FORMAT_OK    4
#define DIRETTA_EVENT_ERROR        5

// 事件回调签名。
//   event_type  : DIRETTA_EVENT_* 之一
//   error_code  : DIRETTA_OK 或 DIRETTA_ERR_*（仅 ERROR/UNDERRUN 事件携带有效值）
//   user_data   : sync_open 时由调用方传入的不透明指针，C Shim 透传
//
// 线程约束（D18）：回调在 Diretta 内部线程触发，调用方实现必须非阻塞
// （推荐 try_send() 到无界 channel），违反约束会导致音频线程卡顿。
typedef void (*diretta_event_cb)(int event_type, int error_code, void* user_data);

#ifdef __cplusplus
} // extern "C"
#endif
