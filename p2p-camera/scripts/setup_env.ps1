# setup_env.ps1 — Windows 平台一键配置 Viewer 编译环境
#
# 用法:
#   .\setup_env.ps1                                    # 默认安装全部组件
#   .\setup_env.ps1 -DryRun                            # 仅检测环境状态
#   .\setup_env.ps1 -SkipRust -SkipVS                  # 跳过指定组件
#   .\setup_env.ps1 -VcpkgRoot C:\vcpkg                # 指定 vcpkg 路径
#   .\setup_env.ps1 -Target aarch64-pc-windows-msvc    # 指定编译目标
#
# 前置条件:
#   - Windows 10 (Build 1903+) 或 Windows 11
#   - 管理员权限 (安装 VS Build Tools 需要 UAC 提权)
#   - PowerShell 5.1+

param(
    [string]$VcpkgRoot = "C:\vcpkg",
    [string]$Target,
    [switch]$SkipRust,
    [switch]$SkipVS,
    [switch]$SkipVcpkg,
    [switch]$SkipLLVM,
    [switch]$DryRun
)

$ErrorActionPreference = "Stop"

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$ProjectRoot = Resolve-Path (Join-Path $ScriptDir "..\..")

# ============================================================
# 公共工具层
# ============================================================

function Write-Log {
    param(
        [Parameter(Mandatory)]
        [ValidateSet("INFO", "WARN", "ERROR")]
        [string]$Level,

        [Parameter(Mandatory)]
        [string]$Message,

        [string]$Step
    )

    $timestamp = Get-Date -Format "HH:mm:ss"
    $stepPart = if ($Step) { " [$Step]" } else { "" }
    $line = "[$timestamp] [$Level]$stepPart $Message"

    switch ($Level) {
        "ERROR" { Write-Host $line -ForegroundColor Red }
        "WARN"  { Write-Host $line -ForegroundColor Yellow }
        default { Write-Host $line }
    }
}

function Test-AdminPrivilege {
    [OutputType([bool])]
    param()

    try {
        $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
        $principal = New-Object Security.Principal.WindowsPrincipal($identity)
        return $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
    }
    catch {
        return $false
    }
}

function Get-SystemArchitecture {
    [OutputType([string])]
    param()

    try {
        $arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture
        if ($arch -eq [System.Runtime.InteropServices.Architecture]::Arm64) {
            return "aarch64-pc-windows-msvc"
        }
    }
    catch {}

    return "x86_64-pc-windows-msvc"
}

function Set-EnvVarPersistent {
    param(
        [Parameter(Mandatory)]
        [string]$Name,

        [Parameter(Mandatory)]
        [string]$Value,

        [switch]$Force
    )

    $currentValue = [Environment]::GetEnvironmentVariable($Name, "User")
    if (-not $Force -and $currentValue) {
        if (Test-Path $currentValue -ErrorAction SilentlyContinue) {
            Write-Log -Level INFO -Step "EnvVar" -Message "$Name already set to '$currentValue', skipping"
            $sessionValue = [Environment]::GetEnvironmentVariable($Name, "Process")
            if (-not $sessionValue) {
                Set-Item -Path "env:$Name" -Value $currentValue
            }
            return
        }
    }

    Set-Item -Path "env:$Name" -Value $Value
    Write-Log -Level INFO -Step "EnvVar" -Message "Set $Name = '$Value' (current session)"

    try {
        [Environment]::SetEnvironmentVariable($Name, $Value, "User")
        Write-Log -Level INFO -Step "EnvVar" -Message "Persisted $Name to user-level registry"
    }
    catch {
        Write-Log -Level WARN -Step "EnvVar" -Message "Failed to persist $Name to registry. It will only be available in this session."
    }
}

function Test-DiskSpace {
    [OutputType([bool])]
    param(
        [Parameter(Mandatory)]
        [string]$Path,

        [Parameter(Mandatory)]
        [long]$RequiredMB
    )

    try {
        $resolvedPath = Resolve-Path $Path -ErrorAction SilentlyContinue
        if (-not $resolvedPath) {
            $resolvedPath = $Path
        }
        $drive = (Split-Path -Qualifier $resolvedPath)
        if (-not $drive) { return $true }

        $disk = Get-PSDrive -Name $drive.TrimEnd(':') -ErrorAction SilentlyContinue
        if (-not $disk) { return $true }

        $freeMB = [math]::Floor($disk.Free / 1MB)
        if ($freeMB -lt $RequiredMB) {
            Write-Log -Level WARN -Step "DiskSpace" -Message "Low disk space on ${drive}. vcpkg build may require ~$($RequiredMB / 1024)GB. Current free: $freeMB MB"
            return $false
        }
        return $true
    }
    catch {
        return $true
    }
}

# ============================================================
# 安装模块
# ============================================================

function Install-RustToolchain {
    [OutputType([hashtable])]
    param(
        [Parameter(Mandatory)]
        [string]$Target,

        [switch]$DryRun
    )

    $step = "Rust"
    $result = @{ Name = "Rust"; Status = "Failed"; Path = ""; Version = ""; Message = "" }

    $rustup = Get-Command rustup -ErrorAction SilentlyContinue
    if ($rustup) {
        Write-Log -Level INFO -Step $step -Message "rustup already installed at: $($rustup.Source)"

        $toolchains = & rustup toolchain list 2>$null
        if ($toolchains -match "stable") {
            Write-Log -Level INFO -Step $step -Message "Rust toolchain already installed"
            $rustcVer = & rustc --version 2>$null
            $result.Status = "Skipped"
            $result.Path = $rustup.Source
            $result.Version = if ($rustcVer) { ($rustcVer -split ' ')[1] } else { "" }
            return $result
        }
    }

    if ($DryRun) {
        $result.Status = "Failed"
        $result.Message = "Not installed (DryRun)"
        return $result
    }

    if (-not $rustup) {
        Write-Log -Level INFO -Step $step -Message "Downloading rustup-init.exe..."
        $rustupUrl = if ($Target -match "aarch64") {
            "https://win.rustup.rs/aarch64"
        } else {
            "https://win.rustup.rs/x86_64"
        }
        $rustupInit = Join-Path $env:TEMP "rustup-init.exe"

        try {
            Invoke-WebRequest -Uri $rustupUrl -OutFile $rustupInit -UseBasicParsing
            Write-Log -Level INFO -Step $step -Message "Installing rustup..."
            & $rustupInit -y --default-toolchain stable 2>&1 | ForEach-Object {
                Write-Host $_
            }
            if ($LASTEXITCODE -ne 0) {
                Write-Log -Level ERROR -Step $step -Message "rustup installation failed"
                $result.Message = "Install manually: https://rustup.rs/"
                return $result
            }
        }
        catch {
            Write-Log -Level ERROR -Step $step -Message "Failed to download rustup. Install manually: https://rustup.rs/"
            $result.Message = "Download failed"
            return $result
        }
        finally {
            if (Test-Path $rustupInit) { Remove-Item $rustupInit -Force -ErrorAction SilentlyContinue }
        }
    }

    $toolchains = & rustup toolchain list 2>$null
    if ($toolchains -notmatch "stable") {
        Write-Log -Level INFO -Step $step -Message "Installing Rust stable toolchain..."
        & rustup toolchain install stable 2>&1 | ForEach-Object {
            Write-Host $_
        }
        if ($LASTEXITCODE -ne 0) {
            Write-Log -Level ERROR -Step $step -Message "Failed to install Rust stable toolchain"
            $result.Message = "Toolchain install failed"
            return $result
        }
    }

    $cargoCmd = Get-Command cargo -ErrorAction SilentlyContinue
    if (-not $cargoCmd) {
        Write-Log -Level WARN -Step $step -Message "cargo not found in current session. Please restart your terminal."
    }
    else {
        $cargoVer = & cargo --version 2>$null
        Write-Log -Level INFO -Step $step -Message "cargo: $cargoVer"
    }

    $rustcVer = & rustc --version 2>$null
    $rustupPath = (Get-Command rustup -ErrorAction SilentlyContinue).Source
    $result.Status = "Ready"
    $result.Path = if ($rustupPath) { $rustupPath } else { "" }
    $result.Version = if ($rustcVer) { ($rustcVer -split ' ')[1] } else { "" }
    return $result
}

function Install-VSBuildTools {
    [OutputType([hashtable])]
    param(
        [switch]$DryRun
    )

    $step = "VS"
    $result = @{ Name = "VS Build Tools"; Status = "Failed"; Path = ""; Version = ""; Message = "" }

    $vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"

    if (Test-Path $vswhere) {
        $installPath = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath 2>$null
        if ($installPath) {
            $isIDE = $installPath -match "Community|Professional|Enterprise"
            if ($isIDE) {
                Write-Log -Level INFO -Step $step -Message "Visual Studio IDE detected, skipping Build Tools installation"
            }
            else {
                Write-Log -Level INFO -Step $step -Message "Visual Studio Build Tools already installed at: $installPath"
            }
            $vsVer = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationVersion 2>$null
            $result.Status = "Skipped"
            $result.Path = $installPath
            $result.Version = if ($vsVer) { $vsVer.Trim() } else { "" }
            return $result
        }
    }
    else {
        Write-Log -Level INFO -Step $step -Message "vswhere not found, attempting to detect cl.exe"
        $cl = Get-Command cl.exe -ErrorAction SilentlyContinue
        if ($cl) {
            Write-Log -Level INFO -Step $step -Message "Found cl.exe in PATH"
            $result.Status = "Skipped"
            $result.Path = $cl.Source
            $result.Version = ""
            return $result
        }
    }

    if ($DryRun) {
        $result.Status = "Failed"
        $result.Message = "Not installed (DryRun)"
        return $result
    }

    Write-Log -Level INFO -Step $step -Message "Downloading Visual Studio Build Tools installer..."
    $vsInstallerUrl = "https://aka.ms/vs/17/release/vs_BuildTools.exe"
    $vsInstaller = Join-Path $env:TEMP "vs_BuildTools.exe"

    try {
        Invoke-WebRequest -Uri $vsInstallerUrl -OutFile $vsInstaller -UseBasicParsing
        Write-Log -Level INFO -Step $step -Message "Installing Visual Studio Build Tools 2022 (C++ Desktop workload)..."
        Write-Log -Level INFO -Step $step -Message "This may take 10-30 minutes. Please wait..."

        $proc = Start-Process -FilePath $vsInstaller -ArgumentList "--quiet", "--wait", "--norestart", "--add", "Microsoft.VisualStudio.Workload.VCTools", "--includeRecommended" -Wait -PassThru

        if ($proc.ExitCode -ne 0 -and $proc.ExitCode -ne 3010) {
            Write-Log -Level ERROR -Step $step -Message "VS Build Tools installation failed with exit code: $($proc.ExitCode)"
            $result.Message = "Download manually: https://visualstudio.microsoft.com/visual-cpp-build-tools/"
            return $result
        }
    }
    catch {
        Write-Log -Level ERROR -Step $step -Message "Failed to download Build Tools. Download manually: https://visualstudio.microsoft.com/visual-cpp-build-tools/"
        $result.Message = "Download failed"
        return $result
    }
    finally {
        if (Test-Path $vsInstaller) { Remove-Item $vsInstaller -Force -ErrorAction SilentlyContinue }
    }

    if (Test-Path $vswhere) {
        $installPath = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath 2>$null
        $vsVer = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationVersion 2>$null
        $result.Path = if ($installPath) { $installPath.Trim() } else { "" }
        $result.Version = if ($vsVer) { $vsVer.Trim() } else { "" }
    }

    $result.Status = "Ready"
    Write-Log -Level INFO -Step $step -Message "Visual Studio Build Tools installed successfully"
    return $result
}

function Install-Vcpkg {
    [OutputType([hashtable])]
    param(
        [string]$VcpkgRoot,

        [Parameter(Mandatory)]
        [string]$Target,

        [switch]$DryRun
    )

    $step = "vcpkg"
    $result = @{ Name = "vcpkg"; Status = "Failed"; Path = ""; FfmpegInstalled = $false; Sdl2Installed = $false; Version = ""; Message = "" }

    $triplet = if ($Target -match "aarch64") { "arm64-windows" } else { "x64-windows" }

    $foundRoot = $null
    if ($VcpkgRoot -and (Test-Path (Join-Path $VcpkgRoot "vcpkg.exe"))) {
        $foundRoot = $VcpkgRoot
    }
    if (-not $foundRoot -and $env:VCPKG_ROOT -and (Test-Path (Join-Path $env:VCPKG_ROOT "vcpkg.exe"))) {
        $foundRoot = $env:VCPKG_ROOT
    }
    if (-not $foundRoot) {
        $candidates = @(
            "C:\vcpkg",
            (Join-Path $ProjectRoot "vcpkg"),
            (Join-Path $env:USERPROFILE "vcpkg")
        )
        foreach ($c in $candidates) {
            if (Test-Path (Join-Path $c "vcpkg.exe")) {
                $foundRoot = $c
                break
            }
        }
    }

    if ($foundRoot) {
        Write-Log -Level INFO -Step $step -Message "Found vcpkg at: $foundRoot"
        $vcpkgVer = & (Join-Path $foundRoot "vcpkg.exe") version 2>$null
        $result.Path = $foundRoot
        $result.Version = if ($vcpkgVer) { ($vcpkgVer -split '`n')[0] } else { "" }
    }
    else {
        if ($DryRun) {
            $result.Status = "Failed"
            $result.Message = "Not installed (DryRun)"
            return $result
        }

        $gitCmd = Get-Command git -ErrorAction SilentlyContinue
        if (-not $gitCmd) {
            Write-Log -Level ERROR -Step $step -Message "git not found. Install git from https://git-scm.com/ or download vcpkg manually."
            $result.Message = "git not found"
            return $result
        }

        $installPath = "C:\vcpkg"
        Write-Log -Level INFO -Step $step -Message "Cloning vcpkg to $installPath..."
        try {
            & git clone https://github.com/microsoft/vcpkg $installPath 2>&1 | ForEach-Object {
                Write-Host $_
            }
            if ($LASTEXITCODE -ne 0) {
                Write-Log -Level ERROR -Step $step -Message "Failed to clone vcpkg"
                $result.Message = "git clone failed"
                return $result
            }
        }
        catch {
            Write-Log -Level ERROR -Step $step -Message "Failed to clone vcpkg: $_"
            $result.Message = "git clone failed"
            return $result
        }

        Write-Log -Level INFO -Step $step -Message "Bootstrapping vcpkg..."
        & (Join-Path $installPath "bootstrap-vcpkg.bat") 2>&1 | ForEach-Object {
            Write-Host $_
        }
        if ($LASTEXITCODE -ne 0) {
            Write-Log -Level ERROR -Step $step -Message "vcpkg bootstrap failed"
            $result.Message = "bootstrap failed"
            return $result
        }

        $foundRoot = $installPath
        $result.Path = $foundRoot
        $result.Status = "Ready"
        Write-Log -Level INFO -Step $step -Message "vcpkg installed at: $foundRoot"
    }

    $ffmpegHeader = Join-Path $foundRoot "installed\$triplet\include\libavcodec\avcodec.h"
    $sdl2Header = Join-Path $foundRoot "installed\$triplet\include\SDL2\SDL.h"
    $ffmpegInstalled = Test-Path $ffmpegHeader
    $sdl2Installed = Test-Path $sdl2Header

    if ($ffmpegInstalled -and $sdl2Installed) {
        Write-Log -Level INFO -Step $step -Message "vcpkg dependencies (ffmpeg, sdl2) already installed for $triplet"
        $result.FfmpegInstalled = $true
        $result.Sdl2Installed = $true
    }
    else {
        if ($DryRun) {
            $result.FfmpegInstalled = $ffmpegInstalled
            $result.Sdl2Installed = $sdl2Installed
            if ($result.Status -ne "Ready") { $result.Status = "Failed" }
            $result.Message = "Dependencies not installed (DryRun)"
            return $result
        }

        Test-DiskSpace -Path $foundRoot -RequiredMB 5120 | Out-Null

        Write-Log -Level INFO -Step $step -Message "Installing vcpkg dependencies (ffmpeg:$triplet sdl2:$triplet)..."
        Write-Log -Level INFO -Step $step -Message "This may take 10-30 minutes for ffmpeg compilation. Please wait..."

        try {
            & (Join-Path $foundRoot "vcpkg.exe") install "ffmpeg:$triplet" "sdl2:$triplet" 2>&1 | ForEach-Object {
                Write-Host $_
            }
            if ($LASTEXITCODE -ne 0) {
                $buildtrees = Join-Path $foundRoot "buildtrees"
                Write-Log -Level ERROR -Step $step -Message "vcpkg install failed. See logs in: $buildtrees"
                $result.Message = "vcpkg install failed"
                return $result
            }
        }
        catch {
            Write-Log -Level ERROR -Step $step -Message "vcpkg download failed. Check your network connection and retry."
            $result.Message = "Network error"
            return $result
        }

        $result.FfmpegInstalled = Test-Path $ffmpegHeader
        $result.Sdl2Installed = Test-Path $sdl2Header
        Write-Log -Level INFO -Step $step -Message "vcpkg dependencies installed"
    }

    if ($result.Status -ne "Ready") {
        $result.Status = "Ready"
    }
    return $result
}

function Install-LLVM {
    [OutputType([hashtable])]
    param(
        [Parameter(Mandatory)]
        [string]$VcpkgRoot,

        [Parameter(Mandatory)]
        [string]$Target,

        [switch]$DryRun
    )

    $step = "LLVM"
    $result = @{ Name = "LLVM/clang"; Status = "Failed"; Path = ""; Version = ""; Message = "" }

    $triplet = if ($Target -match "aarch64") { "arm64-windows" } else { "x64-windows" }

    if ($env:LIBCLANG_PATH -and (Test-Path (Join-Path $env:LIBCLANG_PATH "libclang.dll"))) {
        Write-Log -Level INFO -Step $step -Message "Found libclang at: $env:LIBCLANG_PATH (from LIBCLANG_PATH env)"
        $result.Status = "Skipped"
        $result.Path = $env:LIBCLANG_PATH
        $result.Version = ""
        return $result
    }

    $llvmCandidates = @(
        (Join-Path $VcpkgRoot "installed\$triplet\tools\llvm\bin"),
        "C:\Program Files\LLVM\bin"
    )
    foreach ($c in $llvmCandidates) {
        if (Test-Path (Join-Path $c "libclang.dll")) {
            Write-Log -Level INFO -Step $step -Message "Found libclang at: $c"
            $result.Status = "Skipped"
            $result.Path = $c
            $result.Version = ""
            return $result
        }
    }

    if ($DryRun) {
        $result.Status = "Failed"
        $result.Message = "Not installed (DryRun)"
        return $result
    }

    if ($VcpkgRoot -and (Test-Path (Join-Path $VcpkgRoot "vcpkg.exe"))) {
        Write-Log -Level INFO -Step $step -Message "Installing LLVM via vcpkg..."
        try {
            & (Join-Path $VcpkgRoot "vcpkg.exe") install "llvm:$triplet" 2>&1 | ForEach-Object {
                Write-Host $_
            }
            if ($LASTEXITCODE -eq 0) {
                $llvmBin = Join-Path $VcpkgRoot "installed\$triplet\tools\llvm\bin"
                if (Test-Path (Join-Path $llvmBin "libclang.dll")) {
                    Write-Log -Level INFO -Step $step -Message "LLVM installed via vcpkg"
                    $result.Status = "Ready"
                    $result.Path = $llvmBin
                    $result.Version = ""
                    return $result
                }
            }
        }
        catch {}

        Write-Log -Level WARN -Step $step -Message "vcpkg LLVM install failed. Install LLVM manually from https://releases.llvm.org/ and set LIBCLANG_PATH."
        $result.Message = "Install manually from https://releases.llvm.org/"
        $result.Status = "Warning"
        return $result
    }

    Write-Log -Level INFO -Step $step -Message "LLVM not found in standard paths. Set LIBCLANG_PATH manually if needed."
    $result.Message = "Not found. Install from https://releases.llvm.org/ and set LIBCLANG_PATH."
    $result.Status = "Warning"
    return $result
}

function Show-EnvironmentSummary {
    param(
        [Parameter(Mandatory)]
        [hashtable[]]$ComponentResults
    )

    Write-Host ""
    Write-Host "============================================" -ForegroundColor Cyan
    Write-Host "  Environment Setup Summary" -ForegroundColor Cyan
    Write-Host "============================================" -ForegroundColor Cyan
    Write-Host ""

    $criticalFailed = $false

    foreach ($comp in $ComponentResults) {
        $icon = switch ($comp.Status) {
            "Ready"   { "[OK]" }
            "Skipped" { "[OK]" }
            "Warning" { "[!!]" }
            default   { "[FAIL]" }
        }
        $color = switch ($comp.Status) {
            "Ready"   { "Green" }
            "Skipped" { "Green" }
            "Warning" { "Yellow" }
            default   { "Red" }
        }

        $verStr = if ($comp.Version) { " (v$($comp.Version))" } else { "" }
        $pathStr = if ($comp.Path) { " -> $($comp.Path)" } else { "" }
        $msgStr = if ($comp.Message) { " [$($comp.Message)]" } else { "" }

        Write-Host "  $icon $($comp.Name)$verStr$pathStr$msgStr" -ForegroundColor $color

        $isCritical = $comp.Name -in @("Rust", "VS Build Tools", "vcpkg")
        if ($isCritical -and $comp.Status -eq "Failed") {
            $criticalFailed = $true
        }
    }

    Write-Host ""

    if ($criticalFailed) {
        Write-Host "Environment setup INCOMPLETE. See [FAIL] items above for manual installation steps." -ForegroundColor Red
        Write-Host ""
        foreach ($comp in $ComponentResults) {
            if ($comp.Status -eq "Failed" -and $comp.Message) {
                Write-Host "  $($comp.Name): $($comp.Message)" -ForegroundColor Yellow
            }
        }
        Write-Host ""
        $global:SetupExitCode = 1
    }
    else {
        Write-Host "All critical components are ready!" -ForegroundColor Green
        Write-Host ""
        Write-Host "Next step: .\scripts\build_viewer.ps1" -ForegroundColor Cyan
        Write-Host ""
        $global:SetupExitCode = 0
    }
}

# ============================================================
# 主流程
# ============================================================

Write-Log -Level INFO -Step "Setup" -Message "=== Windows Viewer Build Environment Setup ==="
Write-Log -Level INFO -Step "Setup" -Message "Project root: $ProjectRoot"

# --- 前置检查：管理员权限 ---
if (-not (Test-AdminPrivilege)) {
    Write-Log -Level ERROR -Step "PreCheck" -Message "This script requires Administrator privileges. Please run as Administrator."
    exit 1
}
Write-Log -Level INFO -Step "PreCheck" -Message "Administrator privileges confirmed"

# --- 前置检查：系统架构 ---
if (-not $Target) {
    $Target = Get-SystemArchitecture
}
$triplet = if ($Target -match "aarch64") { "arm64-windows" } else { "x64-windows" }

Write-Log -Level INFO -Step "PreCheck" -Message "System architecture: $Target"
Write-Log -Level INFO -Step "PreCheck" -Message "vcpkg triplet: $triplet"

# --- DryRun 模式 ---
if ($DryRun) {
    Write-Log -Level INFO -Step "DryRun" -Message "DryRun mode: detecting environment status only"
}

$results = @()

# --- 1. Rust 工具链 ---
if ($SkipRust) {
    Write-Log -Level INFO -Step "Rust" -Message "Skipping Rust toolchain installation (-SkipRust)"
    $results += @{ Name = "Rust"; Status = "Skipped"; Path = ""; Version = ""; Message = "Skipped by user" }
}
else {
    $rustResult = Install-RustToolchain -Target $Target -DryRun:$DryRun
    $results += $rustResult
}

# --- 2. VS Build Tools ---
if ($SkipVS) {
    Write-Log -Level INFO -Step "VS" -Message "Skipping VS Build Tools installation (-SkipVS)"
    $results += @{ Name = "VS Build Tools"; Status = "Skipped"; Path = ""; Version = ""; Message = "Skipped by user" }
}
else {
    $vsResult = Install-VSBuildTools -DryRun:$DryRun
    $results += $vsResult
}

# --- 3. vcpkg + 依赖 ---
if ($SkipVcpkg) {
    Write-Log -Level INFO -Step "vcpkg" -Message "Skipping vcpkg and dependencies installation (-SkipVcpkg)"
    $results += @{ Name = "vcpkg"; Status = "Skipped"; Path = ""; Version = ""; Message = "Skipped by user" }
    $results += @{ Name = "ffmpeg"; Status = "Skipped"; Path = ""; Version = ""; Message = "Skipped by user" }
    $results += @{ Name = "SDL2"; Status = "Skipped"; Path = ""; Version = ""; Message = "Skipped by user" }
}
else {
    $vcpkgResult = Install-Vcpkg -VcpkgRoot $VcpkgRoot -Target $Target -DryRun:$DryRun
    $results += $vcpkgResult

    $results += @{
        Name = "ffmpeg"
        Status = if ($vcpkgResult.FfmpegInstalled) { "Ready" } elseif ($vcpkgResult.Status -eq "Skipped") { "Skipped" } else { "Failed" }
        Path = if ($vcpkgResult.Path) { Join-Path $vcpkgResult.Path "installed\$triplet" } else { "" }
        Version = ""
        Message = ""
    }
    $results += @{
        Name = "SDL2"
        Status = if ($vcpkgResult.Sdl2Installed) { "Ready" } elseif ($vcpkgResult.Status -eq "Skipped") { "Skipped" } else { "Failed" }
        Path = if ($vcpkgResult.Path) { Join-Path $vcpkgResult.Path "installed\$triplet" } else { "" }
        Version = ""
        Message = ""
    }
}

# --- 4. LLVM/clang ---
if ($SkipLLVM) {
    Write-Log -Level INFO -Step "LLVM" -Message "Skipping LLVM installation (-SkipLLVM)"
    $results += @{ Name = "LLVM/clang"; Status = "Skipped"; Path = ""; Version = ""; Message = "Skipped by user" }
}
else {
    $resolvedVcpkgRoot = ""
    foreach ($r in $results) {
        if ($r.Name -eq "vcpkg" -and $r.Path) {
            $resolvedVcpkgRoot = $r.Path
            break
        }
    }
    $llvmResult = Install-LLVM -VcpkgRoot $resolvedVcpkgRoot -Target $Target -DryRun:$DryRun
    $results += $llvmResult
}

# --- 5. 环境变量持久化 ---
if (-not $DryRun) {
    Write-Log -Level INFO -Step "EnvPersist" -Message "Setting environment variables..."

    $resolvedVcpkgRoot = ""
    foreach ($r in $results) {
        if ($r.Name -eq "vcpkg" -and $r.Path) {
            $resolvedVcpkgRoot = $r.Path
            break
        }
    }
    if ($resolvedVcpkgRoot) {
        Set-EnvVarPersistent -Name "VCPKG_ROOT" -Value $resolvedVcpkgRoot
    }

    $resolvedLibclangPath = ""
    foreach ($r in $results) {
        if ($r.Name -eq "LLVM/clang" -and $r.Path) {
            $resolvedLibclangPath = $r.Path
            break
        }
    }
    if ($resolvedLibclangPath) {
        Set-EnvVarPersistent -Name "LIBCLANG_PATH" -Value $resolvedLibclangPath
    }

    Set-EnvVarPersistent -Name "VCPKGRS_DYNAMIC" -Value "1"
}

# --- 6. 验证与摘要 ---
Show-EnvironmentSummary -ComponentResults $results

exit $global:SetupExitCode
