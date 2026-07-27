@echo off
set LOG=e:\work\project\win-rust-libp2p\p2p-camera\scripts\vcpkg_deps.log
echo [%time%] === vcvars64 + vcpkg install (ffmpeg/sdl2/llvm) === >> %LOG%
call "C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\VC\Auxiliary\Build\vcvars64.bat" >> %LOG% 2>&1
echo [%time%] INCLUDE set, length=%INCLUDE:~0,80%... >> %LOG%
echo [%time%] launching vcpkg install ... >> %LOG%
E:\vcpkg\vcpkg.exe install ffmpeg:x64-windows sdl2:x64-windows llvm:x64-windows >> %LOG% 2>&1
echo [%time%] vcpkg install exit=%ERRORLEVEL% >> %LOG%
setx VCPKG_ROOT E:\vcpkg >nul 2>&1
setx LIBCLANG_PATH E:\vcpkg\installed\x64-windows\tools\llvm\bin >nul 2>&1
setx VCPKGRS_DYNAMIC 1 >nul 2>&1
echo [%time%] DONE. >> %LOG%
