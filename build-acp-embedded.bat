@echo off
setlocal
rem Build embedded-profile ACP binaries (size optimized): acp_harness_agent + acp_agent_cli
rem Output: target\embedded\acp_harness_agent.exe and target\embedded\acp_agent_cli.exe
rem Requires nightly toolchain (build-std) with rust-src installed.
rem --features lite drops bundled SQLite; note MSVC always emits .pdb, exclude when packaging.

cd /d "%~dp0"

cargo +nightly build --profile embedded --config .cargo/config.embedded.toml -p agentscope-harness --no-default-features --features lite --bins
set "EXITCODE=%ERRORLEVEL%"

if not "%EXITCODE%"=="0" (
  echo [build-acp-embedded] build FAILED, exit=%EXITCODE%
  exit /b %EXITCODE%
)

echo [build-acp-embedded] done:
echo   target\embedded\acp_harness_agent.exe
echo   target\embedded\acp_agent_cli.exe
exit /b 0
