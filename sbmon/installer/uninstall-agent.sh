#!/usr/bin/env bash
set -euo pipefail

if [[ ${EUID:-$(id -u)} -ne 0 ]]; then
  echo "请使用 root 用户运行。" >&2
  exit 1
fi

systemctl disable --now sb-agent.service 2>/dev/null || true
rc-service sb-agent stop 2>/dev/null || true
rc-update del sb-agent default >/dev/null 2>&1 || true
rm -f /etc/systemd/system/sb-agent.service /etc/init.d/sb-agent
rm -f /usr/local/bin/sb-agent
systemctl daemon-reload 2>/dev/null || true

echo "sb-agent 已卸载。"
echo "配置和累计状态仍保留在 /etc/sb-agent 与 /var/lib/sb-agent。"
echo "确认不再需要后，可手动删除这两个目录。"
