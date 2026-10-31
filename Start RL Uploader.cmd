@echo off
cd /d "%~dp0"
py -3 -m rl_uploader --data-dir "%~dp0.local" %*
if errorlevel 1 pause
