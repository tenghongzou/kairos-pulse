#!/bin/sh
# 確認執行檔不是「測試用建置」：沒有開啟只給 tests/systemd/ 的編譯期 feature（crates/kairos-proto/REMOTE.md
# 第 17.9.9 節）。目前只有 Pulse 的 fault-injection：升級輔助程式在指定的當機點停住，等測試腳本終止整台
# 主機。這段程式不得出現在正式發佈的執行檔中。
#
# 用法：
#   sh packaging/check-release-binary.sh <執行檔>...                 每個檔案都必須是正式的建置
#   sh packaging/check-release-binary.sh --test-build <執行檔>...    每個檔案都必須是測試用建置（確認這個
#                                                                    檢查真的認得出來，不是永遠通過）
#   sh packaging/check-release-binary.sh --archive <tar.gz|deb|rpm|zip>...
#                                                                    從發佈檔取出執行檔再檢查（需要 tar；
#                                                                    deb 另需 ar，rpm 另需 rpm2cpio 與 cpio，
#                                                                    zip 另需 unzip）
# 結束代碼：0 全部符合；1 有不符合的檔案；2 用法或環境的錯誤。
#
# 由 packaging/build-release-common.sh（每個目標建置完成後，容器內）與 packaging/release.sh（打包完成後，
# 對輸出目錄中的發佈檔）呼叫；CI 的 package job 另外對上傳前的發佈檔執行一次。
#
# 判斷方式（兩者任一出現就是測試用建置）：
#   - 標記字串：開啟 feature 時才編譯進去（pulse/src/remote/upgrade/helper.rs 的 FAULT_INJECTION_MARKER；
#     單元測試 the_packaging_check_knows_the_fault_injection_marker 確認兩邊一致）
#   - `--version` 的結尾標記：開啟 feature 時 `pulse --version` 的輸出另有它（pulse/src/cli.rs）
set -eu

MARKER=pulse-fault-injection-build
VERSION_TAG='[fault-injection]'

die() {
    echo "錯誤：$*" >&2
    exit 2
}

MODE="release"
ARCHIVE=0
while [ $# -gt 0 ]; do
    case "$1" in
    --test-build) MODE="test"; shift ;;
    --archive) ARCHIVE=1; shift ;;
    --) shift; break ;;
    -*) die "未知的選項 $1" ;;
    *) break ;;
    esac
done
[ $# -gt 0 ] || die "用法：sh packaging/check-release-binary.sh [--test-build] [--archive] <檔案>..."

# is_test_build <執行檔>：含任一個標記時回傳 0
is_test_build() {
    grep -aqF -- "$MARKER" "$1" || grep -aqF -- "$VERSION_TAG" "$1"
}

# extract <發佈檔> <目錄>：把其中的執行檔（<名稱>、<名稱>.exe；名稱取自檔名開頭）解到目錄，stdout 為路徑
extract() {
    base=$(basename "$1")
    name=${base%%-*}
    case "$base" in
    *.tar.gz)
        tar -xzf "$1" -C "$2" || die "無法解開 $1"
        ;;
    *.deb)
        command -v ar >/dev/null 2>&1 || die "需要 ar 才能檢查 $1"
        (cd "$2" && ar x "$1" && for d in data.tar.*; do tar -xf "$d"; done) || die "無法解開 $1"
        ;;
    *.rpm)
        command -v rpm2cpio >/dev/null 2>&1 || die "需要 rpm2cpio 才能檢查 $1"
        (cd "$2" && rpm2cpio "$1" | cpio -idm --quiet --no-absolute-filenames) || die "無法解開 $1"
        ;;
    *.zip)
        command -v unzip >/dev/null 2>&1 || die "需要 unzip 才能檢查 $1"
        unzip -q "$1" -d "$2" || die "無法解開 $1"
        ;;
    *) die "不支援的發佈檔 $1（tar.gz、deb、rpm、zip）" ;;
    esac
    found=$(find "$2" -type f \( -name "$name" -o -name "$name.exe" \) | head -n 1)
    [ -n "$found" ] || die "$1 中找不到執行檔 $name"
    echo "$found"
}

rc=0
for f in "$@"; do
    [ -f "$f" ] || die "找不到 $f"
    target=$f
    tmp=""
    if [ "$ARCHIVE" = 1 ]; then
        case "$f" in /*) abs=$f ;; *) abs=$(pwd)/$f ;; esac
        tmp=$(mktemp -d)
        target=$(extract "$abs" "$tmp") || {
            rm -rf "$tmp"
            exit 2
        }
    fi
    if is_test_build "$target"; then
        if [ "$MODE" = release ]; then
            echo "錯誤：$f 是測試用建置（含故障注入的程式：$MARKER／$VERSION_TAG），不可發佈" >&2
            rc=1
        else
            echo "    $f：測試用建置（含故障注入的標記）"
        fi
    else
        if [ "$MODE" = test ]; then
            echo "錯誤：$f 應該是測試用建置，卻沒有故障注入的標記（feature 沒有開啟，或檢查已失效）" >&2
            rc=1
        else
            echo "    $f：沒有測試用的 feature"
        fi
    fi
    [ -z "$tmp" ] || rm -rf "$tmp"
done
exit "$rc"
