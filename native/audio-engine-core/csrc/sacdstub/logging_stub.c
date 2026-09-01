/**
 * logging_stub.c — libdstdec 所需的 logging 接口的空实现（阶段2 SACD DST 解码）。
 *
 * ## 背景
 *
 * libdstdec 的 dst_decoder.c 在错误处理路径中调用 LOG(lm_main, LOG_ERROR, ...) 等
 * 宏，依赖 libcommon/logging.h 与 libcommon/log.h。但完整 libcommon/log.c 体积较大
 * （11 KB+），且依赖环境变量、文件描述符等运行时设施。本 stub 仅提供接口契约的
 * 空实现，让链接器满意，同时让所有 LOG 宏在运行时变成 no-op。
 *
 * ## 原理
 *
 * log.h 第 200 行无条件 `#define DEBUG 1`，导致 LOG 宏始终活跃：
 *   #define LOG_TEST(_module, _level)  ((_module)->level >= (_level))
 *   #define LOG(_module, _level, _args) { if (LOG_TEST(_module, _level)) { log_print _args; } }
 *
 * 我们让 `lm_main` 指向一个 level=LOG_NONE（值为 0）的静态结构，使 LOG_TEST
 * 永远返回 false（0 >= 任何 LOG_* level 都为假），从而完全跳过 log_print 调用。
 *
 * log_print 等函数仍提供空实现以满足链接器符号需求。
 */

#include <stddef.h>

/* 通过 -I libcommon 路径直接 include 原始头文件，保证类型签名一致 */
#include "log.h"
#include "logging.h"

/* 静态 log_module_info_t：level=LOG_NONE 让 LOG_TEST 始终返回 false */
static log_module_info_t s_lm_main_stub = {
    .name  = "main",
    .level = LOG_NONE,  /* 0；任何 LOG_* >= 1 都不会触发 log_print */
    .next  = NULL,
};

/* libdstdec 引用的全局变量 */
log_module_info_t *lm_main = &s_lm_main_stub;

/* === libcommon/log.h 中声明的函数空实现 === */

void log_init(void)
{
    /* no-op */
}

void log_destroy(void)
{
    /* no-op */
}

log_module_info_t *create_log_module(const char *name)
{
    (void)name;
    return &s_lm_main_stub;
}

int set_log_file(const char *name)
{
    (void)name;
    return 0;  /* PR_FALSE 失败语义——我们不在乎 */
}

void set_log_buffering(int buffer_size)
{
    (void)buffer_size;
}

void log_print(const char *fmt, ...)
{
    (void)fmt;  /* no-op；正常情况下永远不会被调用 */
}

void log_flush(void)
{
    /* no-op */
}

void log_assert(const char *s, const char *file, int ln)
{
    (void)s;
    (void)file;
    (void)ln;
    /* no-op；libdstdec 不应触发此路径 */
}

/* === libcommon/logging.h 中声明的函数空实现 === */
/* libdstdec 不直接调用 init_logging / destroy_logging，但为完整性提供 */

void init_logging(int yes)
{
    (void)yes;
}

void destroy_logging(void)
{
    /* no-op */
}
