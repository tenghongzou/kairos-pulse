#!/bin/sh
# Pulse 發佈包打包腳本
#
# 在 docker 內建置 release 執行檔，產出（<基本名稱> = pulse-<版本>-<commit>[-dirty]-<os>-<arch>）
#   Linux x86_64、aarch64（musl 靜態連結）：<基本名稱>.{tar.gz,deb,rpm,cdx.json}
#   Windows x86_64（mingw-w64 交叉編譯）：<基本名稱>.{zip,msi,cdx.json}
#   一行安裝腳本：pulse-install.sh
#   發佈清單：pulse-<版本>-<commit>-release.json（遠端升級用；-dirty 的建置不產生）
# 並更新同目錄下與 Kairos 共用的合併版 SHA256SUMS。實作與可用的環境變數（TARGETS、PACKAGES 等）
# 見 repo 根目錄的 packaging/build-release-common.sh；正式發佈（含檢查與簽章）請用 packaging/release.sh。
#
# 用法（在 repo 任一目錄執行皆可；Windows 請用 Git Bash）：
#   sh packaging/build-release.sh          （在產品目錄下）
#   sh pulse/packaging/build-release.sh    （在 monorepo 根目錄）
set -eu

# ---- 產品設定 -------------------------------------------------------------
NAME=pulse                           # 執行檔名稱（Cargo 的 bin 名稱）
EXAMPLE_SRC=config.toml              # 產品目錄中的預設設定（編譯時嵌入，放進發佈包作為範例）
EXAMPLE_DST=pulse.example.toml       # 放進發佈包時的檔名
UNIT_SRC=packaging/pulse.service     # systemd unit 範本（Linux）
# 其他放進發佈包根目錄的檔案：錯誤代碼對照（pulse explain 指向它）、Prometheus 告警規則範例、
# 遠端升級的輔助程式 unit（tar 安裝依 INSTALL.md 第 10 節手動安裝；deb／rpm 另外裝到 /usr/lib/systemd/system）
EXTRA_SRC="ERROR-CODES.md packaging/pulse-alerts.example.yml packaging/pulse-upgrade.path packaging/pulse-upgrade.service"
# 預設建置的目標：Linux x86_64、aarch64（musl 靜態連結）與 Windows x86_64（含 MSI）
DEFAULT_TARGETS="x86_64-unknown-linux-musl aarch64-unknown-linux-musl x86_64-pc-windows-gnu"
# 直接放到輸出目錄的檔案（來源:輸出檔名）：一行安裝腳本
EXTRA_DIST="packaging/install.sh:pulse-install.sh"
# 發佈清單 pulse-<版本>-<commit>-release.json 的手動欄位（crates/kairos-proto/REMOTE.md 第 4.1.3 節）
RELEASE_META=packaging/release-meta.toml
# ---------------------------------------------------------------------------

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
# shellcheck source=../../packaging/build-release-common.sh
. "$SCRIPT_DIR/../../packaging/build-release-common.sh"
