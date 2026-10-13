把编译好的 `ncm-castd`（Windows 为 `ncm-castd.exe`）放在这个目录下，
YesPlayMusic 的 `src/electron/castService.js` 会在开发模式下从这里启动它。

```bash
cp ../../target/release/ncm-castd .        # macOS / Linux
copy ..\..\target\release\ncm-castd.exe .  # Windows
```

打包时 `vue.config.js` 的 `extraResources` 会把这个目录带到 `<app>/resources/`。
