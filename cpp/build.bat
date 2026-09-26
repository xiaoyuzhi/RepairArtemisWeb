@echo off
rem ============================================================
rem  Build RepairArtemisWeb (C++, static, single exe)
rem  Prereq: MSYS2 mingw-w64 g++  (C:\msys64\mingw64)
rem ============================================================
setlocal
rem 把 msys 的 mingw64\bin 放到 PATH 最前面, 避免被其他 mingw
rem 工具 (如 gstreamer) 的同名 DLL 劫持导致 cc1plus 启动失败
set "PATH=C:\msys64\mingw64\bin;%PATH%"

rem Icon + version resource. assets\app.rc is shared with the Rust build.rs.
rem cwd must be assets while windres runs: the ICON entry uses a relative path.
rem Keep these comment lines ASCII-only - cmd reads .bat as GBK and a mojibake
rem byte can swallow a quote and corrupt the parsing of the next command line.
pushd ..\assets
windres app.rc -O coff -o app.res
set "RES_EXIT=%errorlevel%"
popd
rem No closing paren inside the echo text - it would close this if-block early.
if not "%RES_EXIT%"=="0" echo RESOURCE COMPILE FAILED: windres app.rc
if not "%RES_EXIT%"=="0" exit /b 1

g++ -std=c++17 -O2 -static -municode RepairArtemisWeb.cpp ..\assets\app.res ^
    -o RepairArtemisWeb.exe -lws2_32 -lshell32 -ladvapi32
if errorlevel 1 (
    echo BUILD FAILED
    exit /b 1
)
echo BUILD OK: RepairArtemisWeb.exe
