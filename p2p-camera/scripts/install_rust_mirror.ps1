$ErrorActionPreference = "Continue"
$log = "e:\work\project\win-rust-libp2p\p2p-camera\scripts\install_rust.log"
$env:RUSTUP_DIST_SERVER = "https://rsproxy.cn"
$env:RUSTUP_UPDATE_ROOT = "https://rsproxy.cn/rustup"
$tmp = if ($env:TEMP) { $env:TEMP } else { $env:USERPROFILE }
$out = Join-Path $tmp "rustup-init.exe"
$url = "https://rsproxy.cn/rustup/dist/x86_64-pc-windows-msvc/rustup-init.exe"

Write-Output ("[$(Get-Date)] Downloading rustup-init -> $out") | Tee-Object -FilePath $log
Invoke-WebRequest -Uri $url -OutFile $out -UseBasicParsing -TimeoutSec 180
Write-Output ("[$(Get-Date)] Downloaded size=" + (Get-Item $out).Length) | Tee-Object -FilePath $log -Append

Write-Output ("[$(Get-Date)] Installing stable toolchain (uses rsproxy mirror)...") | Tee-Object -FilePath $log -Append
& $out -y --default-toolchain stable 2>&1 | Tee-Object -FilePath $log -Append
Write-Output ("[$(Get-Date)] rustup exit code=" + $LASTEXITCODE) | Tee-Object -FilePath $log -Append

# 配置 cargo 国内镜像，加速后续构建
$cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE ".cargo" }
if (-not (Test-Path $cargoHome)) { New-Item -ItemType Directory -Path $cargoHome -Force | Out-Null }
$configPath = Join-Path $cargoHome "config.toml"
$config = @"
[source.crates-io]
replace-with = "rsproxy-sparse"
[source.rsproxy-sparse]
registry = "sparse+https://rsproxy.cn/index/"
[registries.rsproxy]
index = "https://rsproxy.cn/crates.io-index"
[net]
git-fetch-with-cli = true
"@
Set-Content -Path $configPath -Value $config -Encoding utf8
Write-Output ("[$(Get-Date)] Wrote cargo config -> $configPath") | Tee-Object -FilePath $log -Append
