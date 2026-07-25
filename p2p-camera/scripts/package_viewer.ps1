# package_viewer.ps1 — Windows 平台发布打包脚本
#
# 用法:
#   .\package_viewer.ps1                  # Release 打包
#   .\package_viewer.ps1 -OutputDir out   # 指定输出目录
#
# 产出: viewer-windows-x64.zip (含 media_viewer.exe + DLL + viewer.toml)

param(
    [switch]$Release = $true,
    [string]$OutputDir = "release"
)

$ErrorActionPreference = "Stop"

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
# p2p-camera 是独立的 Cargo workspace，产物在 p2p-camera/target/ 下
$ProjectRoot = Resolve-Path (Join-Path $ScriptDir "..")

$buildConfig = if ($Release) { "release" } else { "debug" }
$viewerExe = Join-Path $ProjectRoot "target\$buildConfig\examples\media_viewer.exe"

# ---- 编译（若产物不存在）----
if (-not (Test-Path $viewerExe)) {
    Write-Host "[INFO] media_viewer.exe not found, building release..."
    Push-Location $ProjectRoot
    try {
        & cargo build --release --example media_viewer -p mobile-core --features player
        if ($LASTEXITCODE -ne 0) {
            Write-Host "[ERROR] Build failed." -ForegroundColor Red
            exit 1
        }
    } finally {
        Pop-Location
    }
}

# ---- 创建发布目录 ----
$publishDir = Join-Path $OutputDir "viewer-windows-x64"
if (Test-Path $publishDir) {
    Remove-Item $publishDir -Recurse -Force
}
New-Item -ItemType Directory -Path $publishDir -Force | Out-Null
Write-Host "[INFO] Created publish directory: $publishDir"

# ---- 复制 media_viewer.exe ----
Copy-Item $viewerExe $publishDir
Write-Host "[INFO] Copied media_viewer.exe"

# ---- 收集依赖 DLL ----
$vcpkgRoot = $env:VCPKG_ROOT
if (-not $vcpkgRoot) {
    $candidates = @(
        "E:\vcpkg",
        "C:\vcpkg",
        (Join-Path $ProjectRoot "vcpkg"),
        (Join-Path $env:USERPROFILE "vcpkg")
    )
    foreach ($c in $candidates) {
        if (Test-Path (Join-Path $c "vcpkg.exe")) {
            $vcpkgRoot = $c
            break
        }
    }
}

$requiredDlls = @(
    "avcodec-*.dll",
    "avformat-*.dll",
    "avutil-*.dll",
    "swscale-*.dll",
    "swresample-*.dll",
    "avfilter-*.dll",
    "SDL2.dll"
)

if ($vcpkgRoot -and (Test-Path $vcpkgRoot)) {
    $dllDir = Join-Path $vcpkgRoot "installed\x64-windows\bin"
    $missingPatterns = [System.Collections.ArrayList]::new()

    foreach ($pattern in $requiredDlls) {
        $matches = Get-ChildItem -Path $dllDir -Filter $pattern -ErrorAction SilentlyContinue
        if ($matches) {
            foreach ($m in $matches) {
                Copy-Item $m.FullName $publishDir
                Write-Host "[INFO] Copied $($m.Name)"
            }
        } else {
            Write-Host "[WARN] $pattern not found in vcpkg, skipping" -ForegroundColor Yellow
            $missingPatterns.Add($pattern) | Out-Null
        }
    }

    # 检查关键 DLL (ffmpeg core + SDL2)
    $criticalPatterns = @("avcodec-*.dll", "avformat-*.dll", "SDL2.dll")
    foreach ($crit in $criticalPatterns) {
        if ($missingPatterns -contains $crit) {
            Write-Host "[ERROR] Required DLL pattern unmatched: $crit" -ForegroundColor Red
            exit 1
        }
    }
} else {
    Write-Host "[WARN] vcpkg not found, skipping DLL collection. Set VCPKG_ROOT to include DLLs." -ForegroundColor Yellow
}

# ---- 生成默认 viewer.toml ----
$tomlContent = @"
# P2P Camera Viewer Configuration
# Edit this file and restart the viewer to apply changes.

[viewer]
# Relay server multiaddress (required)
# relay = "/ip4/YOUR_RELAY_IP/udp/4001/quic-v1/p2p/YOUR_RELAY_PEER_ID"

# DeviceCam peer ID (required)
# camera = "YOUR_DEVICE_CAM_PEER_ID"

# Stream type: "main" (high quality) or "sub" (low quality)
# stream = "sub"

# Enable mDNS LAN discovery (default: true)
# enable_mdns = true

# UDP port for DCUtR hole punching (0 = random)
# udp_port = 0
"@

$tomlPath = Join-Path $publishDir "viewer.toml"
Set-Content -Path $tomlPath -Value $tomlContent -Encoding UTF8
Write-Host "[INFO] Generated viewer.toml"

# ---- 生成 README.txt ----
$readmeContent = @"
P2P Camera Viewer (Windows)
============================

Running:
  1. Edit viewer.toml with your Relay address and DeviceCam peer ID
  2. Double-click media_viewer.exe or run from command line:
     media_viewer.exe --relay <addr> --camera <peer_id> --play

  Command-line arguments override viewer.toml settings.

Configuration:
  See viewer.toml for all available options.

Dependencies:
  This package includes all required DLLs (ffmpeg + SDL2).
  No additional installation needed.

Controls:
  ESC or close window to quit.
"@

$readmePath = Join-Path $publishDir "README.txt"
Set-Content -Path $readmePath -Value $readmeContent -Encoding UTF8
Write-Host "[INFO] Generated README.txt"

# ---- 打包 zip ----
$zipPath = Join-Path $OutputDir "viewer-windows-x64.zip"
if (Test-Path $zipPath) {
    Remove-Item $zipPath -Force
}
Compress-Archive -Path $publishDir -DestinationPath $zipPath
Write-Host "[INFO] Package created: $zipPath" -ForegroundColor Green
