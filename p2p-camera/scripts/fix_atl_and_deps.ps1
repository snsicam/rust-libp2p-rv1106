$ErrorActionPreference = "Continue"
$log = "e:\work\project\win-rust-libp2p\p2p-camera\scripts\fix_atl.log"
$triplet = "x64-windows"
$vcpkgRoot = "E:\vcpkg"
$vsInstaller = "C:\Program Files (x86)\Microsoft Visual Studio\Installer\vs_installer.exe"
$vsPath = "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools"

function Log($m) { Write-Output ("[$(Get-Date -Format 'HH:mm:ss')] $m") | Tee-Object -FilePath $log -Append }

Log "=== Step 1: install VS ATL component (requires admin) ==="
if (Test-Path $vsInstaller) {
    & $vsInstaller modify --installPath $vsPath --add Microsoft.VisualStudio.Component.VC.ATL --quiet --norestart 2>&1 | Tee-Object -FilePath $log -Append
    Log ("vs_installer exit=" + $LASTEXITCODE)
} else {
    Log "vs_installer.exe NOT found!"
}

# Wait for atlbase.h to appear
$atlHeader = Join-Path $vsPath "VC\Tools\MSVC\14.44.35207\include\atlbase.h"
$cnt = 0
while (-not (Test-Path $atlHeader) -and $cnt -lt 30) {
    Start-Sleep -Seconds 10
    $cnt++
    Log ("waiting for atlbase.h ... ($cnt)")
}
if (Test-Path $atlHeader) { Log "atlbase.h FOUND - ATL installed" } else { Log "atlbase.h STILL MISSING" }

Log "=== Step 2: re-run vcpkg install (ffmpeg/sdl2/llvm) ==="
& (Join-Path $vcpkgRoot "vcpkg.exe") install "ffmpeg:$triplet" "sdl2:$triplet" "llvm:$triplet" 2>&1 | Tee-Object -FilePath $log -Append
Log ("vcpkg install exit=" + $LASTEXITCODE)

$llvmBin = Join-Path $vcpkgRoot "installed\$triplet\tools\llvm\bin"
[Environment]::SetEnvironmentVariable("VCPKG_ROOT", $vcpkgRoot, "User")
[Environment]::SetEnvironmentVariable("LIBCLANG_PATH", $llvmBin, "User")
[Environment]::SetEnvironmentVariable("VCPKGRS_DYNAMIC", "1", "User")
Log "Persisted VCPKG_ROOT=$vcpkgRoot, LIBCLANG_PATH=$llvmBin, VCPKGRS_DYNAMIC=1 (user env)"
Log "DONE."
