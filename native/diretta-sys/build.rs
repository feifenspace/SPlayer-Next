use std::env;
use std::path::PathBuf;

fn main() {
    let target_arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();

    println!("cargo:rerun-if-changed=include/diretta_c_api.h");
    println!("cargo:rerun-if-changed=src/diretta_c_shim.cpp");
    println!("cargo:rerun-if-env-changed=DIRETTA_ARCH");
    println!("cargo:rerun-if-env-changed=DIRETTA_SDK_DIR");
    println!("cargo:rerun-if-env-changed=DIRETTA_USE_SDK_LOG");

    // 定位 SDK 根目录（优先 150，其次 149 / 148）
    let sdk_dir = env::var("DIRETTA_SDK_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let p150 = PathBuf::from("/home/songlian/DirettaHostSDK_150");
            let p149 = PathBuf::from("/home/songlian/DirettaHostSDK_149");
            let p148 = PathBuf::from("/home/songlian/DirettaHostSDK_148");
            if p150.exists() {
                p150
            } else if p149.exists() {
                p149
            } else {
                p148
            }
        });

    let sdk_include = sdk_dir.join("Host");
    let sdk_lib = sdk_dir.join("lib");

    if !sdk_dir.exists() {
        panic!(
            "DirettaHostSDK not found at {}. Please set DIRETTA_SDK_DIR environment variable.",
            sdk_dir.display()
        );
    }

    // 动态探测 SDK Release 版本
    let release_header = sdk_include.join("Release.hpp");
    let release_no = if release_header.exists() {
        std::fs::read_to_string(&release_header)
            .ok()
            .and_then(|content| {
                for line in content.lines() {
                    if line.contains("ReleaseNo") {
                        let parts: Vec<&str> = line.split('=').collect();
                        if parts.len() >= 2 {
                            let val_str = parts[1].trim().trim_end_matches(';').trim();
                            if let Ok(v) = val_str.parse::<u16>() {
                                return Some(v);
                            }
                        }
                    }
                }
                None
            })
            .unwrap_or(150)
    } else {
        150
    };

    let sdk_define = match release_no {
        150 => "DIRETTA_SDK_150",
        149 => "DIRETTA_SDK_149",
        _ => "DIRETTA_SDK_148",
    };

    // 微架构判定
    let diretta_arch = env::var("DIRETTA_ARCH").unwrap_or_else(|_| "auto".to_string());
    let use_sdk_log = env::var("DIRETTA_USE_SDK_LOG")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let (suffix, march) = resolve_arch(&target_arch, &diretta_arch, use_sdk_log);

    println!("cargo:warning=[diretta-sys] Building for arch: target_arch={}, resolved_arch={}, suffix={}, march={}, sdk_release={}, sdk_log={}",
             target_arch, diretta_arch, suffix, march, release_no, if use_sdk_log { "on" } else { "off" });

    // 编译 C++ shim
    let mut build = cc::Build::new();
    build
        .cpp(true)
        .std("c++17")
        .define(sdk_define, None)
        .file("src/diretta_c_shim.cpp")
        .include("include")
        .include(&sdk_include)
        .flag("-fPIC")
        .flag("-O3")
        .flag(&format!("-march={}", march));

    build.compile("diretta_c_shim");

    // 静态链接 SDK 库
    let host_lib_name = format!("libDirettaHost_{}.a", suffix);
    let acqua_lib_name = format!("libACQUA_{}.a", suffix);

    let host_lib_path = sdk_lib.join(&host_lib_name);
    let acqua_lib_path = sdk_lib.join(&acqua_lib_name);

    if !host_lib_path.exists() {
        panic!("Diretta static lib not found: {}", host_lib_path.display());
    }
    if !acqua_lib_path.exists() {
        panic!("ACQUA static lib not found: {}", acqua_lib_path.display());
    }

    println!("cargo:rustc-link-search=native={}", sdk_lib.display());

    // 链接 Direct SDK 静态库 (去除 "lib" 前缀和 ".a" 后缀)
    let host_link_name = host_lib_name.trim_start_matches("lib").trim_end_matches(".a");
    let acqua_link_name = acqua_lib_name.trim_start_matches("lib").trim_end_matches(".a");

    println!("cargo:rustc-link-lib=static={}", host_link_name);
    println!("cargo:rustc-link-lib=static={}", acqua_link_name);

    // 链接系统依赖
    if target_os == "linux" {
        println!("cargo:rustc-link-lib=stdc++");
        println!("cargo:rustc-link-lib=pthread");
        println!("cargo:rustc-link-lib=rt");
        println!("cargo:rustc-link-lib=m");
    }
}

fn resolve_arch(target_arch: &str, requested_arch: &str, use_sdk_log: bool) -> (String, String) {
    // 后缀：带日志版本去掉 "-nolog"，启用 SDK 内部 SysLog
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
