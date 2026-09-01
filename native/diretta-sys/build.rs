use std::env;
use std::path::PathBuf;

fn main() {
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();

    println!("cargo:rustc-check-cfg=cfg(diretta_sdk_enabled)");
    println!("cargo:rerun-if-changed=include/diretta_bridge.h");
    println!("cargo:rerun-if-changed=src/bridge.cpp");
    println!("cargo:rerun-if-env-changed=DIRETTA_ARCH");
    println!("cargo:rerun-if-env-changed=DIRETTA_SDK_DIR");
    println!("cargo:rerun-if-env-changed=DIRETTA_SDK_ROOT");
    println!("cargo:rerun-if-env-changed=DIRETTA_USE_SDK_LOG");

    // 定位 SDK 根目录（优先环境变量，其次 150/149/148）
    let sdk_dir_opt = env::var("DIRETTA_SDK_DIR")
        .or_else(|_| env::var("DIRETTA_SDK_ROOT"))
        .map(PathBuf::from)
        .ok()
        .or_else(|| {
            let p150 = PathBuf::from("/home/songlian/DirettaHostSDK_150");
            let p149 = PathBuf::from("/home/songlian/DirettaHostSDK_149");
            let p148 = PathBuf::from("/home/songlian/DirettaHostSDK_148");
            if p150.exists() {
                Some(p150)
            } else if p149.exists() {
                Some(p149)
            } else if p148.exists() {
                Some(p148)
            } else {
                None
            }
        });

    let Some(sdk_dir) = sdk_dir_opt else {
        println!("cargo:warning=[diretta-sys] DirettaHostSDK not found, building stub mode.");
        return;
    };

    let sdk_include = sdk_dir.join("Host");
    let sdk_lib = sdk_dir.join("lib");

    if !sdk_include.is_dir() || !sdk_lib.is_dir() {
        println!(
            "cargo:warning=[diretta-sys] DirettaHostSDK Host/ or lib/ missing at {}, skipping native link",
            sdk_dir.display()
        );
        return;
    }

    // 微架构判定
    let diretta_arch = env::var("DIRETTA_ARCH").unwrap_or_else(|_| "auto".to_string());
    let use_sdk_log = env::var("DIRETTA_USE_SDK_LOG")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let (suffix, march) = resolve_arch(&target_arch, &diretta_arch, use_sdk_log);

    println!(
        "cargo:warning=[diretta-sys] Building for arch: target_arch={}, resolved_arch={}, suffix={}, march={}, sdk_log={}",
        target_arch, diretta_arch, suffix, march, if use_sdk_log { "on" } else { "off" }
    );

    // 编译 C++ 桥接
    let mut build = cc::Build::new();
    build
        .cpp(true)
        .std("c++20")
        .file("src/bridge.cpp")
        .include("include")
        .include(&sdk_include)
        .flag_if_supported("-fPIC")
        .flag_if_supported("-O3")
        .flag_if_supported(&format!("-march={}", march));

    build.compile("splayer_diretta_bridge");

    // 静态链接 SDK 库
    let host_lib_name = format!("libDirettaHost_{}.a", suffix);
    let acqua_lib_name = format!("libACQUA_{}.a", suffix);

    let host_lib_path = sdk_lib.join(&host_lib_name);
    let acqua_lib_path = sdk_lib.join(&acqua_lib_name);

    if !host_lib_path.exists() || !acqua_lib_path.exists() {
        println!(
            "cargo:warning=[diretta-sys] Static libs not found: {} or {}",
            host_lib_path.display(),
            acqua_lib_path.display()
        );
        return;
    }

    println!("cargo:rustc-link-search=native={}", sdk_lib.display());

    let host_link_name = host_lib_name.trim_start_matches("lib").trim_end_matches(".a");
    let acqua_link_name = acqua_lib_name.trim_start_matches("lib").trim_end_matches(".a");

    println!("cargo:rustc-link-lib=static={}", host_link_name);
    println!("cargo:rustc-link-lib=static={}", acqua_link_name);

    // 链接系统依赖
    if target_os == "linux" {
        println!("cargo:rustc-link-lib=dylib=stdc++");
        println!("cargo:rustc-link-lib=dylib=pthread");
        println!("cargo:rustc-link-lib=dylib=dl");
        println!("cargo:rustc-link-lib=dylib=m");
        println!("cargo:rustc-link-lib=dylib=atomic");
    }

    println!("cargo:rustc-cfg=diretta_sdk_enabled");
}

fn resolve_arch(target_arch: &str, requested_arch: &str, use_sdk_log: bool) -> (String, String) {
    let log_suffix = if use_sdk_log { "" } else { "-nolog" };

    if target_arch == "aarch64" || requested_arch == "aarch64" || requested_arch == "arm64" {
        return (
            format!("aarch64-linux-15{}", log_suffix),
            "armv8-a".to_string(),
        );
    }

    match requested_arch {
        "v4" => (format!("x64-linux-15v4{}", log_suffix), "x86-64-v4".to_string()),
        "v3" => (format!("x64-linux-15v3{}", log_suffix), "x86-64-v3".to_string()),
        "zen4" => (format!("x64-linux-15zen4{}", log_suffix), "znver4".to_string()),
        "v2" => (format!("x64-linux-15v2{}", log_suffix), "x86-64-v2".to_string()),
        _ => {
            // 自动检测主机 CPU
            if let Ok(cpuinfo) = std::fs::read_to_string("/proc/cpuinfo") {
                if cpuinfo.contains("avx512") || cpuinfo.contains("avx512f") {
                    (format!("x64-linux-15v4{}", log_suffix), "x86-64-v4".to_string())
                } else if cpuinfo.contains("avx2") {
                    (format!("x64-linux-15v3{}", log_suffix), "x86-64-v3".to_string())
                } else {
                    (format!("x64-linux-15v2{}", log_suffix), "x86-64-v2".to_string())
                }
            } else {
                (format!("x64-linux-15v2{}", log_suffix), "x86-64-v2".to_string())
            }
        }
    }
}
