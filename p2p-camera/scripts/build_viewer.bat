@echo off
REM build_viewer.bat — 双击运行：编译 Windows viewer (media_viewer with SDL player)
REM 用法:
REM   双击本文件            -> Debug 构建
REM   命令行: build_viewer.bat release   -> Release 构建
REM   命令行: build_viewer.bat arm64     -> ARM64 交叉编译 (Debug)
REM
REM 说明: 本脚本只是用 -ExecutionPolicy Bypass 调用 build_viewer.ps1，
REM       绕过系统 PowerShell 脚本执行策略限制，无需手动 set-executionpolicy。

setlocal
set SCRIPT_DIR=%~dp0
REM 项目根 = scripts\..  (即 p2p-camera/)
set PROJECT_ROOT=%SCRIPT_DIR%..

set ARGS=
if /I "%~1"=="release" set ARGS=-Release
if /I "%~1"=="arm64"   set ARGS=-Target aarch64-pc-windows-msvc

echo [INFO] Building viewer (project: %PROJECT_ROOT%)
powershell -NoProfile -ExecutionPolicy Bypass -File "%SCRIPT_DIR%build_viewer.ps1" %ARGS%
set RC=%ERRORLEVEL%

echo.
if %RC%==0 (
    echo [DONE] Build succeeded. Exe at: %PROJECT_ROOT%target\debug\examples\media_viewer.exe
) else (
    echo [ERROR] Build failed (exit %RC%). See output above.
)
pause
endlocal
