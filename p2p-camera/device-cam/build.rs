// RV1106 SDK 构建脚本
// 仅在 --features rv1106 时生效

fn main() {
    // 检测是否启用 rv1106 feature
    let rv1106 = std::env::var("CARGO_FEATURE_RV1106").is_ok();

    if !rv1106 {
        return;
    }

    println!("cargo:rerun-if-changed=src/rk_camera.c");

    // SDK 头文件路径 — rockit MPI 头文件
    let sdk_include = std::env::var("RV1106_SDK_INCLUDE")
        .unwrap_or_else(|_| "/usr/include".to_string());

    // rkaiq ISP 头文件路径 — rkaiq include 目录结构分散 (uAPI2/common/xcore/algos/...)
    // 直接递归添加 include/rkaiq 下所有子目录
    let rkaiq_include = std::env::var("RV1106_RKAIQ_INCLUDE")
        .unwrap_or_else(|_| {
            let sdk_lib = std::env::var("RV1106_SDK_LIB").unwrap_or_default();
            for lib_dir in sdk_lib.split(':') {
                let lib_dir = lib_dir.trim();
                if lib_dir.contains("rkaiq") {
                    let parent = std::path::Path::new(lib_dir)
                        .parent()
                        .unwrap();
                    let include_root = parent.join("include");
                    // 递归收集 include 下所有目录
                    let mut paths = vec![include_root.display().to_string()];
                    if let Ok(entries) = std::fs::read_dir(&include_root) {
                        for entry in entries.flatten() {
                            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                                let p = entry.path();
                                paths.push(p.display().to_string());
                                // 再递归一层 (rkaiq/algos/adebayer 等)
                                if let Ok(sub_entries) = std::fs::read_dir(&p) {
                                    for sub in sub_entries.flatten() {
                                        if sub.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                                            paths.push(sub.path().display().to_string());
                                        }
                                    }
                                }
                            }
                        }
                    }
                    return paths.join(":");
                }
            }
            "/usr/include".to_string()
        });

    // 编译 C shim 为静态库 (SDK .so 在运行时动态链接)
    let mut cc_build = cc::Build::new();
    cc_build
        .file("src/rk_camera.c")
        .include(&sdk_include);

    // 添加 rkaiq 头文件路径 (可能是冒号分隔的多路径)
    for inc_dir in rkaiq_include.split(':') {
        let inc_dir = inc_dir.trim();
        if !inc_dir.is_empty() {
            cc_build.include(inc_dir);
        }
    }

    cc_build.compile("rk_camera");

    // SDK 库路径 — 支持多个路径 (用冒号分隔)
    let sdk_lib_paths = std::env::var("RV1106_SDK_LIB")
        .unwrap_or_else(|_| "/usr/lib".to_string());

    for lib_dir in sdk_lib_paths.split(':') {
        let lib_dir = lib_dir.trim();
        if !lib_dir.is_empty() {
            println!("cargo:rustc-link-search=native={}", lib_dir);
        }
    }

    // 链接 SDK 库 (动态链接, 运行时需要 .so 在 RV1106 上)
    // 用 -Wl,--allow-shlib-undefined 忽略 .so 内部的未解析符号
    println!("cargo:rustc-link-arg=-Wl,--allow-shlib-undefined");
    println!("cargo:rustc-link-lib=dylib=rockit_full");
    println!("cargo:rustc-link-lib=dylib=rkaiq");
}
