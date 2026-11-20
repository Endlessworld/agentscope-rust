@echo off
setlocal
rem Thin wrapper over the workspace task runner (cross-platform entry point):
rem   cargo xtask acp build sqlite --profile embedded
rem
rem The sqlite variant is the same feature set as before: lite,durable-sqlite.
rem Requires nightly toolchain (build-std) with rust-src installed.
rem Output: target\embedded\ (cargo) + target\acp\embedded\sqlite\ (copies)
rem
rem For all three variants: cargo xtask acp build all --profile embedded

cd /d "%~dp0"
cargo xtask acp build sqlite --profile embedded
exit /b %ERRORLEVEL%
