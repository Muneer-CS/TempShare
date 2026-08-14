@echo off
setlocal
cd /d "%~dp0"

if not exist ".env" copy /Y ".env.example" ".env" >NUL
if not exist "data" mkdir "data"
set "TEMPSHARE_AUTO_TUNNEL=true"

echo TempShare management dashboard: http://127.0.0.1:7420
echo A secure public HTTPS address will appear in the dashboard.
echo.
echo Keep this window open while sharing files.
start "" "http://127.0.0.1:7420"
tempshare.exe

if errorlevel 1 (
  echo.
  echo TempShare stopped with an error. Review .env and the messages above.
  pause
)
