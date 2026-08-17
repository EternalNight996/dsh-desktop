#!/usr/bin/env bash
# 安装 Tauri 2 在 Ubuntu/Debian 上所需的系统依赖（一次性）
# 用法：just setup-linux   （或：bash scripts/setup-linux.sh）
set -euo pipefail

echo "==> 检测发行版..."
if ! command -v apt-get >/dev/null 2>&1; then
  echo "错误：本脚本仅支持 Ubuntu/Debian（apt-get）。"
  echo "其他发行版（Fedora/Arch 等）请参考 Tauri 官方文档："
  echo "  https://v2.tauri.app/start/prerequisites/"
  exit 1
fi

echo "==> 更新软件源并安装依赖..."
sudo apt-get update
sudo apt-get install -y \
  libwebkit2gtk-4.1-dev \
  build-essential \
  curl \
  wget \
  file \
  libxdo-dev \
  libssl-dev \
  libayatana-appindicator3-dev \
  librsvg2-dev

echo "==> 完成。接下来：just setup && just dev"
