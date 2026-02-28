@echo off
:: Enable LAN Sync for TiddlyDesktop RS (Portable Mode)
:: Right-click this file and select "Run as administrator"
::
:: This adds a Windows Firewall rule to allow TiddlyDesktop RS
:: to communicate on your local network for wiki syncing.

net session >nul 2>&1
if %errorlevel% neq 0 (
    echo This script requires administrator privileges.
    echo Right-click "enable-lan-sync.bat" and select "Run as administrator".
    pause
    exit /b 1
)

set "APP_EXE=%~dp0tiddlydesktop-rs.exe"

echo Removing old firewall rule (if any)...
netsh advfirewall firewall delete rule name="TiddlyDesktop RS" >nul 2>&1

echo Adding firewall rule for: %APP_EXE%
netsh advfirewall firewall add rule name="TiddlyDesktop RS" dir=in action=allow program="%APP_EXE%" enable=yes profile=private,domain

if %errorlevel% equ 0 (
    echo.
    echo Firewall rule added successfully. LAN sync is now enabled.
) else (
    echo.
    echo Failed to add firewall rule. Please check your permissions.
)

pause
