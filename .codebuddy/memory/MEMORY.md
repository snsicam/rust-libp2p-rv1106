# 长期记忆 (MEMORY.md)

## 项目：win-rust-libp2p / p2p-camera
- Windows 开发机（用户 `song`，主机 `DESKTOP-867VBCJ`）构建 p2p-camera。
- Viewer 编译需要：Rust 工具链 + VS Build Tools 2022 (C++ 桌面) + vcpkg(`ffmpeg`/`sdl2`/`llvm` for x64-windows) + `libclang`。
- 一键脚本：`p2p-camera/scripts/setup_env.ps1`（强制管理员，装 Rust/VS/vcpkg/LLVM 并写 `VCPKG_ROOT`/`LIBCLANG_PATH`/`VCPKGRS_DYNAMIC`）。
- **llvm 在本项目只用于提供 `libclang`**：`mobile-core/build.rs` 用 `bindgen` 在编译期解析 ffmpeg/sdl2 的 C 头、生成 Rust FFI 绑定（ffmpeg-next 6.x / sdl2 0.37 是 C 库的 Rust 封装，需绑定才能调用）。`LIBCLANG_PATH` 即指向 LLVM 的 libclang.dll。`VCPKGRS_DYNAMIC=1` 让 vcpkg-rs 动态链接。libclang 仅服务编译期，运行时 viewer 只链 ffmpeg/sdl2 的 dll，不依赖 llvm。vcpkg 的 llvm 端口默认编全套（clang/lld/mlir/全 target），实际只用 libclang 一个库，所以编译极慢（2–3h）。

## 网络（国内）：默认源不稳定，务必用镜像
- rustup / cargo：用 `rsproxy.cn` 镜像。cargo 配置在 `C:\Users\song\.cargo\config.toml`
  （`[source.crates-io] replace-with = "rsproxy-sparse"`，`registry = "sparse+https://rsproxy.cn/index/"`，`[net] git-fetch-with-cli = true`）。
- rustup-init 下载：`https://rsproxy.cn/rustup/dist/x86_64-pc-windows-msvc/rustup-init.exe`（设 `RUSTUP_DIST_SERVER=https://rsproxy.cn`、`RUSTUP_UPDATE_ROOT=https://rsproxy.cn/rustup`）。
- vcpkg 克隆：用 gitee 镜像 `https://gitee.com/mirrors/vcpkg`，避免 github 失败。

## vcpkg on Windows 编译坑（关键）
- vcpkg 的 `atl` 端口用 `find_path(... PATHS $ENV{INCLUDE})` 找 `atlbase.h`。VS Build Tools 默认**不装 ATL**，需额外用 VS 安装器补 `Microsoft.VisualStudio.Component.VC.ATL`（ATL 头文件装在 `VC\Tools\MSVC\<ver>\atlmfc\include\`，不是 `include\`）。这是 ffmpeg 的间接依赖，缺失会直接 `BUILD_FAILED`。
- 补装 ATL 后，vcpkg 仍可能报 `Unable to locate 'atlbase.h'`：**必须在 `vcvars64.bat` 环境下运行 vcpkg**（`vcvars64` 才会把 `atlmfc\include` 加进 `INCLUDE` 环境变量）。普通 shell 的 `INCLUDE` 为空 → 失败。
- 注意：`vcvars64.bat` 会把 `VCPKG_ROOT` 改成 VS 自带的 `...\VC\vcpkg`，与我们的 `E:\vcpkg` 冲突（vcpkg 会忽略并用自身目录，仅警告，无害）。脚本里最后用 `setx VCPKG_ROOT E:\vcpkg` 覆盖回去即可。
- vcpkg install 会 re-exec 自身：父进程很快返回（打印 DONE），但子 vcpkg/cmake 进程在后台继续编译并持有 `vcpkg-running.lock`，属正常现象，勿重复启动（会报 lock busy）。
- 机器上会有 **3 个 vcpkg 目录，别混**：① `E:\vcpkg` = 真正在用（装包、编译中，受 `VCPKG_ROOT` 用户变量指向）；② `C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\vcpkg` = VS 2022 自带 bundled 系统组件（vcvars 会把 `VCPKG_ROOT` 临时改成它），**勿删**；③ `C:\vcpkg` = 早期克隆的**冗余源码副本**（仅 19MB、有 `.git`、无 installed/buildtrees/packages、未被任何环境变量/脚本引用），可安全删除，留着也无害。

## CodeBuddy 执行器注意事项（重要）
- `execute_command` 对“可能耗时久”的命令会**自动跳过**（如 bootstrap、长编译），需要改用**脱离进程**后台跑。
- ⚠️ 长时构建（如 vcpkg 编译 ffmpeg，30–60 分钟）的可靠方式：**`schtasks /Create /TN xxx /TR "路径\xxx.bat" /SC ONCE /ST 23:59 /IT /F` 然后 `schtasks /Run /TN xxx`**。
  - 必须加 **`/IT`（交互式）**：否则任务在 headless 会话里运行，vcpkg/ffmpeg 等需要控制台的构建会立即死掉（拿不到 tty/console）。
  - 交互式任务运行在**独立会话**，工具后续的 `execute_command` 诊断**不会**把它杀掉。
  - 反例：`cmd /c start "" "xxx.bat"` 看似脱离，但每次发新的 `execute_command` 时工具会回收上一条命令的进程树，把正在编译的子进程连带杀死（本次 ffmpeg 屡次在查状态时死掉就是这个原因）。
- `call vcvars64.bat >> %LOG%` 会把批处理后续 `echo >> %LOG%` 的句柄弄乱、导致后续回显丢失；vcvars 的输出应重定向到 `>nul 2>&1`，构建命令再单独 `>> %LOG% 2>&1`。
- 管理员提权 `Start-Process -Verb RunAs` 的 UAC 弹窗在本工具里不可靠（可能不弹/超时），尽量把需要写系统盘(C:\)的操作改到用户可写的非系统盘（如 `E:\vcpkg`），普通权限即可，无需 UAC。

## 当前环境状态（2026-07-25 验证通过）
- ✅ Rust 1.97.1（用户目录，已配 rsproxy 镜像）
- ✅ VS Build Tools 2022（MSVC / cl.exe）
- ✅ vcpkg 装在 `E:\vcpkg`，ffmpeg (8.1.2, libavcodec 62) / sdl2 / llvm 已安装完成
- ✅ **Windows viewer 编译验证通过**（`media_viewer.exe` 20.9MB）
- ✅ `LIBCLANG_PATH` 已修正为 `E:\vcpkg\installed\x64-windows\bin`（libclang.dll 在此目录，不在 `tools\llvm\bin`）
- ✅ `build_viewer.ps1` 已修正 LIBCLANG_PATH 候选路径（优先查 `installed\<triplet>\bin`）
- ✅ `run_media_viewer.ps1` 已修正：自动添加 vcpkg DLL 目录到 PATH、自动查找 `viewer.toml`（p2p-camera/ → repo root）、移除旧 `--config` 处理避免重复
- ✅ 已创建占位 `E:\vcpkg\installed\x64-windows\include\libavcodec\avfft.h`（FFmpeg 8.0 移除此头文件，但 ffmpeg-sys-next 仍引用它；占位为空文件即可让 bindgen 通过）
- 辅助脚本：`p2p-camera/scripts/` 下 `install_rust_mirror.ps1`、`write_cargo_config.ps1`、`install_vcpkg.ps1`、`install_vcpkg_deps.ps1`、`run_deps.bat`、`_elevate_setup.ps1`、`_build_final.bat`

## ffmpeg-next 版本匹配关键规则（2026-07-25 验证）
- **vcpkg 默认安装最新 ffmpeg**（当前 FFmpeg 8.1.2, libavcodec 62），**不是** FFmpeg 6.x。
- `ffmpeg-next` crate 版本号跟踪 FFmpeg 版本：`"6"` → FFmpeg 6.x，`"7"` → FFmpeg 7.x，`"8"` → FFmpeg 8.x。
- `mobile-core/Cargo.toml` 中 `ffmpeg-next = { version = "8" }` 匹配当前 vcpkg 安装的 FFmpeg 8.1.2。
- **版本不匹配会导致**：API 常量缺失（如 `AVFMT_ALLOW_FLUSH`、`AV_CODEC_CAP_SUBFRAMES`）、枚举变体不覆盖、大量编译错误。
- 如果 vcpkg 版本变（如 vcpkg 更新 ffmpeg 到 9.x），需同步更新 `ffmpeg-next` 版本号。

## p2p-camera 项目结构与 Workspace（2026-07-24 修正）
- **`p2p-camera/Cargo.toml` 是独立的 Cargo workspace**（members: proto, mobile-core, device-cam, relay-server）。
- 父级 `win-rust-libp2p/Cargo.toml` 是**另一个** workspace（libp2p 官方 crate），**不包含** p2p-camera 的 crate。
- Cargo 不支持嵌套 workspace，所有构建命令**必须从 `p2p-camera/` 目录执行**，不能从 `win-rust-libp2p/` 根执行。
- Viewer 不独立存在：是 `mobile-core` crate 的 example（`mobile-core/examples/media_viewer.rs`），编译命令 `cargo build --example media_viewer -p mobile-core --features player`。
- path 依赖从 `p2p-camera/` 向上引用父 workspace 源码：`../libp2p`、`../protocols/stream`、`../swarm`。

## 脚本潜在问题与修复（2026-07-24）
- **`build_viewer.ps1` / `run_media_viewer.ps1` / `package_viewer.ps1` 的 `$ProjectRoot`** 原为 `..\..`（指向 `win-rust-libp2p/`），已修正为 `..`（指向 `p2p-camera/`），否则 cargo 找不到 mobile-core。
- **`build_viewer.ps1`** 已新增：vcpkg lock 检查（防止后台安装冲突）、`E:\vcpkg` 候选路径、退出码修正、交叉编译 `--target` 仅在非 host 时传递。
- **`package_viewer.ps1`** 已修正：vcpkg 候选路径、DLL 使用通配符（`avcodec-*.dll` 等）自动匹配版本，不再硬编码 soname。
