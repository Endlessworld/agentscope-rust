@echo off
setlocal
rem Build release ACP binaries: acp_harness_agent + acp_agent_cli
rem Output: target\release\acp_harness_agent.exe and target\release\acp_agent_cli.exe
rem Note: Windows MSVC always emits .pdb files; exclude them when packaging.

cd /d "%~dp0"

cargo build --release -p agentscope-harness --bin acp_harness_agent --bin acp_agent_cli
set "EXITCODE=%ERRORLEVEL%"

if not "%EXITCODE%"=="0" (
  echo [build-acp-release] build FAILED, exit=%EXITCODE%
  exit /b %EXITCODE%
)

echo [build-acp-release] done:
echo   target\release\acp_harness_agent.exe
echo   target\release\acp_agent_cli.exe
exit /b 0
