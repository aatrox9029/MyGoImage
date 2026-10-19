@echo off
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0cleanup-test-output.ps1"
echo.
pause
