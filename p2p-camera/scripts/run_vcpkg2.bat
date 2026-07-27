@echo off
set LOG=E:\vcpkg_build.log
echo [%time%] === vcpkg build (interactive task) start === >> %LOG%
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat" >nul 2>&1
set VCPKG_ROOT=
echo [%time%] vcvars done, starting vcpkg install ... >> %LOG%
E:\vcpkg\vcpkg.exe install ffmpeg:x64-windows sdl2:x64-windows llvm:x64-windows >> %LOG% 2>&1
echo [%time%] vcpkg install exit=%ERRORLEVEL% >> %LOG%
setx VCPKG_ROOT E:\vcpkg >nul 2>&1
setx LIBCLANG_PATH E:\vcpkg\installed\x64-windows\tools\llvm\bin >nul 2>&1
setx VCPKGRS_DYNAMIC 1 >nul 2>&1
echo [%time%] DONE === >> %LOG%
