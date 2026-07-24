// RV1106 SDK 构建脚本
// 仅在 --features rv1106 时生效

fn main() {
    // 编译时间 (所有构建模式都需要)
    // 通过写入时间戳文件并声明依赖，确保每次编译都更新 BUILD_TIME
    let build_time = chrono_now();
    let out_dir = std::env::var("OUT_DIR").unwrap();
    let stamp_path = std::path::Path::new(&out_dir).join("build_stamp");
    std::fs::write(&stamp_path, &build_time).unwrap();
    println!("cargo:rerun-if-changed={}", stamp_path.display());
    println!("cargo:rustc-env=BUILD_TIME={build_time}");

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
        .file("src/lcd_preview.c")   // LCD 预览模块 (从 rk_camera.c 抽取)
        .file("src/bbox_shm.c")     // cam→LVGL bbox 共享内存环形队列
        .file("src/rknn_infer.c")    // rknn YOLOv5 推理 (复用 LCD selfpath 通道)
        .include(&sdk_include);

    // RKNN 头文件 (rknn_api.h) — 不在 SDK 默认 include/rknn 下,
    // 而在 rknn 例子目录里。自动探测候选路径, 找不到再用 env 覆盖提示。
    let sdk_root_rknn_inc = std::env::var("RV1106_SDK_ROOT")
        .map(|r| format!("{}/media/rockit/rockit/mpi/sdk/include/rknn", r))
        .unwrap_or_default();
    let rknn_inc_candidates = vec![
        std::env::var("RV1106_RKNN_INCLUDE").unwrap_or_default(), // 手动覆盖优先
        format!("{}/rknn", sdk_include),
        sdk_root_rknn_inc,
        // rknn 例子 (用户确认可参考其代码):
        "/home/song/samba/work/rv1106/rknn/luckfox_pico_rknn_example/include/rknn".to_string(),
        "/home/song/samba/work/rv1106".to_string(),   // 递归兜底
    ];
    let rknn_include = find_dir_containing(&rknn_inc_candidates, "rknn_api.h", 6)
        .unwrap_or_else(|| {
            println!("cargo:warning=RKNN include NOT found. Set RV1106_RKNN_INCLUDE to dir containing rknn_api.h");
            format!("{}/rknn", sdk_include)
        });
    println!("cargo:warning=RKNN include path: {}", rknn_include);
    cc_build.include(&rknn_include);

    // 添加 rkaiq 头文件路径 (可能是冒号分隔的多路径)
    for inc_dir in rkaiq_include.split(':') {
        let inc_dir = inc_dir.trim();
        if !inc_dir.is_empty() {
            cc_build.include(inc_dir);
        }
    }

    // RGA (2D 加速器) — NV12→BGRA 硬件转换, 静态链接避免运行时依赖
    let rga_include = std::env::var("RV1106_RGA_INCLUDE")
        .unwrap_or_else(|_| {
            let sdk_root = std::env::var("RV1106_SDK_ROOT")
                .unwrap_or_else(|_| "/workspace/rv1106/RV1106_Linux_SDK".to_string());
            let inc = std::path::Path::new(&sdk_root)
                .join("media/rga/release_rga_rv1106_arm-rockchip830-linux-uclibcgnueabihf/include/rga");
            println!("cargo:warning=RGA auto-detect: checking {}", inc.display());
            if inc.exists() {
                println!("cargo:warning=RGA include found: {}", inc.display());
                return inc.display().to_string();
            }
            "/usr/include/rga".to_string()
        });
    let rga_lib = std::env::var("RV1106_RGA_LIB")
        .unwrap_or_else(|_| {
            let sdk_root = std::env::var("RV1106_SDK_ROOT")
                .unwrap_or_else(|_| "/workspace/rv1106/RV1106_Linux_SDK".to_string());
            let lib = std::path::Path::new(&sdk_root)
                .join("media/rga/release_rga_rv1106_arm-rockchip830-linux-uclibcgnueabihf/lib");
            if lib.exists() {
                return lib.display().to_string();
            }
            "/usr/lib".to_string()
        });
    println!("cargo:warning=RGA include path: {}", rga_include);
    println!("cargo:warning=RGA lib path: {}", rga_lib);

    cc_build.include(&rga_include);

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
    println!("cargo:rustc-link-search=native={}", rga_lib);
    println!("cargo:rustc-link-lib=static=rga");
    println!("cargo:rustc-link-lib=static=stdc++");  // RGA 是 C++ 库, 需要 libstdc++

    // RKNN 推理库 (librknnmrt.so) — 不在 SDK 默认 lib 下, 而在 rknn 例子 lib/glibc。
    // 自动探测候选路径 (优先 glibc 版本, 匹配 gnueabihf target)。
    let rknn_lib_candidates = vec![
        std::env::var("RV1106_RKNN_LIB").unwrap_or_default(), // 手动覆盖优先
        format!("{}/../lib/glibc", sdk_lib_paths),
        "/home/song/samba/work/rv1106/rknn/luckfox_pico_rknn_example/lib/glibc".to_string(),
        "/home/song/samba/work/rv1106".to_string(),   // 递归兜底
    ];
    let rknn_lib = find_dir_containing(&rknn_lib_candidates, "librknnmrt.so", 6)
        .unwrap_or_else(|| {
            println!("cargo:warning=RKNN lib NOT found. Set RV1106_RKNN_LIB to dir containing librknnmrt.so");
            format!("{}/../lib/glibc", sdk_lib_paths)
        });
    println!("cargo:rustc-link-search=native={}", rknn_lib);
    println!("cargo:rustc-link-lib=dylib=rknnmrt");
}

// 在候选根目录下查找某个文件, 返回其所在目录 (含直接命中与递归子目录)。
// 用于自动探测 rknn_api.h / librknnmrt.so 的位置 (它们不在 SDK 默认 include/lib 下,
// 而在 rknn 例子目录里)。
fn find_dir_containing(roots: &[String], filename: &str, max_depth: u32) -> Option<String> {
    for root in roots {
        if root.is_empty() { continue; }
        let root = root.trim_end_matches('/');
        // 直接命中: root 本身就是包含该文件的目录
        if std::path::Path::new(root).join(filename).exists() {
            return Some(root.to_string());
        }
        // 递归子目录查找
        if let Some(found) = walk_find(std::path::Path::new(root), filename, max_depth) {
            return Some(found);
        }
    }
    None
}

fn walk_find(dir: &std::path::Path, filename: &str, depth: u32) -> Option<String> {
    if depth == 0 { return None; }
    let entries = std::fs::read_dir(dir).ok()?;
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            if let Some(f) = walk_find(&p, filename, depth - 1) {
                return Some(f);
            }
        } else if p.file_name().map(|n| n == filename).unwrap_or(false) {
            return p.parent().map(|pp| pp.display().to_string());
        }
    }
    None
}

fn chrono_now() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // 北京时间 UTC+8
    let now_local = now + 8 * 3600;
    let days = now_local / 86400;
    let time_of_day = now_local % 86400;
    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;
    let seconds = time_of_day % 60;
    let (year, month, day) = days_to_ymd(days);
    format!("{year:04}-{month:02}-{day:02} {hours:02}:{minutes:02}:{seconds:02} CST")
}

fn days_to_ymd(mut days: u64) -> (u64, u64, u64) {
    let mut year = 1970;
    loop {
        let days_in_year = if is_leap(year) { 366 } else { 365 };
        if days < days_in_year { break; }
        days -= days_in_year;
        year += 1;
    }
    let leap = is_leap(year);
    let month_days = [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut month = 1;
    for &md in &month_days {
        if days < md { break; }
        days -= md;
        month += 1;
    }
    (year, month, days + 1)
}

fn is_leap(year: u64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}
