@echo off
REM ncm-cast release build (Windows)
REM
REM 注意：某些环境下安全策略会拦截 winres/link 阶段，导致 `cargo build` 被终止。
REM 若遇到，请先在本目录执行 `cargo check --workspace` 确认源码无误，
REM 再在策略允许的环境（或管理员终端）中运行本脚本。
setlocal
pushd "%~dp0"
cargo build --release --workspace
if errorlevel 1 goto :fail
if not exist "%~dp0dist" mkdir "%~dp0dist"
copy /Y "%~dp0target\release\ncm-castd.exe" "%~dp0dist\ncm-castd.exe" >nul
copy /Y "%~dp0target\release\ncm-castd.exe" "%~dp0integrations\yesplaymusic\bin\ncm-castd.exe" >nul
echo.
echo 构建完成：
echo   %~dp0dist\ncm-castd.exe
echo   %~dp0integrations\yesplaymusic\bin\ncm-castd.exe
echo.
echo 运行：
echo   dist\ncm-castd.exe --port 9100
popd
endlocal
exit /b 0

:fail
echo.
echo 构建失败，请查看上方错误信息。
popd
endlocal
exit /b 1
