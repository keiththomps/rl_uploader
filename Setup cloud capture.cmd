@echo off
cd /d "%~dp0"
py -3 -m pip install --target "%~dp0.vendor" -r requirements.txt
if errorlevel 1 pause
