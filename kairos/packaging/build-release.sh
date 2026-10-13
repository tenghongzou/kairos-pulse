#!/bin/sh
# Kairos 發佈包打包腳本
#
# 以 docker 建置 musl 靜態連結的 release 執行檔，產出
# <repo>/dist/kairos-<版本>-<commit>[-dirty]-linux-x86_64.tar.gz，並更新同目錄下與 Pulse
# 共用的合併版 SHA256SUMS。實作與可用的環境變數見 repo 根目錄的
# packaging/build-release-common.sh；正式發佈（含檢查與簽章）請用 packaging/release.sh。
#
# 用法（在 repo 任一目錄執行皆可；Windows 請用 Git Bash）：
#   sh packaging/build-release.sh          （在產品目錄下）
#   sh kairos/packaging/build-release.sh   （在 monorepo 根目錄）
set -eu

# ---- 產品設定 -------------------------------------------------------------
NAME=kairos                          # 執行檔名稱（Cargo 的 bin 名稱）
EXAMPLE_SRC=kairos.example.toml      # 產品目錄中的範例設定
EXAMPLE_DST=kairos.example.toml      # 放進 tar 時的檔名
UNIT_SRC=packaging/kairos.service    # systemd unit 範本
# 其他放進 tar 根目錄的檔案（以空白分隔，路徑不可含空白）：CRL 定期更新的 unit 範例、
# 資料儲存與主控台 API 的說明
EXTRA_SRC="packaging/kairos-crl.service packaging/kairos-crl.timer STORAGE.md API.md"
# ---------------------------------------------------------------------------

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=../../packaging/build-release-common.sh
. "$SCRIPT_DIR/../../packaging/build-release-common.sh"
