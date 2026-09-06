/// 音频线程 SCHED_FIFO 实时调度优先级（1-99，数值越高越优先）。
/// 默认 70：高于普通系统 I/O 任务（通常 ≤50），低于内核级硬实时（≥80）。
/// 可经 `configure_rt_priority` 调整（蓝图 §3.2 要求 75~85，headless 配置注入）
static AUDIO_RT_PRIORITY: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(70);

/// 配置音频线程 RT 优先级（1-99，越界取默认 70）。须在音频线程启动前调用
pub fn configure_rt_priority(priority: i32) {
    let value = if (1..=99).contains(&priority) {
        priority
    } else {
        tracing::warn!(priority, "RT 优先级越界，回退默认 70");
        70
    };
    AUDIO_RT_PRIORITY.store(value, std::sync::atomic::Ordering::Release);
}

/// 解析内核 cpulist 格式（如 "2-3,5,8-9"）为 CPU ID 列表
fn parse_cpulist(spec: &str) -> Vec<u32> {
    let mut cpus = Vec::new();
    for part in spec.split(',') {
        let part = part.trim();
        if let Some((start, end)) = part.split_once('-') {
            if let (Ok(a), Ok(b)) = (start.trim().parse::<u32>(), end.trim().parse::<u32>()) {
                if a <= b {
                    cpus.extend(a..=b);
                }
            }
        } else if let Ok(id) = part.parse::<u32>() {
            cpus.push(id);
        }
    }
    cpus
}

// ── Windows ─────────────────────────────────────────────────────────────────
#[cfg(target_os = "windows")]
mod imp {
    use tracing::warn;
    use windows::Win32::System::Threading::{
        GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_HIGHEST,
    };

    pub fn boost_current_audio_thread(name: &str) {
        unsafe {
            if let Err(err) = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST) {
                warn!(thread = name, error = %err, "设置音频线程优先级失败");
            }
        }
    }

    pub fn bind_current_thread_to_performance_cores(_name: &str) {
        // Windows 不在此实现：CPU 亲和力已由 WASAPI/MMCSS 管理
    }
}

// ── Linux ────────────────────────────────────────────────────────────────────
#[cfg(target_os = "linux")]
mod imp {
    use super::{parse_cpulist, AUDIO_RT_PRIORITY};
    use std::sync::atomic::Ordering;
    use tracing::{debug, warn};

    /// 读取 Linux CPU 拓扑，返回"性能核"的 CPU ID 列表。
    ///
    /// 判定策略：
    /// - ARM big.LITTLE：读取 `/sys/devices/system/cpu/cpuN/cpufreq/cpuinfo_max_freq`，
    ///   取最高频率，凡与最高频率相同的核即为 Performance Core（大核）。
    /// - x86（含多路服务器）：读取 `/sys/devices/system/cpu/cpuN/topology/core_id`，
    ///   仅保留每个物理 core_id 的第一个逻辑线程（避开超线程虚拟核），以降低调度抖动。
    /// - 探测失败时返回空 Vec，调用方退化为不绑核。
    fn detect_performance_cores() -> Vec<u32> {
        // 蓝图 §四：rt-tuning 脚本产出的隔离核优先（isolcpus 专核，无系统负载干扰）
        if let Ok(spec) = std::fs::read_to_string("/sys/devices/system/cpu/isolated") {
            let isolated = parse_cpulist(spec.trim());
            if !isolated.is_empty() {
                debug!("CPU 亲和力：使用隔离核 {:?}", isolated);
                return isolated;
            }
        }

        // 先探测 CPU 数量
        let nproc = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_CONF) };
        if nproc <= 0 {
            return Vec::new();
        }
        let nproc = nproc as usize;

        // 尝试读取每个 CPU 的最大频率（ARM big.LITTLE 场景）
        let mut freq_map: Vec<Option<u64>> = vec![None; nproc];
        let mut max_freq: u64 = 0;
        let mut has_freq_info = false;

        for cpu in 0..nproc {
            let path = format!("/sys/devices/system/cpu/cpu{cpu}/cpufreq/cpuinfo_max_freq");
            if let Ok(content) = std::fs::read_to_string(&path) {
                if let Ok(freq) = content.trim().parse::<u64>() {
                    freq_map[cpu] = Some(freq);
                    if freq > max_freq {
                        max_freq = freq;
                    }
                    has_freq_info = true;
                }
            }
        }

        if has_freq_info && max_freq > 0 {
            // ARM 策略：只选频率等于最大频率的核（大核）
            let perf_cores: Vec<u32> = (0..nproc)
                .filter(|&cpu| freq_map[cpu] == Some(max_freq))
                .map(|cpu| cpu as u32)
                .collect();
            if !perf_cores.is_empty() {
                debug!("CPU 亲和力：ARM 大核识别 {:?}", perf_cores);
                return perf_cores;
            }
        }

        // x86 策略：每个物理 core_id 只取第一个逻辑 CPU（避免超线程）
        let mut seen_core_ids = std::collections::HashSet::new();
        let mut physical_cores: Vec<u32> = Vec::new();
        for cpu in 0..nproc {
            let path = format!("/sys/devices/system/cpu/cpu{cpu}/topology/core_id");
            if let Ok(content) = std::fs::read_to_string(&path) {
                if let Ok(core_id) = content.trim().parse::<u32>() {
                    if seen_core_ids.insert(core_id) {
                        physical_cores.push(cpu as u32);
                    }
                }
            }
        }

        if !physical_cores.is_empty() {
            debug!("CPU 亲和力：x86 物理核识别 {:?}", physical_cores);
            physical_cores
        } else {
            debug!("CPU 亲和力：探测失败，退化为不绑核");
            Vec::new()
        }
    }

    /// 将当前线程绑定到 Performance Core（ARM 大核 / x86 独立物理核）。
    ///
    /// 自动探测 CPU 拓扑，失败时静默降级（不绑核），不影响音频功能。
    pub fn bind_current_thread_to_performance_cores(name: &str) {
        let perf_cores = detect_performance_cores();
        if perf_cores.is_empty() {
            debug!(thread = name, "CPU 亲和力：无性能核信息，跳过绑定");
            return;
        }

        // 构造 cpu_set_t 并通过 sched_setaffinity 绑定
        let mut cpu_set = unsafe { std::mem::zeroed::<libc::cpu_set_t>() };
        for &cpu in &perf_cores {
            unsafe { libc::CPU_SET(cpu as usize, &mut cpu_set) };
        }
        let ret =
            unsafe { libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &cpu_set) };
        if ret == 0 {
            debug!(thread = name, cores = ?perf_cores, "CPU 亲和力：已绑定到性能核");
        } else {
            let errno = unsafe { *libc::__errno_location() };
            warn!(
                thread = name,
                errno = errno,
                "CPU 亲和力：sched_setaffinity 失败（可能需要 CAP_SYS_NICE 权限）"
            );
        }
    }

    /// 将当前线程升级为 SCHED_FIFO 实时调度策略。
    ///
    /// 优先级 70（高于系统 I/O，低于内核硬实时）。
    /// 需要 CAP_SYS_NICE 或 /etc/security/limits.conf 配置 rtprio；
    /// 失败时静默降级，不影响音频功能。
    pub fn boost_current_audio_thread(name: &str) {
        let priority = AUDIO_RT_PRIORITY.load(Ordering::Acquire);
        let param = libc::sched_param {
            sched_priority: priority,
        };
        let ret =
            unsafe { libc::pthread_setschedparam(libc::pthread_self(), libc::SCHED_FIFO, &param) };
        if ret == 0 {
            debug!(thread = name, priority, "SCHED_FIFO 实时调度已启用");
        } else {
            warn!(
                thread = name,
                errno = ret,
                "SCHED_FIFO 设置失败（需要 CAP_SYS_NICE 或 limits.conf 配置 rtprio≥{priority}），已降级为标准调度"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse_cpulist;

    #[test]
    fn cpulist_parses_kernel_isolated_format() {
        assert_eq!(parse_cpulist("2-3"), vec![2, 3]);
        assert_eq!(parse_cpulist("2,5,8-9"), vec![2, 5, 8, 9]);
        assert_eq!(parse_cpulist(""), Vec::<u32>::new());
    }
}

// ── 其他平台（macOS 等）──────────────────────────────────────────────────────
#[cfg(not(any(target_os = "windows", target_os = "linux")))]
mod imp {
    pub fn boost_current_audio_thread(_name: &str) {}

    pub fn bind_current_thread_to_performance_cores(_name: &str) {}
}

pub use imp::boost_current_audio_thread;

/// 将当前线程绑定到 CPU 性能核心（ARM 大核 / x86 独立物理核）。
///
/// 应在以下音频关键线程启动时立即调用：
/// - Diretta 数据推流线程（`diretta-direct-dsd`、`diretta-direct-pcm`）
/// - 主解码 / DSP 线程
///
/// 在非 Linux 平台上为空操作（no-op）。
pub use imp::bind_current_thread_to_performance_cores;
