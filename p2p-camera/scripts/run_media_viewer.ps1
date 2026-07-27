# run_media_viewer.ps1 — Windows 平台启动 media_viewer 并实时播放
#
# 用法:
#   方式1 (配置文件): .\run_media_viewer.ps1
#     -> 首次运行自动生成 viewer.toml，编辑后重启
#
#   方式2 (命令行参数): .\run_media_viewer.ps1 -Relay <addr> -Camera <peer_id> -Play
#
# 示例:
#   .\run_media_viewer.ps1
#   .\run_media_viewer.ps1 -Relay "/ip4/101.35.90.171/udp/4001/quic-v1/p2p/12D3KooW..." -Camera "12D3KooW..." -Play
#   .\run_media_viewer.ps1 -Relay "/ip4/101.35.90.171/udp/4001/quic-v1/p2p/12D3KooW..." -Camera "12D3KooW..." -UdpPort 34501
#
# 前置条件: 已运行 build_viewer.ps1 编译成功

param(
    [string[]]$Relay,
    [string]$Camera,
    [string]$Stream,
    [switch]$Play,
    [switch]$NoAudio,
    [uint16]$UdpPort,
    [bool]$EnableMdns,
    [string]$Config
)

$ErrorActionPreference = "Stop"

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
# p2p-camera 是独立的 Cargo workspace，产物在 p2p-camera/target/ 下
$ProjectRoot = Resolve-Path (Join-Path $ScriptDir "..")

# ---- 检测编译产物 ----
$releaseExe = Join-Path $ProjectRoot "target\release\examples\media_viewer.exe"
$debugExe = Join-Path $ProjectRoot "target\debug\examples\media_viewer.exe"

$viewerBin = $null
if (Test-Path $releaseExe) {
    $viewerBin = $releaseExe
} elseif (Test-Path $debugExe) {
    $viewerBin = $debugExe
}

if (-not $viewerBin) {
    Write-Host "[ERROR] media_viewer.exe not found. Please run '.\build_viewer.ps1' first." -ForegroundColor Red
    exit 1
}

Write-Host "[INFO] Using: $viewerBin"

# ---- 构建命令参数 ----
$viewerArgs = [System.Collections.ArrayList]::new()

if ($Relay) {
    foreach ($r in $Relay) {
        $viewerArgs.AddRange(@("--relay", $r))
    }
}
if ($Camera) {
    $viewerArgs.AddRange(@("--camera", $Camera))
}
if ($Stream) {
    $viewerArgs.AddRange(@("--stream", $Stream))
}
if ($Play) {
    $viewerArgs.Add("--play") | Out-Null
}
if ($NoAudio) {
    $viewerArgs.Add("--no-audio") | Out-Null
}
if ($UdpPort) {
    $viewerArgs.AddRange(@("--udp-port", $UdpPort.ToString()))
}
if ($PSBoundParameters.ContainsKey("EnableMdns")) {
    $viewerArgs.AddRange(@("--enable-mdns", $EnableMdns.ToString().ToLower()))
}

# ---- 启动横幅 ----
Write-Host ""
Write-Host "============================================"
Write-Host "  P2P Camera Viewer (SDL Player)"
Write-Host "============================================"
if ($Relay) {
    Write-Host "  Relay:     $($Relay -join ', ')"
}
if ($Camera) {
    Write-Host "  DeviceCam: $Camera"
}
if ($Stream) {
    Write-Host "  Stream:    $Stream"
}
if ($UdpPort) {
    Write-Host "  UDP Port:  $UdpPort"
}
if (-not $Relay -and -not $Camera) {
    Write-Host "  Config: viewer.toml"
}
Write-Host ""
Write-Host "  ESC / Close window to quit"
Write-Host "============================================"
Write-Host ""

# ---- 设置环境变量 ----
$env:RUST_LOG = "info"
$env:RUST_BACKTRACE = "full"

# ---- 设置 vcpkg DLL 搜索路径 (动态链接 ffmpeg/SDL2 需要) ----
$vcpkgRoot = $env:VCPKG_ROOT
if (-not $vcpkgRoot) {
    $candidates = @("E:\vcpkg", "C:\vcpkg", (Join-Path $env:USERPROFILE "vcpkg"))
    foreach ($c in $candidates) {
        if (Test-Path (Join-Path $c "vcpkg.exe")) {
            $vcpkgRoot = $c
            break
        }
    }
}
if ($vcpkgRoot) {
    $vcpkgBin = Join-Path $vcpkgRoot "installed\x64-windows\bin"
    if (Test-Path $vcpkgBin) {
        $env:PATH = "$vcpkgBin;$env:PATH"
        Write-Host "[INFO] Added vcpkg DLLs to PATH"
    } else {
        Write-Host "[WARN] vcpkg bin directory not found: $vcpkgBin" -ForegroundColor Yellow
    }
} else {
    Write-Host "[WARN] vcpkg not found - ffmpeg/SDL2 DLLs may fail to load" -ForegroundColor Yellow
}

# ---- 默认 viewer.toml (先在 p2p-camera 目录找, 再在上级项目根找) ----
if ($PSBoundParameters.ContainsKey("Config")) {
    $configPath = $Config
} else {
    $configCandidates = @(
        (Join-Path $ProjectRoot "viewer.toml"),                       # p2p-camera/viewer.toml
        (Join-Path (Resolve-Path (Join-Path $ProjectRoot "..")) "viewer.toml")  # repo root viewer.toml
    )
    $configPath = $null
    foreach ($c in $configCandidates) {
        if (Test-Path $c) {
            $configPath = $c
            break
        }
    }
    if (-not $configPath) {
        Write-Host "[WARN] viewer.toml not found. It will be auto-generated with defaults." -ForegroundColor Yellow
        $configPath = "viewer.toml"
    }
}
$viewerArgs.Add("--config") | Out-Null
$viewerArgs.Add($configPath) | Out-Null

# ---- 创建日志目录 ----
$logDir = Join-Path $ScriptDir "logs"
if (-not (Test-Path $logDir)) {
    New-Item -ItemType Directory -Path $logDir | Out-Null
}
$logFile = Join-Path $logDir "viewer.log"

# ---- 启动进程 ----
Write-Host "[INFO] Config: $configPath"
Write-Host "[INFO] Running: $viewerBin $($viewerArgs -join ' ')"
Write-Host ""

# 注意: 不能用 `& $viewerBin @viewerArgs 2>&1 | Tee-Object`。
# 在 $ErrorActionPreference='Stop' 下, PowerShell 会把原生程序写入 stderr 的
# 每一行 (例如 eprintln! 的 "[Player] Decoder flushed...") 当作终止性
# NativeCommandError, 从而中止管道并杀掉 viewer 进程 (表现为"选中设备后直接退出")。
# 改为经 cmd.exe 在 OS 层把 stderr 合并进 stdout, PowerShell 只收到纯文本流。
$argStr = ($viewerArgs | ForEach-Object {
    if ($_ -match '[\s"]') { '"' + ($_ -replace '"', '\"') + '"' } else { $_ }
}) -join ' '
& cmd /c "`"$viewerBin`" $argStr 2>&1" | Tee-Object -FilePath $logFile
