@echo off
setlocal
rem Thin wrapper over the workspace task runner (cross-platform entry point):
rem   cargo xtask acp build durable --profile release
rem
rem Builds acp_harness_agent + acp_agent_cli with the default feature set
rem (rusqlite,durable: durable driver on the file store) and copies them to
rem target\acp\release\durable\. The plain target\release\ copies stay in place.
rem
rem Other variants:  cargo xtask acp build native   --profile release
rem                  cargo xtask acp build sqlite   --profile release
rem                  cargo xtask acp build all      --profile release
rem Note: Windows MSVC always emits .pdb files; exclude them when packaging.

cd /d "%~dp0"
cargo xtask acp build durable --profile release
exit /b %ERRORLEVEL%
