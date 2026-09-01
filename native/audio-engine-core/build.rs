fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let csrc_dir = std::path::Path::new(&manifest_dir).join("csrc");
    let libdstdec_dir = csrc_dir.join("libdstdec");
    let libcommon_dir = csrc_dir.join("libcommon");
    let sacdstub_dir = csrc_dir.join("sacdstub");

    let dst_files = [
        "buffer_pool.c",
        "ccp_calc.c",
        "dst_ac.c",
        "dst_data.c",
        "dst_decoder.c",
        "dst_fram.c",
        "dst_init.c",
        "unpack_dst.c",
        "yarn.c",
    ];

    let mut build = cc::Build::new();
    for f in &dst_files {
        let path = libdstdec_dir.join(f);
        build.file(&path);
        println!("cargo:rerun-if-changed={}", path.to_string_lossy());
    }

    let stub_path = sacdstub_dir.join("logging_stub.c");
    build.file(&stub_path);
    println!("cargo:rerun-if-changed={}", stub_path.to_string_lossy());

    build
        .include(&libdstdec_dir)
        .include(&libcommon_dir)
        .include(&sacdstub_dir)
        .std("c11")
        .opt_level(2)
        .flag_if_supported("-pthread")
        .flag_if_supported("-D_GNU_SOURCE")
        .flag_if_supported("-Wno-unused-parameter")
        .flag_if_supported("-Wno-unused-variable")
        .flag_if_supported("-Wno-unused-but-set-variable")
        .flag_if_supported("-Wno-sign-compare")
        .flag_if_supported("-Wno-noreturn")
        .warnings(false)
        .compile("dstdec");

    println!("cargo:rustc-link-lib=dylib=pthread");
    println!("cargo:rustc-cfg=has_libdstdec");
    println!("cargo:rerun-if-changed=build.rs");
}
