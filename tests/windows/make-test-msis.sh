#!/bin/sh
# 產生 Windows 系統層測試（tests/windows/Invoke-S0.ps1）用的兩個輔助 MSI。在有 docker 的 Linux 主機上執行：
#
#   sh tests/windows/make-test-msis.sh dist/pulse-<版本>-<commit>-windows-x86_64.zip <輸出目錄>
#
# 輸出（內容與發佈的 zip 相同，只有 MSI 的版本與 ServiceControl 不同；pulse.exe 回報的仍是原本的版本）：
#   pulse-test-legacy-0.0.1.msi   版本 0.0.1、拿掉 ServiceControl Start：重現 0.8.0～0.10.1 的 MSI（ADR-0001 的 X4），
#                                 並驗證「從舊式 MSI 升級到現行 MSI 之後服務會啟動」
#   pulse-test-next-99.0.0.msi    版本 99.0.0、現行的 pulse.wxs：驗證 MajorUpgrade 之後服務會啟動
#
# 測試用的 MSI 必須使用不同的數字版本：ProductCode 由去掉預發佈標記的數字版本推導，只差預發佈標記的
# 兩個 MSI 會被 Windows Installer 當成同一個產品（ADR-0001 第 4.8 節 M4）。
# 映像檔與 msitools 的版本取自 packaging/build-release-common.sh，與正式打包相同。這兩個 MSI 只供測試，
# 不可重現、不簽署、不發佈。
set -eu

if [ $# -ne 2 ]; then
    echo "用法：$0 <pulse-…-windows-x86_64.zip> <輸出目錄>" >&2
    exit 2
fi
ZIP=$1
OUT=$2
REPO=$(cd "$(dirname "$0")/../.." && pwd)
COMMON=$REPO/packaging/build-release-common.sh
IMAGE=$(sed -n 's/^IMAGE=//p' "$COMMON" | head -n 1)
MSITOOLS=$(grep -o 'msitools=[^ "]*' "$COMMON" | head -n 1)
if [ -z "$IMAGE" ] || [ -z "$MSITOOLS" ]; then
    echo "錯誤：無法從 $COMMON 取得映像檔或 msitools 的版本" >&2
    exit 1
fi
[ -f "$ZIP" ] || {
    echo "錯誤：找不到 $ZIP" >&2
    exit 1
}

mkdir -p "$OUT"
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
cp "$ZIP" "$WORK/pulse.zip"
cp "$REPO/pulse/packaging/windows/pulse.wxs" "$REPO/pulse/packaging/windows/pulse.default.toml" "$WORK/"

# 容器內：解開 zip（內含一層目錄）、補上 MSI 的初始 pulse.toml、以 wixl 產生兩個 MSI。
# 結束前刪除容器建立的中間檔（它們屬於 root，主機上的 trap 刪不掉）
# shellcheck disable=SC2016
INNER='
set -eu
apk add --no-cache "$MSITOOLS" >/dev/null
cd /work
mkdir unz src
unzip -q pulse.zip -d unz
cp unz/*/* src/
cp pulse.default.toml src/pulse.toml
build() { # $1 輸出檔名 $2 版本 $3 ProductCode $4 legacy｜current
    if [ "$4" = legacy ]; then
        sed -e "/PulseServiceStart/d" pulse.wxs
    else
        cat pulse.wxs
    fi | sed -e "s|@VERSION@|$2|g" -e "s|@PRODUCT_CODE@|$3|g" -e "s|@SRC@|/work/src|g" > product.wxs
    wixl -a x64 -o "$1" product.wxs
    echo "==> $1"
    msiinfo export "$1" ServiceControl | sed "s/^/    /"
}
build pulse-test-legacy-0.0.1.msi 0.0.1 0C0FFEE0-0000-4000-8000-000000000001 legacy
build pulse-test-next-99.0.0.msi 99.0.0 0C0FFEE0-0000-4000-8000-000000000099 current
# 舊式的不得有 Start（Event 的位元 1），現行的必須有
if msiinfo export pulse-test-legacy-0.0.1.msi ServiceControl | grep -q PulseServiceStart; then
    echo "錯誤：舊式 MSI 仍含 PulseServiceStart" >&2
    exit 1
fi
msiinfo export pulse-test-next-99.0.0.msi ServiceControl | grep -q PulseServiceStart || {
    echo "錯誤：現行 MSI 沒有 PulseServiceStart" >&2
    exit 1
}
rm -rf unz src product.wxs
chmod 644 ./*.msi
'
docker run --rm -e MSITOOLS="$MSITOOLS" -v "$WORK:/work" "$IMAGE" sh -c "$INNER"
cp "$WORK"/pulse-test-legacy-0.0.1.msi "$WORK"/pulse-test-next-99.0.0.msi "$OUT/"
ls -l "$OUT"/pulse-test-*.msi
