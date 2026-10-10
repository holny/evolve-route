#!/bin/bash
# EvolveRouter 网关：编译 → 部署 → 重启 一键脚本
# 用法: ./scripts/deploy.sh          # 编译+部署+重启
#       ./scripts/deploy.sh --no-build   # 只重启（不编译）
set -euo pipefail

BIN_SRC="target/release/evo-router"
BIN_DST="$HOME/.local/bin/evo-router"
LABEL="ai.evolve.gateway"

if [[ "${1:-}" != "--no-build" ]]; then
    echo "[1/4] cargo build --release ..."
    cargo build --release
fi

# 防呆：确认二进制真的是刚构建的（SIGPIPE 掐死构建的教训）
if [[ "${1:-}" != "--no-build" ]]; then
    src_mtime=$(stat -f %m "$BIN_SRC")
    now=$(date +%s)
    age=$(( now - src_mtime ))
    if (( age > 120 )); then
        echo "错误: $BIN_SRC mtime 距今 ${age}s，构建可能未真正完成" >&2
        exit 1
    fi
fi

echo "[2/4] 部署到 $BIN_DST ..."
# 原地 cp 覆盖已签名二进制 → macOS 代码签名失效 → exec 被 SIGKILL。
# 必须 rm（换 inode）+ codesign 临时重签
rm -f "$BIN_DST"
cp "$BIN_SRC" "$BIN_DST"
codesign --force --sign - "$BIN_DST" 2>/dev/null || true

echo "[3/4] 重启 LaunchAgent ..."
launchctl kickstart -k "gui/$(id -u)/$LABEL"

echo "[4/4] 健康检查 ..."
for _ in $(seq 1 25); do
    sleep 1
    code=$(curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:8787/healthz || true)
    if [[ "$code" == "200" ]]; then
        echo "完成: 网关已就绪 (http://127.0.0.1:8787)"
        exit 0
    fi
done
echo "警告: 健康检查未通过（10s 内无 200），查看 ~/.evolve/gateway.log" >&2
exit 1
