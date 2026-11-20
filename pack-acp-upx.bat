@echo off
setlocal
rem Thin wrapper over the workspace task runner (cross-platform entry point):
rem   cargo xtask acp upx sqlite --profile embedded
rem
rem Optional post-processing: UPX-pack the embedded ACP binaries.
rem
rem NOT part of the build -- run it by hand, only when the size of the .exe ON
rem DISK is a hard requirement. Read docs\EMBEDDED_SIZE.md first.
rem Measured costs: +130 ms per launch, +7.5 MB resident memory, UPX0/UPX1
rem section names (AV/EDR heuristic), no usable crash dumps or PDB mapping.
rem
rem Locating upx.exe: %UPX_EXE%, else `upx` on PATH.
rem   set UPX_EXE=C:\tools\upx\upx.exe
rem   pack-acp-upx.bat
rem Downloads: https://github.com/upx/upx/releases

cd /d "%~dp0"
cargo xtask acp upx sqlite --profile embedded
exit /b %ERRORLEVEL%
