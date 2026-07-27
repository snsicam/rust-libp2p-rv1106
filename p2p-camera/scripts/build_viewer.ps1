# build_viewer.ps1 — Windows 平台编译 media_viewer (with SDL player)
#
# 用法:
#   .\build_viewer.ps1                  # Debug 构建
#   .\build_viewer.ps1 -Release         # Release 构建
#   .\build_viewer.ps1 -Target aarch64-pc-windows-msvc  # ARM64 交叉编译
#
# 前置条件 (Windows):
#   - Visual Studio Build Tools 2022 (含 C++ 桌面开发工作负载)
#   - vcpkg (若未安装则提示安装步骤)
#   - vcpkg install ffmpeg:x64-windows sdl2:x64-windows

param(
    [switch]$Release,
    [string]$Target = "x86_64-pc-windows-msvc",
    [string]$VcpkgRoot
)

$ErrorActionPreference = "Stop"

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
# p2p-camera 是独立的 Cargo workspace（父级 win-rust-libp2p 是另一个 workspace，不包含 mobile-core）
# 必须从 p2p-camera/ 目录执行构建
$ProjectRoot = Resolve-Path (Join-Path $ScriptDir "..")

# ---- 检测 MSVC 工具链 ----
function Test-MSVC {
    $vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
    if (Test-Path $vswhere) {
        $installPath = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath 2>$null
        if ($installPath) {
            Write-Host "[INFO] Found Visual Studio at: $installPath"
            return $true
        }
    }
    $cl = Get-Command cl.exe -ErrorAction SilentlyContinue
    if ($cl) {
        Write-Host "[INFO] Found cl.exe in PATH"
        return $true
    }
    return $false
}

if (-not (Test-MSVC)) {
    Write-Host "[ERROR] MSVC toolchain not found. Install Visual Studio Build Tools from https://visualstudio.microsoft.com/visual-cpp-build-tools/" -ForegroundColor Red
    exit 1
}

# ---- 检测 vcpkg ----
if (-not $VcpkgRoot) {
    $VcpkgRoot = $env:VCPKG_ROOT
}
if (-not $VcpkgRoot) {
    $candidates = @(
        "E:\vcpkg",
        "C:\vcpkg",
        (Join-Path $ProjectRoot "vcpkg"),
        (Join-Path $env:USERPROFILE "vcpkg")
    )
    foreach ($c in $candidates) {
        if (Test-Path (Join-Path $c "vcpkg.exe")) {
            $VcpkgRoot = $c
            break
        }
    }
}
if (-not $VcpkgRoot -or -not (Test-Path (Join-Path $VcpkgRoot "vcpkg.exe"))) {
    Write-Host "[ERROR] vcpkg not found. Install with:" -ForegroundColor Red
    Write-Host "  git clone https://github.com/microsoft/vcpkg"
    Write-Host "  .\vcpkg\bootstrap-vcpkg.bat"
    Write-Host "  Then set -VcpkgRoot or VCPKG_ROOT environment variable."
    exit 2
}
Write-Host "[INFO] Using vcpkg at: $VcpkgRoot"

# ---- 检测/安装 vcpkg 依赖 ----
$archSuffix = if ($Target -match "aarch64") { "arm64-windows" } else { "x64-windows" }
$ffmpegInstalled = Test-Path (Join-Path $VcpkgRoot "installed\$archSuffix\include\libavcodec\avcodec.h")
$sdl2Installed = Test-Path (Join-Path $VcpkgRoot "installed\$archSuffix\include\SDL2\SDL.h")

if (-not $ffmpegInstalled -or -not $sdl2Installed) {
    # 检查 vcpkg 是否正在后台安装（lock 文件存在）
    $lockFile = Join-Path $VcpkgRoot "installed\vcpkg\vcpkg-running.lock"
    if (Test-Path $lockFile) {
        Write-Host "[ERROR] vcpkg is currently installing (lock file detected)." -ForegroundColor Red
        Write-Host "  $lockFile" -ForegroundColor Yellow
        Write-Host "  Wait for the background install to complete, then re-run this script." -ForegroundColor Yellow
        exit 3
    }

    Write-Host "[INFO] Installing vcpkg dependencies (ffmpeg:$archSuffix sdl2:$archSuffix)..."
    & (Join-Path $VcpkgRoot "vcpkg.exe") install "ffmpeg:$archSuffix" "sdl2:$archSuffix"
    if ($LASTEXITCODE -ne 0) {
        Write-Host "[ERROR] vcpkg install failed." -ForegroundColor Red
        exit 4
    }
    Write-Host "[INFO] vcpkg dependencies installed."
} else {
    Write-Host "[INFO] vcpkg dependencies (ffmpeg, sdl2) already installed for $archSuffix."
}

# ---- 设置 LIBCLANG_PATH ----
if (-not $env:LIBCLANG_PATH) {
    $llvmCandidates = @(
        (Join-Path $VcpkgRoot "installed\$archSuffix\bin"),          # vcpkg llvm: libclang.dll 在 bin/ 下
        (Join-Path $VcpkgRoot "installed\$archSuffix\tools\llvm\bin"),
        "C:\Program Files\LLVM\bin"
    )
    foreach ($c in $llvmCandidates) {
        if (Test-Path (Join-Path $c "libclang.dll")) {
            $env:LIBCLANG_PATH = $c
            Write-Host "[INFO] Setting LIBCLANG_PATH=$c"
            break
        }
    }
    if (-not $env:LIBCLANG_PATH) {
        Write-Host "[WARN] LIBCLANG_PATH not set, bindgen may fail." -ForegroundColor Yellow
    }
}

# ---- 设置 VCPKGRS_DYNAMIC ----
$env:VCPKGRS_DYNAMIC = "1"
Write-Host "[INFO] Setting VCPKGRS_DYNAMIC=1"

# ---- 执行 cargo build ----
$cargoArgs = @("build", "--example", "media_viewer", "-p", "mobile-core", "--features", "player")
if ($Release) {
    $cargoArgs += "--release"
}
# 仅交叉编译时传 --target（host target 不传，避免产物放到 target/<triple>/ 子目录）
$isCrossTarget = $Target -and $Target -ne "x86_64-pc-windows-msvc"
if ($isCrossTarget) {
    $cargoArgs += @("--target", $Target)
}

Write-Host "[INFO] Building media_viewer (with SDL player, $(if ($Release) { 'release' } else { 'debug' }))..."
Write-Host "[INFO] cargo $($cargoArgs -join ' ')"

Push-Location $ProjectRoot
try {
    & cargo @cargoArgs
    if ($LASTEXITCODE -ne 0) {
        Write-Host "[ERROR] Build failed. See errors above." -ForegroundColor Red
        exit 5
    }
} finally {
    Pop-Location
}

$buildDir = if ($Release) { "release" } else { "debug" }
if ($isCrossTarget) {
    $viewerExe = Join-Path $ProjectRoot "target\$Target\$buildDir\examples\media_viewer.exe"
} else {
    $viewerExe = Join-Path $ProjectRoot "target\$buildDir\examples\media_viewer.exe"
}

if (Test-Path $viewerExe) {
    Write-Host "[INFO] Build SUCCESS -> $viewerExe" -ForegroundColor Green
} else {
    Write-Host "[WARN] media_viewer.exe not found at expected path: $viewerExe" -ForegroundColor Yellow
}
