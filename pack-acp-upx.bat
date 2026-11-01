@echo off
setlocal
rem Optional post-processing: UPX-pack the embedded ACP binaries.
rem
rem NOT part of build-acp-embedded.bat -- run it by hand, only when the size of
rem the .exe ON DISK is a hard requirement. Read docs\EMBEDDED_SIZE.md first.
rem Measured costs: +130 ms per launch, +7.5 MB resident memory, UPX0/UPX1 section
rem names (AV/EDR heuristic), no usable crash dumps or PDB mapping.
rem
rem Locating upx.exe: %UPX_EXE%, else `upx` on PATH.
rem   set UPX_EXE=C:\tools\upx\upx.exe
rem   pack-acp-upx.bat
rem Downloads: https://github.com/upx/upx/releases

cd /d "%~dp0"

set "SRC_DIR=target\embedded"
set "OUT_DIR=target\embedded\upx"

if not exist "%SRC_DIR%\acp_harness_agent.exe" (
  echo [pack-acp-upx] %SRC_DIR%\acp_harness_agent.exe not found.
  echo [pack-acp-upx] run build-acp-embedded.bat first.
  exit /b 1
)

if defined UPX_EXE (
  set "UPX_BIN=%UPX_EXE%"
) else (
  set "UPX_BIN=upx"
)

"%UPX_BIN%" --version >nul 2>&1
if errorlevel 1 (
  echo [pack-acp-upx] upx not found: install UPX 5.x and add it to PATH,
  echo [pack-acp-upx] or set UPX_EXE=C:\path\to\upx.exe
  echo [pack-acp-upx] downloads: https://github.com/upx/upx/releases
  exit /b 1
)

if not exist "%OUT_DIR%" mkdir "%OUT_DIR%"

for %%B in (acp_harness_agent acp_agent_cli) do (
  echo [pack-acp-upx] packing %%B.exe ...
  if exist "%OUT_DIR%\%%B.exe" del /f /q "%OUT_DIR%\%%B.exe"
  "%UPX_BIN%" --best --lzma -q -o "%OUT_DIR%\%%B.exe" "%SRC_DIR%\%%B.exe"
  if errorlevel 1 (
    echo [pack-acp-upx] pack FAILED on %%B.exe
    exit /b 1
  )
  "%UPX_BIN%" -t -q "%OUT_DIR%\%%B.exe"
  if errorlevel 1 (
    echo [pack-acp-upx] integrity test FAILED on %%B.exe
    exit /b 1
  )
)

echo [pack-acp-upx] done: original -^> packed
for %%F in ("%SRC_DIR%\acp_harness_agent.exe" "%SRC_DIR%\acp_agent_cli.exe") do (
  for %%P in ("%OUT_DIR%\%%~nxF") do (
    echo   %%~nxF: %%~zF bytes -^> %%~zP bytes
  )
)
exit /b 0
