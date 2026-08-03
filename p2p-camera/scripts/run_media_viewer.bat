@echo off
REM run_media_viewer.bat — 双击运行：启动 Windows viewer (media_viewer with SDL player)
REM 前置: 先运行 build_viewer.bat 编译成功。
REM 用法:
REM   双击本文件            -> 用 viewer.toml 配置启动
REM   命令行可追加参数传给 media_viewer，例如:
REM     run_media_viewer.bat --relay "/ip4/101.35.90.171/udp/4001/quic-v1/p2p/12D3..." --camera "12D3..." --play
REM
REM 说明: 本脚本用 -ExecutionPolicy Bypass 调用 run_media_viewer.ps1，
REM       绕过系统 PowerShell 脚本执行策略限制，无需手动 set-executionpolicy。

setlocal
set SCRIPT_DIR=%~dp0

echo [INFO] Launching viewer...
powershell -NoProfile -ExecutionPolicy Bypass -File "%SCRIPT_DIR%run_media_viewer.ps1" %*
set RC=%ERRORLEVEL%

echo.
if %RC%==0 (
    echo [DONE] Viewer exited normally.
) else (
    echo [ERROR] Viewer exited with code %RC%. See output above.
)
pause
endlocal
