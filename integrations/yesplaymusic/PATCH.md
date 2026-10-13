# 把推送能力接进 YesPlayMusic

`ncm-cast` 是一个独立的 Rust 守护进程（`ncm-castd`），占用 `127.0.0.1:9100`。
YesPlayMusic 用 `fetch` 驱动它 —— 不引入任何 native 模块、不改 IPC 协议、
**不改动任何现有的网易云 API 调用**（YPM 自己的 `@neteaseapireborn/api` 仍在 10754 上原样运行）。

已针对上游 commit `df075cc` 验证：`yesplaymusic-cast.patch` 可 `git apply` 干净应用。

## 一键接入

```bash
cd /path/to/YesPlayMusic
git apply /path/to/ncm-cast/integrations/yesplaymusic/yesplaymusic-cast.patch
```

补丁做了 4 处修改 + 新增 4 个文件：

| 文件 | 改动 |
|------|------|
| `src/background.js` | `init()` 里 `startCastService()`，`will-quit` 里 `stopCastService()` |
| `src/components/Player.vue` | 播放条右侧加"推送到设备"按钮，点击展开面板 |
| `src/App.vue` | 全局挂载 `<CastDevices />` |
| `vue.config.js` | `extraResources` 把 `bin/ncm-castd*` 打进安装包（必须在 asar 外） |
| `src/utils/cast.js` | 新增 — HTTP 客户端 + 响应式共享状态 |
| `src/components/CastDevices.vue` | 新增 — 设备面板 |
| `src/electron/castService.js` | 新增 — 主进程守护进程生命周期 |
| `src/assets/icons/cast.svg` | 新增 — 图标 |

## 手动接入（等价步骤）

### 1. 拷贝 4 个文件

```text
src/utils/cast.js                     →  <YPM>/src/utils/cast.js
src/components/CastDevices.vue        →  <YPM>/src/components/CastDevices.vue
src/electron/castService.js           →  <YPM>/src/electron/castService.js
src/assets/icons/cast.svg             →  <YPM>/src/assets/icons/cast.svg
```

### 2. `src/background.js`

```js
import { startNeteaseMusicApi } from './electron/services';
import { startCastService, stopCastService } from './electron/castService';  // 新增
```

```js
    this.neteaseMusicAPI = startNeteaseMusicApi();
    this.castService = startCastService();                                    // 新增
```

```js
    app.on('will-quit', () => {
      globalShortcut.unregisterAll();
      stopCastService();                                                      // 新增
    });
```

### 3. `src/components/Player.vue`

在 `right-control-buttons` 的 `<div class="volume-control">` 之前插入：

```html
<button-icon
  :class="{ active: castState.selected.length > 0 }"
  title="推送到设备（DLNA / AirPlay / HEOS / QPlay）"
  @click.native="toggleCast"
  ><svg-icon icon-class="cast"
/></button-icon>
```

```js
import { castState } from '@/utils/cast';
// data() 里： return { mouseDownTarget: null, castState };
// methods 里：toggleCast() { castState.open = !castState.open; },
```

### 4. `src/App.vue`

```js
import CastDevices from './components/CastDevices.vue';
// components: { Navbar, Player, Toast, CastDevices, ... }
```

```html
<Toast />
<CastDevices />
```

## 放置守护进程二进制

`castService.js` 按以下顺序查找，找到就启动，找不到只打一行日志、**不影响播放器启动**：

1. `$NCM_CASTD_PATH`
2. `<app>/resources/ncm-castd[.exe]`（打包后，由 `vue.config.js` 的 `extraResources` 带入）
3. `<project>/bin/ncm-castd[.exe]`（开发时）
4. `cwd/bin/`

启动参数（在 `src/electron/castService.js` 里拼好，可用环境变量覆盖）：

* `NCM_CAST_DIRECT` —— 直连模式开关，**默认开启**（取值 `0` 才关回本机网关）。
  家庭网络下渲染器通常连不回本机，直连让设备自己去网易 CDN 取流，是更稳的路径。
* `NCM_CAST_HOST` —— 交给渲染器的局域网地址，多网卡机器显式指定（如 `192.168.29.44`）。
  仅本机网关模式需要；直连模式下设备不连本机，设了也无害。

> 直连模式下设备直接抓网易 CDN，必须有稳定直链。未登录时网易会限流、
> 间歇性返回空直链，设备便停在 `stop`。播放器登录态由前端 `created()` 里
> `pushCookie()` 自动交给守护进程（`/api/netease/cookie`），登录后直链即稳定。

所以开发时把编译产物放 `bin/` 即可：

```bash
mkdir -p bin && cp target/release/ncm-castd bin/      # macOS/Linux
mkdir -p bin && copy target\release\ncm-castd.exe bin\  # Windows
```

## 登录态与音质

`CastDevices.vue` 在 `created()` 里调用 `cast.pushCookie()`，把 YPM 已有的登录态交给守护进程：
它读出 `localStorage['cookie-MUSIC_U']` / `cookie-__csrf`（见 `src/utils/auth.js`），
拼成 `MUSIC_U=…; __csrf=…; os=pc` 发给 `/api/netease/cookie`。

`os=pc` 是必须的 —— 网易的 `/api/...` 无签名接口只对 PC 端应答。

面板上可选音质 `standard / exhigh / lossless / hires`，默认 `lossless`。
守护进程按 `lossless → exhigh → higher → standard` 自动降级；无损需要 VIP 账号登录。

**无损的实现方式**：守护进程在 `/t/<id>.flac` 上做**字节级中转** ——
解析出网易 CDN 签名直链后把上游字节原样转发（透传 `Range`、`HEAD` 只回头部），
不做任何重编码。所以 DLNA/HEOS/QPlay 拿到的是原始 FLAC 文件本身。

## 多设备同步

* **HEOS**：`group/set_group` 是设备侧原生能力，勾选多个 HEOS 音箱点"多设备同步播放"
  后由设备自己保证同步，面板显示"原生同步"。
* **其他协议**：守护进程跑软件漂移校正 —— leader 领先 120 ms 起播，
  每 5 秒比对各成员进度，>250 ms 做 seek 微调，>3 s 整曲重启。
  跨品牌软件同步不是采样级精确，但人耳不可辨。

## 协议能力对照

| 协议 | 发现 | 无损 | 多设备同步 | 状态回调 |
|------|------|------|-----------|----------|
| DLNA/UPnP | SSDP `M-SEARCH` | ✅ 字节级透传 | 软件校正（5s 一次） | GENA `NOTIFY` |
| HEOS | SSDP `ACT-Denon:1` + Telnet 1255 | ✅ 字节级透传 | ✅ 原生分组 | `event/*` 推送 |
| AirPlay 1 | mDNS `_raop._tcp` | ALAC 16/44.1（无损但重编码） | 软件校正 | 轮询 |
| QPlay | mDNS + SSDP | ✅ 字节级透传 | 软件校正 | GENA `NOTIFY` |

AirPlay **2** 不做：那是 Apple 私有配对 + MFi 认证，第三方无法实现。
真 QPlay 需要腾讯 SDK（NDA + 官方认证），这里实现的是"标准 UPnP + QPlay 私有头"兼容层。

## 前端可调用的方法（`src/utils/cast.js`）

```js
import * as cast from '@/utils/cast';

await cast.refresh();                      // 一轮发现
await cast.devices();                      // 设备列表
await cast.connect(id)  / cast.disconnect(id)
await cast.play([id], cast.toTrack(track)) // 推送单曲
await cast.playQueue([id], tracks)         // 推送队列，自动续播
await cast.pause(id) / resume(id) / stop(id)
await cast.volume(id, 0..100) / cast.mute(id, true)
await cast.status(id)
await cast.group([id1, id2])               // true = 协议原生分组
await cast.ungroup()

// 网易云直通（与 YPM 现有 API 并存，互不干扰）
await cast.qrLogin() / cast.qrPoll(key)
await cast.myPlaylists() / cast.playlistTracks(id) / cast.search(kw)
await cast.pushCookie()

cast.subscribe(e => { /* e.type: device_found|device_lost|status|track_changed|end_of_track|error */ });
```
