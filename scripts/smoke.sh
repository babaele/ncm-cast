#!/usr/bin/env bash
# 冒烟测试：确认守护进程活着、能发现设备、能拿到网易云数据。
# 用法：./scripts/smoke.sh [端口]
set -u
BASE="http://127.0.0.1:${1:-9100}"

j() { python -c "import sys,json;d=json.load(sys.stdin);print(json.dumps(d,ensure_ascii=False,indent=2)[:2000])" 2>/dev/null || cat; }

echo "== 1. 发现设备 =="
curl -s -m 30 -X POST "$BASE/api/refresh" | j

echo
echo "== 2. 设备列表 =="
curl -s -m 10 "$BASE/api/devices" | j

echo
echo "== 3. 事件总线（取一次） =="
curl -s -m 30 "$BASE/api/events?cursor=0" | j

echo
echo "== 4. 网易云二维码登录 =="
curl -s -m 30 "$BASE/api/netease/qr" | python -c "
import sys,json
d=json.load(sys.stdin)
if d.get('ok'):
    print('unikey:', d['data']['unikey'], '| image bytes:', len(d['data']['image']))
else:
    print('失败:', d.get('error'))
"

echo
echo "== 5. 搜索（无需登录，验证 API 通道） =="
curl -s -m 30 "$BASE/api/netease/search?kw=起风了&limit=3" | j | head -c 800
echo
