@echo off
setlocal
rem Build embedded-profile ACP binaries (size optimized): acp_harness_agent + acp_agent_cli
rem Output: target\embedded\acp_harness_agent.exe and target\embedded\acp_agent_cli.exe
rem Requires nightly toolchain (build-std) with rust-src installed.
rem --features lite,durable-sqlite keeps the embedded profile but stores durable state in SQLite;
rem SQLite is bundled, so the binaries grow by the driver. AGENTSCOPE_DURABLE_STORE=file falls back.

cd /d "%~dp0"

cargo +nightly build --profile embedded --config .cargo/config.embedded.toml -p agentscope-extensions-acp --no-default-features --features lite,durable-sqlite --bins
set "EXITCODE=%ERRORLEVEL%"

if not "%EXITCODE%"=="0" (
  echo [build-acp-embedded] build FAILED, exit=%EXITCODE%
  exit /b %EXITCODE%
)

echo [build-acp-embedded] done:
for %%F in (target\embedded\acp_harness_agent.exe target\embedded\acp_agent_cli.exe) do (
  echo   %%F  %%~zF bytes
)
echo [build-acp-embedded] optional: pack-acp-upx.bat shrinks further (see docs\EMBEDDED_SIZE.md)
exit /b 0
