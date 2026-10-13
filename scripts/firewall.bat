@echo off
chcp 65001 >nul
rem ---------------------------------------------------------------------------
rem  ncm-cast — 允许局域网设备访问本机网关端口
rem
rem  渲染器（音箱/功放）是主动来本机 HTTP 端口拉流的。Windows 防火墙默认拦掉
rem  这类"未经请求的入站连接"，表现是：设备列表里能看到音箱、连接成功、推送也
rem  返回成功，但音箱永远停在 TRANSITIONING / buffering，进度不走。
rem
rem  右键"以管理员身份运行"。
rem ---------------------------------------------------------------------------

set PORT=%1
if "%PORT%"=="" set PORT=9100

echo 正在为 TCP %PORT% 添加入站放行规则（所有网络配置文件）...
netsh advfirewall firewall delete rule name="ncm-castd" >nul 2>&1
netsh advfirewall firewall add rule name="ncm-castd" dir=in action=allow protocol=TCP localport=%PORT% profile=any
if errorlevel 1 (
  echo.
  echo 失败：需要管理员权限。请右键本文件选择"以管理员身份运行"。
  pause
  exit /b 1
)

echo.
echo 完成。验证（应显示"操作: 允许"）：
netsh advfirewall firewall show rule name="ncm-castd" | findstr /C:"本地端口" /C:"操作" /C:"方向"
echo.
echo 接着请在同一局域网的手机浏览器里打开：
echo     http://^<本机局域网IP^>:%PORT%/api/devices
echo 打不开就说明还有别的拦截（360/火绒的"局域网隐身"、路由器的"AP 隔离"）。
pause
