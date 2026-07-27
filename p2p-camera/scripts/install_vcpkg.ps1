$ErrorActionPreference = "Continue"
$log = "e:\work\project\win-rust-libp2p\p2p-camera\scripts\install_vcpkg.log"
$triplet = "x64-windows"
$vcpkgRoot = "E:\vcpkg"

function Log($m) { Write-Output ("[$(Get-Date -Format 'HH:mm:ss')] $m") | Tee-Object -FilePath $log -Append }

# 清理上次失败的残留目录
if (Test-Path $vcpkgRoot) {
    Log "Removing partial C:\vcpkg ..."
    Remove-Item $vcpkgRoot -Recurse -Force -ErrorAction SilentlyContinue
}

Log "Cloning vcpkg from gitee mirror (CN fast) ..."
& git clone https://gitee.com/mirrors/vcpkg $vcpkgRoot 2>&1 | Tee-Object -FilePath $log -Append
if ($LASTEXITCODE -ne 0) {
    Log "gitee clone failed, retrying github ..."
    & git clone https://github.com/microsoft/vcpkg $vcpkgRoot 2>&1 | Tee-Object -FilePath $log -Append
}

if (-not (Test-Path (Join-Path $vcpkgRoot "vcpkg.exe"))) {
    Log "Bootstrapping vcpkg ..."
    & (Join-Path $vcpkgRoot "bootstrap-vcpkg.bat") 2>&1 | Tee-Object -FilePath $log -Append
}
else {
    Log "vcpkg.exe already present, skip bootstrap"
}
Log ("bootstrap exit=" + $LASTEXITCODE)

Log "Installing ffmpeg, sdl2, llvm for $triplet (heavy source build, ~30-60 min) ..."
& (Join-Path $vcpkgRoot "vcpkg.exe") install "ffmpeg:$triplet" "sdl2:$triplet" "llvm:$triplet" 2>&1 | Tee-Object -FilePath $log -Append
Log ("vcpkg install exit=" + $LASTEXITCODE)

$llvmBin = Join-Path $vcpkgRoot "installed\$triplet\tools\llvm\bin"
[Environment]::SetEnvironmentVariable("VCPKG_ROOT", $vcpkgRoot, "User")
[Environment]::SetEnvironmentVariable("LIBCLANG_PATH", $llvmBin, "User")
[Environment]::SetEnvironmentVariable("VCPKGRS_DYNAMIC", "1", "User")
Log "Persisted VCPKG_ROOT=$vcpkgRoot, LIBCLANG_PATH=$llvmBin, VCPKGRS_DYNAMIC=1 (user env)"
Log "vcpkg setup phase done."
