@echo off
rem ============================================================
rem  Build RepairArtemisWeb (C++, static, single exe)
rem  Prereq: MSYS2 mingw-w64 g++  (C:\msys64\mingw64)
rem ============================================================
setlocal
rem 把 msys 的 mingw64\bin 放到 PATH 最前面, 避免被其他 mingw
rem 工具 (如 gstreamer) 的同名 DLL 劫持导致 cc1plus 启动失败
set "PATH=C:\msys64\mingw64\bin;%PATH%"

g++ -std=c++17 -O2 -static -municode RepairArtemisWeb.cpp ^
    -o RepairArtemisWeb.exe -lws2_32 -lshell32 -ladvapi32
if errorlevel 1 (
    echo BUILD FAILED
    exit /b 1
)
echo BUILD OK: RepairArtemisWeb.exe
