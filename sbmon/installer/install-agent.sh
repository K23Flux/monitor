#!/usr/bin/env bash
set -euo pipefail

SERVER=""
TOKEN=""
NAME=""
UPDATE=false

while [[ $# -gt 0 ]]; do
  case "$1" in
    --update) UPDATE=true; shift ;;
    --server) SERVER="${2:-}"; shift 2 ;;
    --token) TOKEN="${2:-}"; shift 2 ;;
    --name) NAME="${2:-}"; shift 2 ;;
    *) echo "未知参数：$1" >&2; exit 1 ;;
  esac
done

if [[ -f /etc/sb-agent/config.json && "$UPDATE" != true ]]; then
  echo "Agent 已安装。升级请加 --update --server https://你的面板域名；不会重置节点身份。" >&2
  exit 1
fi
if [[ "$UPDATE" == true && ! -f /etc/sb-agent/config.json ]]; then
  echo "没有现有 Agent 配置，不能执行升级。" >&2
  exit 1
fi

if [[ ${EUID:-$(id -u)} -ne 0 ]]; then
  echo "请使用 root 用户运行安装命令。" >&2
  exit 1
fi

echo ""
echo "Sing-box Monitor Agent 安装程序"
echo ""

if [[ -z "$SERVER" ]]; then
  read -r -p "请输入面板地址：https://" SERVER
  if [[ "$SERVER" != http://* && "$SERVER" != https://* ]]; then
    SERVER="https://${SERVER}"
  fi
fi
SERVER="${SERVER%/}"

if [[ -z "$TOKEN" && "$UPDATE" != true ]]; then
  read -r -p "请输入一次性注册密钥：" TOKEN
fi

if [[ -z "$NAME" && "$UPDATE" != true ]]; then
  read -r -p "请输入节点名称：" NAME
fi

if [[ -z "$SERVER" || ( "$UPDATE" != true && ( -z "$TOKEN" || -z "$NAME" ) ) ]]; then
  echo "面板地址、注册密钥和节点名称不能为空。" >&2
  exit 1
fi

case "$SERVER" in
  http://*|https://*) ;;
  *) echo "面板地址必须以 http:// 或 https:// 开头。" >&2; exit 1 ;;
esac

CONFIG_PATH="/etc/sing-box/config.json"
if [[ ! -f "$CONFIG_PATH" ]]; then
  echo "未找到固定配置文件：$CONFIG_PATH" >&2
  echo "请确认 Sing-box 配置已经放在该位置。" >&2
  exit 1
fi

for command_name in curl systemctl sha256sum install; do
  if ! command -v "$command_name" >/dev/null 2>&1; then
    echo "缺少必要命令：$command_name" >&2
    exit 1
  fi
done

if ! command -v nft >/dev/null 2>&1; then
  echo "正在安装 nftables..."
  if command -v apt-get >/dev/null 2>&1; then
    apt-get update
    DEBIAN_FRONTEND=noninteractive apt-get install -y nftables
  elif command -v dnf >/dev/null 2>&1; then
    dnf install -y nftables
  elif command -v yum >/dev/null 2>&1; then
    yum install -y nftables
  else
    echo "无法自动安装 nftables，请先手动安装后重试。" >&2
    exit 1
  fi
fi

machine_arch="$(uname -m)"
case "$machine_arch" in
  x86_64|amd64) agent_arch="amd64" ;;
  aarch64|arm64) agent_arch="arm64" ;;
  *) echo "暂不支持的架构：$machine_arch" >&2; exit 1 ;;
esac

temporary_dir="$(mktemp -d)"
trap 'rm -rf "$temporary_dir"' EXIT
binary_name="sb-agent-linux-${agent_arch}"
download_base="${SERVER}/api/sbmonitor/v1/bin"

echo "正在下载 sb-agent (${agent_arch})..."
curl -fL --retry 3 --connect-timeout 10 \
  "${download_base}/${binary_name}" \
  -o "${temporary_dir}/${binary_name}"
curl -fL --retry 3 --connect-timeout 10 \
  "${download_base}/checksums.txt" \
  -o "${temporary_dir}/checksums.txt"

expected_line="$(grep " ${binary_name}$" "${temporary_dir}/checksums.txt" || true)"
if [[ -z "$expected_line" ]]; then
  echo "校验文件中没有找到 ${binary_name}。" >&2
  exit 1
fi
printf '%s\n' "$expected_line" > "${temporary_dir}/one-checksum.txt"
(
  cd "$temporary_dir"
  sha256sum -c one-checksum.txt
)

if [[ "$UPDATE" == true ]]; then
  cp -p /usr/local/bin/sb-agent "${temporary_dir}/previous-agent"
  systemctl stop sb-agent.service
fi
install -m 0755 "${temporary_dir}/${binary_name}" /usr/local/bin/sb-agent
mkdir -p /etc/sb-agent /var/lib/sb-agent
chmod 0700 /etc/sb-agent /var/lib/sb-agent

if [[ "$UPDATE" != true ]]; then
/usr/local/bin/sb-agent init \
  --server "$SERVER" \
  --token "$TOKEN" \
  --name "$NAME"
fi

unit_file="$(mktemp)"
printf '%s\n' \
  '[Unit]' \
  'Description=Sing-box Monitor Agent' \
  'After=network-online.target sing-box.service' \
  'Wants=network-online.target' \
  '' \
  '[Service]' \
  'Type=simple' \
  'ExecStart=/usr/local/bin/sb-agent run' \
  'Restart=always' \
  'RestartSec=5' \
  'NoNewPrivileges=true' \
  'UMask=0077' \
  '' \
  '[Install]' \
  'WantedBy=multi-user.target' > "$unit_file"
install -m 0644 "$unit_file" /etc/systemd/system/sb-agent.service
rm -f "$unit_file"

systemctl daemon-reload
systemctl enable --now sb-agent.service
sleep 2

echo ""
if systemctl is-active --quiet sb-agent.service; then
  echo "✓ sb-agent 安装完成并已启动"
  echo "✓ 节点名称：$NAME"
  echo "✓ 配置文件：$CONFIG_PATH"
  echo ""
  echo "查看状态：systemctl status sb-agent"
  echo "查看日志：journalctl -u sb-agent -f"
else
  if [[ "$UPDATE" == true ]]; then
    install -m 0755 "${temporary_dir}/previous-agent" /usr/local/bin/sb-agent
    systemctl restart sb-agent.service
    echo "升级未通过启动检查，已恢复旧 Agent 二进制。" >&2
  fi
  echo "sb-agent 启动失败，请运行以下命令查看原因：" >&2
  echo "journalctl -u sb-agent -n 100 --no-pager" >&2
  exit 1
fi
