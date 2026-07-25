@echo off
setlocal enabledelayedexpansion
set LOG=%~dp0..\target\build_viewer.log
echo === Build started at %DATE% %TIME% === > "%LOG%"

echo [1/4] Activating MSVC environment... >> "%LOG%" 2>&1
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat" >nul 2>&1
if %ERRORLEVEL% neq 0 (
    echo [ERROR] vcvars64.bat failed >> "%LOG%"
    exit /b 1
)

echo [2/4] Setting vcpkg/dev environment variables... >> "%LOG%"
set VCPKG_ROOT=E:\vcpkg
set LIBCLANG_PATH=E:\vcpkg\installed\x64-windows\bin
set VCPKGRS_DYNAMIC=1
set PATH=%LIBCLANG_PATH%;%PATH%

echo   VCPKG_ROOT=%VCPKG_ROOT% >> "%LOG%"
echo   LIBCLANG_PATH=%LIBCLANG_PATH% >> "%LOG%"
echo   VCPKGRS_DYNAMIC=%VCPKGRS_DYNAMIC% >> "%LOG%"

echo [3/4] Verifying prerequisites... >> "%LOG%"
if not exist "%LIBCLANG_PATH%\libclang.dll" (
    echo [ERROR] libclang.dll not found at %LIBCLANG_PATH% >> "%LOG%"
    exit /b 1
)
echo   libclang.dll found. >> "%LOG%"

echo [4/4] Building media_viewer (with SDL player, RELEASE)... >> "%LOG%"
cd /d "e:\work\project\rust-libp2p-win\p2p-camera"
cargo build --release --example media_viewer -p mobile-core --features player >> "%LOG%" 2>&1
set BUILD_EXIT=%ERRORLEVEL%

echo. >> "%LOG%"
echo === Build finished at %DATE% %TIME% (exit code %BUILD_EXIT%) === >> "%LOG%"

if %BUILD_EXIT% equ 0 (
    echo BUILD SUCCESS >> "%LOG%"
    echo Output: target\release\examples\media_viewer.exe >> "%LOG%"
) else (
    echo BUILD FAILED (exit code %BUILD_EXIT%) >> "%LOG%"
)

exit /b %BUILD_EXIT%
