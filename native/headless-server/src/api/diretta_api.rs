//! Diretta 设备管理端点：扫描/状态/选择/Target 信息。

//! REST API 控制器
//!
//! 基于 Axum 0.8 的路由定义，提供播放控制、状态查询、扫描和 WebSocket 端点。

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::{extract::State, Json};
use serde::Deserialize;
use serde_json::json;

use super::spawn_isolated_blocking;
use super::PlayerResponse;
use crate::error::ApiError;
use crate::state::AppState;

/// Diretta 可达性/能力探测硬超时：DKS 调用（发现重试 + MTU 测量）内部无超时保护。
/// target_info 探测包含完整建连（扫描 + MTU 测量 + setSink + connectWait，典型 2-3s），
/// 3s 硬超时过于贴近正常耗时、易误杀，导致前端频繁查询失败 → 放宽到 6s
pub(crate) const DIRETTA_PROBE_TIMEOUT: Duration = Duration::from_secs(6);

const DIRETTA_CAPS_CACHE_TTL: Duration = Duration::from_secs(600);

fn diretta_caps_cache() -> &'static Mutex<
    HashMap<
        String,
        (
            Instant,
            audio_engine_core::diretta::DirettaTargetCapabilities,
        ),
    >,
> {
    static CACHE: OnceLock<
        Mutex<
            HashMap<
                String,
                (
                    Instant,
                    audio_engine_core::diretta::DirettaTargetCapabilities,
                ),
            >,
        >,
    > = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cached_target_caps(
    target: &str,
) -> Option<audio_engine_core::diretta::DirettaTargetCapabilities> {
    let mut cache = diretta_caps_cache().lock().ok()?;
    let (stored_at, caps) = cache.get(target)?;
    if stored_at.elapsed() > DIRETTA_CAPS_CACHE_TTL {
        cache.remove(target);
        return None;
    }
    Some(caps.clone())
}

pub(crate) fn prime_target_caps(target: String) {
    std::thread::Builder::new()
        .name("diretta-caps-prewarm".into())
        .spawn(move || {
            match audio_engine_core::diretta::query_target_caps(&target) {
                Ok(caps) => {
                    cache_target_caps(&target, caps);
                    tracing::info!(target = %target, "Diretta Target 完整能力预热缓存完成");
                }
                Err(error) => {
                    tracing::debug!(target = %target, error = %error, "Diretta Target 能力预热暂未完成");
                }
            }
        })
        .ok();
}

fn cache_target_caps(target: &str, caps: audio_engine_core::diretta::DirettaTargetCapabilities) {
    if let Ok(mut cache) = diretta_caps_cache().lock() {
        cache.insert(target.to_string(), (Instant::now(), caps));
    }
}

#[derive(Debug, Deserialize)]
pub struct DirettaSelectRequest {
    pub target: Option<String>,
}

/// 扫描局域网内的 Diretta 目标设备
pub(crate) async fn diretta_scan_handler() -> Result<Json<PlayerResponse>, ApiError> {
    let targets = spawn_isolated_blocking("diretta-scan-worker", || {
        audio_engine_core::diretta::scan_devices().unwrap_or_default()
    })
    .await
    .map_err(|e| ApiError::internal(format!("Diretta scan task failed: {e}")))?;

    Ok(Json(PlayerResponse::ok(
        serde_json::to_value(targets).unwrap_or_default(),
    )))
}

/// 获取当前 Diretta 输出状态与选中的设备
pub(crate) async fn diretta_status_handler(
    State(state): State<AppState>,
) -> Result<Json<PlayerResponse>, ApiError> {
    // 与 play/pause/stop 同款：player 锁读取走隔离线程，HTTP 线程零持锁（A2.2）
    let (selected_device, is_direct_active, is_playing) =
        spawn_isolated_blocking("diretta-status-worker", move || {
            let player = state.player.lock();
            (
                player.selected_device().map(String::from),
                player.direct_active(),
                player.state() == audio_engine_core::PlayerState::Playing,
            )
        })
        .await
        .map_err(|e| ApiError::internal(e))?;

    Ok(Json(PlayerResponse::ok(json!({
        "selected_device": selected_device,
        "is_diretta_active": is_direct_active,
        "is_online": is_direct_active,
        "is_playing": is_playing,
        "target_address": selected_device.as_deref().and_then(audio_engine_core::diretta::selector_target).unwrap_or(""),
    }))))
}

/// 切换音频输出到指定的 Diretta Target 设备（或传入 null/空 恢复默认声卡）。
///
/// 设备选择在**下一次 load 时生效**（当前曲目不受影响），并持久化到 server_state
/// （重启后 AppState 自动恢复，浏览器不在场也能连对设备）。携带 target 时会做一次
/// 可达性探测（隔离线程 + 硬超时），结果仅写入响应不阻断登记——目标可能暂时
/// 离线，用户可先登记待其上线
pub(crate) async fn diretta_select_handler(
    State(state): State<AppState>,
    Json(payload): Json<DirettaSelectRequest>,
) -> Result<Json<PlayerResponse>, ApiError> {
    let raw_target = payload.target.as_deref().map(str::trim).filter(|trimmed| {
        !trimmed.is_empty()
            && *trimmed != "undefined"
            && *trimmed != "diretta:undefined"
            && *trimmed != "null"
            && *trimmed != "diretta:null"
            && *trimmed != "system-default"
    });
    // 裸名（无 diretta:/alsammap: 前缀）有歧义：可能是 Diretta 目标名，也可能是
    // 本地 CPAL 设备 ID（Linux 下形如 alsa:xxx）。对照 /player/devices 同源的
    // 设备表判定：命中按本地输出原样保留，否则视为 Diretta 目标补 diretta: 前缀。
    // 此前一律补前缀，本地声卡被当成 Diretta 目标，PCM/DSD 全部无法播放
    let (dev_name, is_local_device) = match raw_target {
        None => (None, false),
        // 本地 ALSA 选择器（普通 ALSA 与 MMAP），非 Diretta 目标，原样保留。
        // `alsa:plughw:*` 也必须在这里识别；否则会被下面的默认分支错误
        // 包装成 `diretta:alsa:plughw:*`，导致 DSD/PCM load 无法进入 ALSA 路径。
        Some(trimmed) if trimmed.starts_with("alsa:") || trimmed.starts_with("alsammap:") => {
            (Some(trimmed.to_string()), true)
        }
        Some(trimmed) if trimmed.starts_with("diretta:") || trimmed.starts_with("diretta@") => {
            (Some(trimmed.to_string()), false)
        }
        Some(trimmed) => {
            let lookup = trimmed.to_string();
            let is_local = spawn_isolated_blocking("diretta-select-local-lookup", move || {
                audio_engine_core::audio_output::list_output_devices()
                    .iter()
                    .any(|(id, _, _)| *id == lookup)
            })
            .await
            .unwrap_or(false);
            if is_local {
                (Some(trimmed.to_string()), true)
            } else {
                (Some(format!("diretta:{}", trimmed)), false)
            }
        }
    };

    // 本地设备（alsammap / CPAL）不做 Diretta 可达性探测（本地打开失败会在
    // load 时显式报错）
    let reachable = match &dev_name {
        Some(_) if is_local_device => true,
        Some(target) => {
            let target = target.clone();
            // DKS 探测内部无超时（3 轮发现重试 + MTU 测量），Target 被占用/半死时
            // 可无限阻塞并卡死页面初始化链：硬超时兜底，按"暂不可达"放行登记。
            // 超时的探测 OS 线程无法取消，留在后台自行结束
            let target_for_log = target.clone();
            match tokio::time::timeout(
                DIRETTA_PROBE_TIMEOUT,
                spawn_isolated_blocking("diretta-select-verify", move || {
                    audio_engine_core::diretta::query_target_caps(&target)
                }),
            )
            .await
            {
                Ok(Ok(Ok(caps))) => {
                    cache_target_caps(&target_for_log, caps);
                    true
                }
                Ok(Ok(Err(e))) => {
                    tracing::warn!(target = %target_for_log, error = %e, "Diretta 可达性探测失败");
                    false
                }
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, "Diretta 可达性探测线程异常");
                    false
                }
                Err(_) => {
                    tracing::warn!(target = %target_for_log, "Diretta 可达性探测超时，按暂不可达处理");
                    false
                }
            }
        }
        None => true,
    };

    {
        let mut player = state.player.lock();
        player.set_output_device(dev_name.clone());
    }
    // 持久化本次选择（顺序取锁：player 与 db 不嵌套，避免锁序问题）
    {
        let conn = state.db.lock();
        if let Err(e) = crate::db::set_server_state(
            &conn,
            crate::state::OUTPUT_DEVICE_STATE_KEY,
            dev_name.as_deref().unwrap_or(""),
        ) {
            tracing::warn!(error = %e, "保存输出设备选择失败");
        }
    }

    Ok(Json(PlayerResponse::ok(json!({
        "status": "output_device_updated",
        "selected_device": dev_name,
        "takes_effect": "next_load",
        "reachable": reachable,
    }))))
}

#[derive(Debug, Deserialize)]
pub struct DirettaTargetInfoRequest {
    pub target: String,
}

/// 查询指定 Diretta 目标 DAC 的信息
pub(crate) async fn diretta_target_info_handler(
    State(_state): State<AppState>,
    Json(payload): Json<DirettaTargetInfoRequest>,
) -> Result<Json<PlayerResponse>, ApiError> {
    let target = payload
        .target
        .trim()
        .strip_prefix("diretta:")
        .or_else(|| payload.target.trim().strip_prefix("diretta@"))
        .unwrap_or(payload.target.trim())
        .to_string();
    if target.is_empty() || target == "undefined" || target == "null" {
        return Err(ApiError::bad_request("Diretta target is required"));
    }

    let caps = if let Some(cached) = cached_target_caps(&target) {
        tracing::info!(target = %target, "Diretta 查询复用已缓存的完整 Target 能力");
        cached
    } else {
        let target_for_query = target.clone();
        let queried = match tokio::time::timeout(
            DIRETTA_PROBE_TIMEOUT,
            spawn_isolated_blocking("diretta-info-worker", move || {
                audio_engine_core::diretta::query_target_caps(&target_for_query)
            }),
        )
        .await
    {
        Ok(Ok(result)) => result.map_err(|e| {
            tracing::warn!(target = %target, error = %e, phase = "query_target_caps", "Diretta 硬件能力查询失败");
            ApiError::internal(format!("Diretta target capability query failed: {e}"))
        })?,
        Ok(Err(e)) => {
            return Err(ApiError::internal(format!(
                "Diretta target info task failed: {e}"
            )))
        }
            Err(_) => {
                return Err(ApiError::internal(
                    "Diretta target 探测超时（DKS 调用无内部超时，已中止等待）",
                ))
            }
        };
        cache_target_caps(&target, queried.clone());
        queried
    };

    let pcm_format_desc = if caps.supports_pcm {
        format!(
            "{}-{} Hz / {}-{} bit / {}-{} 声道",
            caps.pcm_min_sample_rate,
            caps.pcm_max_sample_rate,
            caps.pcm_min_bits,
            caps.pcm_max_bits,
            caps.pcm_min_channels,
            caps.pcm_max_channels,
        )
    } else {
        "不支持 PCM".to_string()
    };
    let dsd_format_desc = if caps.supports_dsd {
        format!(
            "{}-{} Hz / {}-{} 声道 ({})",
            caps.dsd_min_sample_rate,
            caps.dsd_max_sample_rate,
            caps.dsd_min_channels,
            caps.dsd_max_channels,
            match (caps.supports_dsd_lsb, caps.supports_dsd_msb) {
                (true, true) => "LSB/MSB",
                (true, false) => "LSB",
                (false, true) => "MSB",
                (false, false) => "Native DSD",
            },
        )
    } else {
        "不支持 Native DSD".to_string()
    };
    let transmission_mode = match caps.support_ms_mode {
        0 => "Auto 自动自适应".to_string(),
        mode => format!("Auto 自动自适应 (MS 0x{mode:04x})"),
    };
    let target_address = if caps.full_addr.is_empty() {
        caps.ipv6_addr.clone()
    } else {
        caps.full_addr.clone()
    };

    Ok(Json(PlayerResponse::ok(json!({
        "target_address": target_address,
        "target_name": caps.target_name,
        "output_name": caps.output_name,
        "firmware_version": caps.firmware_version,
        "ipv6_addr": caps.ipv6_addr,
        "full_addr": caps.full_addr,
        "if_idx": caps.if_idx,
        "pcm_format_desc": pcm_format_desc,
        "dsd_format_desc": dsd_format_desc,
        "transmission_mode": transmission_mode,
        "mtu": caps.mtu_measured,
        "mtu_measured": caps.mtu_measured,
        "mtu_min": caps.mtu_min,
        "mtu_req": caps.mtu_req,
        "mtu_max": caps.mtu_max,
        "max_packet_size": caps.max_packet_size,
        "supports_pcm": caps.supports_pcm,
        "pcm_min_sample_rate": caps.pcm_min_sample_rate,
        "pcm_max_sample_rate": caps.pcm_max_sample_rate,
        "pcm_min_bits": caps.pcm_min_bits,
        "pcm_max_bits": caps.pcm_max_bits,
        "pcm_min_channels": caps.pcm_min_channels,
        "pcm_max_channels": caps.pcm_max_channels,
        "pcm_channels": caps.pcm_max_channels,
        "supports_dsd": caps.supports_dsd,
        "supports_dsd_lsb": caps.supports_dsd_lsb,
        "supports_dsd_msb": caps.supports_dsd_msb,
        "supports_native_dsd": caps.supports_dsd,
        "dsd_min_sample_rate": caps.dsd_min_sample_rate,
        "dsd_max_sample_rate": caps.dsd_max_sample_rate,
        "dsd_min_bits": caps.dsd_min_bits,
        "dsd_max_bits": caps.dsd_max_bits,
        "dsd_min_channels": caps.dsd_min_channels,
        "dsd_max_channels": caps.dsd_max_channels,
        "support_ms_mode": caps.support_ms_mode,
        "bit_perfect_supported": caps.supports_pcm || caps.supports_dsd,
        "available": true,
    }))))
}
