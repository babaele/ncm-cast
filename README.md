# ncm-cast — 第三方网易云音乐播放器的多协议推送引擎

把 **DLNA/UPnP、HEOS、AirPlay 1、QPlay** 四种推送协议内置进第三方网易云音乐播放器
（以 [YesPlayMusic](https://github.com/qier222/YesPlayMusic) 这类 Electron + Vue 架构为目标），
提供统一的设备发现、连接管理、播放控制与状态回调，并且**保持与现有网易云 API 完全兼容**——
播放器自己该怎么请求还怎么请求，推送只是额外的一层。

---

## 一、为什么这样做

已有的开源播放器（YesPlayMusic 等）已经把「网易云 API + 界面 + 播放队列」做得很成熟，
真正缺的是把声音送进音箱的能力。所以这里**不重做播放器**，而是做一个常驻的推送守护进程：

```text
┌───────────────────────────────┐
│ 第三方播放器（Electron / Vue） │
│  现有 NetEase API 调用不变     │
└───────────────┬───────────────┘
                │  http://127.0.0.1:9100/api/*
┌───────────────▼───────────────┐
│        ncm-castd (Rust)       │
│  ┌──────────┐ ┌─────────────┐ │
│  │ 统一接口 │ │ 本地音频网关 │ │
│  └────┬─────┘ └──────┬──────┘ │
│   ┌───┴───┬──────┬───┴───┐    │
│   │ UPnP  │ HEOS │AirPlay│QPlay│
└───┴───────┴──────┴───────┴────┘
        │       │       │      │
     音箱/功放  HEOS   AirPlay  QPlay
```

**关键点：音频不进播放器的音频栈。** 对支持"拉取"的协议，守护进程把网易云 CDN 的原始字节
**原封不动**中继给设备（`/t/<song-id>.flac`），没有解码、没有重编码、没有采样率/位深转换，
所以 24/96 的 FLAC 到设备端依然是逐字节相同——这才是真正的"无损推送"，而不是"高码率转码"。

---

## 二、协议支持与诚实边界

| 协议 | 设备发现 | 连接管理 | 播放控制 | 状态回调 | 无损 | 多设备同步 |
|------|---------|---------|---------|---------|------|-----------|
| **DLNA / UPnP AV** | SSDP `M-SEARCH` | SOAP 会话 + GENA 订阅续期 | `SetAVTransportURI` / Play / Pause / Stop / Seek / Volume / Mute | GENA `NOTIFY`（`LastChange`） | ✅ 字节级透传 | 软件漂移校正（5s 一次） |
| **HEOS**（Denon/Marantz） | SSDP `ACT-Denon:1` | Telnet 1255 长连接 | `play_stream` / `set_play_state` / `set_volume` / `set_mute` | `event/player_*` 推送 | ✅ 字节级透传 | ✅ **原生分组**（`group/set_group`，由设备保证同步） |
| **AirPlay 1 / RAOP** | mDNS `_raop._tcp` | RTSP 会话（OPTIONS/ANNOUNCE/SETUP/RECORD） | RTP 推送 / `SET_PARAMETER volume` | 轮询 | ALAC 16 bit / 44.1 kHz（**无损，但要重编码**） | 软件漂移校正 |
| **QPlay** | mDNS `_qplay._tcp` + SSDP | 同 UPnP | 同 UPnP + 私有头 | GENA `NOTIFY` | ✅ 字节级透传 | 软件漂移校正 |

### 必须讲清楚的三件事

1. **AirPlay 2 不做。** 它需要 Apple 私有配对流程和 MFi 硬件认证，第三方无法实现。
   这里实现的是 **AirPlay 1（RAOP）**，2018 年前的 AirPort Express、大量 AirPlay 1 音箱和
   不少 AV 功放仍然支持，且**无需配对**。AirPlay 1 上限是 16 bit / 44.1 kHz ALAC，
   所以它是无损的，但做不到"字节级透传"。
2. **QPlay 是兼容层。** 真正的 QPlay 协议需要腾讯 SDK（NDA）+ 官方认证实验室过检，
   第三方拿不到。公开的部分是：QPlay 设备本质上是 UPnP AV 渲染器，只额外要求几个私有
   SOAP 头。所以本实现 = 标准 UPnP + `QPlay: 1` 私有头。只校验头的设备能正常工作；
   强制要求认证控制器的设备会拒绝。
3. **跨品牌软件同步达不到采样级。** 只有 HEOS 是设备自己保证的原生同步。其余协议用
   "统一起播 + 每 5 秒按 leader 位置校正一次漂移（>3s 则重启该设备）"的策略，
   听感上属于同一个房间，但不是专业多房间系统的精度。

---

## 三、目录结构

```text
ncm-cast/
├── engine/                  # cast-engine（库）
│   └── src/
│       ├── traits.rs        # Adapter / Sink —— 四个协议共用的接口
│       ├── model.rs         # Device / Track / Status / Event / Quality
│       ├── ssdp.rs          # SSDP 发现（UPnP / HEOS / QPlay 共用）
│       ├── mdns.rs          # 手写 mDNS/DNS-SD 查询（AirPlay / QPlay）
│       ├── upnp.rs          # UPnP AV：SOAP 控制 + GENA 事件订阅
│       ├── heos.rs          # HEOS：Telnet 1255 + 原生分组
│       ├── airplay.rs       # AirPlay 1：RTSP + RTP/ALAC + RSA-OAEP
│       ├── qplay.rs         # QPlay 兼容层
│       ├── gateway.rs       # 本地音频网关（字节级透传，支持 Range/HEAD）
│       ├── netease.rs       # 网易云 API（无签名 /api 端点）
│       ├── crypto.rs        # AES-128-CBC（仅 RAOP 用）
│       ├── group.rs         # 多设备同步与漂移校正
│       ├── registry.rs      # 四协议发现聚合 + 设备生命周期
│       ├── hub.rs           # 事件总线 + 设备回调路由
│       └── engine.rs        # 会话管理与播放编排
├── daemon/                  # ncm-castd（二进制）
│   └── src/{main.rs,rpc.rs} # HTTP JSON-RPC + 事件长轮询 + 网关
└── integrations/
    └── yesplaymusic/
        ├── PATCH.md                   # 接入说明（含协议能力对照）
        ├── yesplaymusic-cast.patch    # 可直接 git apply 的补丁（上游 df075cc 已验证）
        ├── bin/                       # 放 ncm-castd 供开发模式启动
        └── src/                       # 4 个待拷入 YPM 的文件
            ├── utils/cast.js
            ├── components/CastDevices.vue
            ├── electron/castService.js
            └── assets/icons/cast.svg
```

---

## 四、构建与运行

```bash
# 构建（Windows 也可直接双击 build.bat）
cargo build --release --workspace

# 运行
./target/release/ncm-castd --port 9100
# 输出：ncm-castd listening on http://127.0.0.1:9100 (LAN: http://192.168.1.5:9100)
```

Cookie（扫码登录后获得，用于 VIP/无损/自己的歌单）保存在可执行文件旁的 `ncm-cookie.txt`。

### HTTP API

| 方法 | 路径 | 说明 |
|------|------|------|
| GET | `/api/devices` | 已发现设备列表 |
| POST | `/api/refresh` | 做一次发现（返回新增/消失） |
| POST | `/api/connect` `{id}` | 建立会话 |
| POST | `/api/disconnect` `{id}` | 断开 |
| POST | `/api/play` `{ids, track, direct?}` | 推送单曲（`direct` 可覆盖投递方式） |
| POST | `/api/queue` `{ids, tracks, direct?}` | 推送队列（自动续播） |
| GET | `/api/direct` | 当前投递方式（false=本机网关 / true=网易 CDN） |
| POST | `/api/direct` `{direct}` | 切换投递方式 |
| GET | `/api/info` | 守护进程信息：局域网地址、端口、投递方式 |
| POST | `/api/next` `{ids}` | 下一首 |
| POST | `/api/pause` `/api/resume` `/api/stop` `{id}` | 传输控制 |
| POST | `/api/volume` `{id, volume}` / `/api/mute` `{id, muted}` | 音量 |
| GET | `/api/status?id=` | 状态快照 |
| POST | `/api/group` `{ids}` | 多设备同步（返回是否原生分组） |
| POST | `/api/ungroup` | 解散分组 |
| GET | `/api/events?cursor=` | 事件长轮询（最长 25 秒） |
| GET | `/api/netease/qr` | 登录二维码（JPEG base64） |
| GET | `/api/netease/qr/poll?key=` | 轮询登录结果 |
| GET | `/api/netease/playlists` | 我的歌单 |
| GET | `/api/netease/playlist?id=&quality=` | 歌单歌曲 |
| GET | `/api/netease/search?kw=&limit=` | 搜索 |
| GET/HEAD | `/t/<song-id>.flac?q=lossless` | **音频网关**（设备从这里拉流，支持 Range） |
| POST | `/evt/<path>` | 设备回调（UPnP GENA `NOTIFY`） |

---

## 五、接入 YesPlayMusic

一行命令：

```bash
cd /path/to/YesPlayMusic
git apply /path/to/ncm-cast/integrations/yesplaymusic/yesplaymusic-cast.patch
```

补丁已对上游 commit `df075cc` 用 `git apply --check` 验证过可干净应用。

| 类型 | 文件 | 作用 |
|------|------|------|
| 改 | `src/background.js` | `init()` 启动守护进程，`will-quit` 停掉 |
| 改 | `src/components/Player.vue` | 播放条右侧「推送到设备」按钮 |
| 改 | `src/App.vue` | 全局挂载 `<CastDevices />` |
| 改 | `vue.config.js` | `extraResources` 打包 `bin/ncm-castd*` |
| 增 | `src/utils/cast.js` | HTTP 客户端 + 响应式共享状态 |
| 增 | `src/components/CastDevices.vue` | 设备面板（发现/连接/控制/分组/自动推送） |
| 增 | `src/electron/castService.js` | 主进程守护进程生命周期 |
| 增 | `src/assets/icons/cast.svg` | 图标 |

YPM 自己的 `@neteaseapireborn/api`（10754）与 express（27232）原样保留，
守护进程只是多出来的第三个本地服务（9100）。**不改动任何现有网易云 API 调用。**

登录态：`CastDevices.vue` 启动时把 `localStorage['cookie-MUSIC_U']` / `cookie-__csrf`
拼成 `MUSIC_U=…; __csrf=…; os=pc` 交给守护进程，VIP 无损直接可用。

详见 [`integrations/yesplaymusic/PATCH.md`](integrations/yesplaymusic/PATCH.md)。

---

## 六、与 swyh-rs 的关系

`swyh-rs` 的 `rendercontrol/`（SSDP + SOAP）和 `netease/proxy.rs`（字节级透传中继）
是本项目的原型来源。这里把它们抽出来、去掉 GUI/i18n 依赖，并补上 HEOS / AirPlay / QPlay
三个协议和统一的会话与同步层。

网易云 API 用的是**当前无签名 `/api/...` 端点**：`weapi`（AES-CBC+RSA）和 `eapi`（AES-ECB+MD5）
已经被网易服务端下线，签名请求一律返回 `200 OK` 空 body。详见 `engine/src/netease.rs` 顶部表格。

---

## 七、已知限制

* AirPlay 1 只支持 FLAC 源（MP3 源请走 DLNA/HEOS）。
* 跨品牌软件同步不是采样级（见上文第 3 点）。
* QPlay 仅为兼容层，未经腾讯官方认证。
* 守护进程目前无鉴权，建议只在本机/可信局域网使用。
* 无 cookie 时付费曲（网易返回 `code:-110 / fee:1`）拿不到直链 —— 先在播放器里登录，
  前端会把 cookie 交给守护进程。

---

## 八、实测记录（2026-10-08，Windows + Denon Home 350）

真机：**Denon Home 350（主卧）192.168.29.45**，同一台设备同时以两种身份被发现。

| 项目 | 结果 |
|------|------|
| `cargo check --workspace --all-targets` | ✅ exit 0，零代码警告 |
| `cargo test -p cast-engine --lib` | ✅ 8 passed（AES/CBC 向量、SOAP 与 DIDL 合法性、GENA URL、网卡评分） |
| `POST /api/refresh` | ✅ 发现 `upnp:c6e1ab95…` 与 `heos:1245386843`，均识别为 `Denon Home 350` |
| `POST /api/connect` | ✅ UPnP SOAP + GENA SUBSCRIBE 均 200；HEOS Telnet 1255 握手成功 |
| `POST /api/play` | ✅ `SetAVTransportURI` 200（手工 SOAP 亦 200） |
| `GET /api/status` | ⚠️ 返回 `playing` / `buffering`，但见下一节 |
| `GET /api/netease/search?kw=起风了` | ✅ 返回真实曲目（id/标题/艺术家/时长/封面） |
| `GET /t/1330348068.mp3?q=standard` HEAD | ✅ `audio/mpeg`、`Accept-Ranges: bytes`、`Content-Length: 5214920` |
| `Range: bytes=0-1023` | ✅ `206 Partial Content`，1024 字节，首 4 字节 `49 44 33 04` = `ID3\x04` —— 字节级透传确认 |
| HEOS `player/get_players` | ✅ `pid=1245386843 name=主卧 model=Denon Home 350 version=3.139.170` |
| HEOS `player/play_stream` | ✅ 返回 `success`，但随后 `event/player_state_changed state=stop` |
| `GET /api/netease/qr` | ❌ 本机被网易风控拦截（`-462` 检测到您的网络环境存在风险） |

二维码 `-462` 不是代码缺陷：网易按**出口 IP** 判定，用同样 UA/Referer 的裸 `curl` 同样返回
`-462`。实际使用中不需要它 —— YesPlayMusic 自己已登录，前端会调 `/api/netease/cookie`
把现有 cookie 交给守护进程。代码已把这个错误翻译成人话，不再抛 JSON。

---

## 九、真机联调排障：音箱连不上来拉流

**症状**：设备能发现、连接成功、推送返回 `ok`，但状态永远停在
`buffering` / `TRANSITIONING`，`position_ms` 不动。

**根因**：DLNA/HEOS 是"拉"模式 —— 我们只给音箱一个 URL，音箱要**主动连回本机**
HTTP 端口取流。这条反向连接被挡住时，一切控制命令都会成功，只有声音不来。

按以下顺序排查（前两条在本机实测中都命中过）：

1. **Windows 防火墙拦掉未经请求的入站连接**（最常见）
   管理员运行：
   ```bat
   scripts\firewall.bat 9100
   ```
   或手工：
   ```bat
   netsh advfirewall firewall add rule name="ncm-castd" dir=in action=allow protocol=TCP localport=9100 profile=any
   ```
   ⚠️ 注意：Windows 有时会给 `ncm-castd.exe` 自动创建**"阻止"**规则（本实测环境里
   `v4/v5/v6` 三个路径各有一条）。先 `netsh advfirewall firewall delete rule name="ncm-castd"`
   清干净再加放行。

2. **360 安全卫士 / 火绒 的"局域网隐身 / 局域网防护"**
   本实测机器上 `360tray.exe` 在运行。这类功能会静默丢掉局域网入站包，
   需要在其"流量防火墙 / 局域网防护"里放行，或临时关闭验证。

3. **路由器 AP 隔离（客户端隔离）**
   音箱在 Wi-Fi、电脑在有线时尤其常见。手机连同一 Wi-Fi，浏览器打开
   `http://<本机IP>:9100/api/devices` —— 打不开就是网络层隔离，
   需要到路由器里关掉"AP 隔离/客户端隔离"。

4. **选错了网卡地址**
   电脑上同时有 VMware/VirtualBox/Hyper-V/WSL/Tailscale 虚拟网卡时，
   它们也有私有 IPv4（如 `192.168.44.1`、`192.168.121.1`）。守护进程现在会
   **排除虚拟网卡并优先物理网卡**，但多网卡机器仍建议显式指定：
   ```bash
   ncm-castd.exe --port 9100 --host 192.168.29.44
   ```
   启动时日志会打印实际交给音箱的地址，核对一下是否和音箱同网段。

5. **URL 里的查询串**
   部分老渲染器不接受带 `?` 的 URL。网关同时接受路径式写法：
   `/t/lossless/1330348068.flac`（等价于 `/t/1330348068.flac?q=lossless`）。

**兜底方案：直连模式（推荐先试这个）**

如果上面几条你都不想动，直接让设备绕开本机，自己去网易 CDN 取流：

```bash
ncm-castd.exe --port 9100 --direct
# 或运行中切换：
curl -X POST http://127.0.0.1:9100/api/direct -H "Content-Type: application/json" -d '{"direct":true}'
```

本实测环境（Windows 防火墙已放行、地址也正确，设备仍连不回来）下，
把同一首歌的**网易 CDN 直链**交给 Denon Home 350 后立刻正常播放：

```text
event/player_state_changed        state=play
event/player_now_playing_progress cur_pos=0    duration=325000
event/player_now_playing_progress cur_pos=5000 duration=325000
player/get_play_state             state=play
```

对比之下，指向本机网关时设备连一个 TCP 连接都不会建立（`state=stop`、
`TRANSITIONING` 卡死）。这证明协议实现正确，卡点在网络入站。

两种模式的取舍：

| | 本机网关（默认） | 直连模式（`--direct`） |
|---|---|---|
| 设备需要连回本机 | 是 | **否** |
| `Content-Length` / `Range` | 有，且准确 | 取决于 CDN |
| MIME 类型 | 我们指定 | CDN 决定 |
| 需登录/付费曲 | 支持（走我们的 cookie） | 依赖 CDN 签名有效期 |
| 无损 / Hi-Res | 支持 | 支持 |
| 直链过期 | 无所谓（每次现取） | CDN 链接有 `?vuutv=` 时效，长队列需重新解析 |

面板里也有对应开关（"直连模式"），一键切换。

> **直连模式必须有稳定可用的 CDN 直链。** 设备是自己去网易 CDN 取流的，
> 守护进程只负责把直链交给它——不像本机网关那样能在取流失败时重新中继。
> 未登录时网易对无 cookie 的 `/api/song/enhance/player/url/v1` 接口会**限流、
> 间歇性返回空直链**，表现就是设备装上了 "Url Stream" 却停在 `stop`、进度不动。
> 解决办法：用播放器登录态（前端 `created()` 里自动 `pushCookie()`）或守护进程
> 扫码登录（`/api/netease/qr`）拿到 cookie，直链就稳定了。
> 另外直连模式每次播放都**重新解析**直链（不读 20 分钟缓存），避免把过期缓存
> 直链交给设备。

排查工具：`scripts/probe_server.py`（看音箱到底发了什么请求）、
`scripts/gena_probe.py`（验证音箱能否回调本机）、`scripts/heos_probe.py`（裸 Telnet 调 HEOS）。

---

## 十、二进制

`target/release/ncm-castd.exe`（约 4.0 MB，单文件，无外部依赖）。

```bash
ncm-castd.exe --port 9100 [--host 192.168.29.44] [--direct] [--cookie-file <path>]
```

* `--host` —— 交给渲染器的局域网地址，多网卡机器建议显式指定（见上一节第 4 条）。
* `--direct` —— 直连模式（见上节）。集成进 YesPlayMusic 时**默认开启**，因为多数家庭网络下渲染器连不回本机；设 `NCM_CAST_DIRECT=0` 可关回本机网关。
* `--cookie-file` —— 默认是可执行文件旁的 `ncm-cookie.txt`。
* 日志级别用 `RUST_LOG=debug|info|warn|error`。
