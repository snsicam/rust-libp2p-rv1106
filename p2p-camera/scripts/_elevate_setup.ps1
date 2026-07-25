# _elevate_setup.ps1 — 以管理员身份运行 setup_env.ps1 并输出到日志
$ErrorActionPreference = "Continue"
$ScriptPath = Join-Path $PSScriptRoot "setup_env.ps1"
$LogPath    = Join-Path $PSScriptRoot "setup_env.log"

Write-Output "=== Elevated setup started at $(Get-Date) ===" | Tee-Object -FilePath $LogPath
try {
    & $ScriptPath *>&1 | Tee-Object -FilePath $LogPath -Append
    $exitCode = $LASTEXITCODE
    Add-Content -Path $LogPath -Value "=== setup_env.ps1 exited with code $exitCode at $(Get-Date) ==="
}
catch {
    Add-Content -Path $LogPath -Value "=== ERROR: $_ ==="
}
