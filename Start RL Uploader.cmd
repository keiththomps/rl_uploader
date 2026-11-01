@echo off
setlocal
if exist "%LOCALAPPDATA%\Programs\RLUploader\rl-uploader.exe" (
  start "" "%LOCALAPPDATA%\Programs\RLUploader\rl-uploader.exe" %*
  exit /b 0
)
if not exist "%~dp0target\release\rl-uploader.exe" (
  echo Build the Rust app first with Build.ps1, then run this launcher again.
  pause
  exit /b 1
)
start "" "%~dp0target\release\rl-uploader.exe" --data-dir "%~dp0.local" %*
