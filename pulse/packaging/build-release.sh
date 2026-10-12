#!/bin/sh
# Pulse 發佈包打包腳本
#
# 以 docker 建置 musl 靜態連結的 release 執行檔，產出
# <repo>/dist/pulse-<版本>-<commit>[-dirty]-linux-x86_64.tar.gz，並更新同目錄下與 Kairos
# 共用的合併版 SHA256SUMS。實作與可用的環境變數見 repo 根目錄的
# packaging/build-release-common.sh；正式發佈（含檢查與簽章）請用 packaging/release.sh。
#
# 用法（在 repo 任一目錄執行皆可；Windows 請用 Git Bash）：
#   sh packaging/build-release.sh          （在產品目錄下）
#   sh pulse/packaging/build-release.sh    （在 monorepo 根目錄）
set -eu

# ---- 產品設定 -------------------------------------------------------------
NAME=pulse                           # 執行檔名稱（Cargo 的 bin 名稱）
EXAMPLE_SRC=config.toml              # 產品目錄中的預設設定（編譯時嵌入，放進 tar 作為範例）
EXAMPLE_DST=pulse.example.toml       # 放進 tar 時的檔名
UNIT_SRC=packaging/pulse.service     # systemd unit 範本
EXTRA_SRC=""                         # 其他放進 tar 根目錄的檔案（無）
# ---------------------------------------------------------------------------

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=../../packaging/build-release-common.sh
. "$SCRIPT_DIR/../../packaging/build-release-common.sh"
