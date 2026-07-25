$uh = [Environment]::GetFolderPath('UserProfile')
$cargoHome = Join-Path $uh '.cargo'
if (-not (Test-Path $cargoHome)) { New-Item -ItemType Directory -Path $cargoHome -Force | Out-Null }
$configPath = Join-Path $cargoHome 'config.toml'
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
Write-Output ("Wrote cargo config -> " + $configPath)
$cargoExe = Join-Path $cargoHome 'bin\cargo.exe'
$rustcExe = Join-Path $cargoHome 'bin\rustc.exe'
Write-Output ("cargo: " + (& $cargoExe --version))
Write-Output ("rustc: " + (& $rustcExe --version))
