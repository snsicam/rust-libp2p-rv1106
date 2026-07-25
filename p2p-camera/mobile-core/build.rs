fn main() {
    let build_time = chrono_now();
    let out_dir = std::env::var("OUT_DIR").unwrap();
    let stamp_path = std::path::Path::new(&out_dir).join("build_stamp");
    std::fs::write(&stamp_path, &build_time).unwrap();
    println!("cargo:rerun-if-changed={}", stamp_path.display());
    println!("cargo:rustc-env=BUILD_TIME={build_time}");

    #[cfg(target_os = "windows")]
    windows_setup();
}

#[cfg(target_os = "windows")]
fn windows_setup() {
    if std::env::var("LIBCLANG_PATH").is_err() {
        let vcpkg_root = std::env::var("VCPKG_ROOT").ok();
        let candidates: Vec<std::path::PathBuf> = {
            let mut paths = Vec::new();
            if let Some(ref root) = vcpkg_root {
                paths.push(std::path::PathBuf::from(root)
                    .join("installed").join("x64-windows")
                    .join("tools").join("llvm").join("bin"));
            }
            paths.push(std::path::PathBuf::from(r"C:\Program Files\LLVM\bin"));
            paths
        };
        let found = candidates.iter().find(|p| p.join("libclang.dll").exists());
        if let Some(path) = found {
            println!("cargo:rustc-env=LIBCLANG_PATH={}", path.display());
            println!("cargo:warning=[INFO] Auto-detected LIBCLANG_PATH={}", path.display());
        } else {
            println!("cargo:warning=[WARN] LIBCLANG_PATH not set, bindgen may fail. Install LLVM or set LIBCLANG_PATH manually.");
        }
    }

    if std::env::var("VCPKGRS_DYNAMIC").is_err() {
        println!("cargo:warning=[INFO] Set VCPKGRS_DYNAMIC=1 for dynamic linking with vcpkg ffmpeg/SDL2");
    }
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
