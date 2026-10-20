#!/bin/sh
# 發佈清單（release manifest，crates/kairos-proto/REMOTE.md 第 4.1.3 節、第 13 節 R1）：輸出
# <名稱>-<版本>-<commit>-release.json 的內容（一行 JSON 加換行）到 stdout。
#
# 由 packaging/build-release-common.sh 在主機上呼叫（需要 git），產品設定了 RELEASE_META 時產生，
# 放進輸出目錄並列在 SHA256SUMS 中，因此受發佈簽章保護。Kairos 匯入版本庫、Pulse 驗證提議時以
# kairos-proto 的 release::ReleaseManifest 解析。
#
# 用法：sh packaging/release-manifest.sh <名稱> <版本> <commit> <SOURCE_DATE_EPOCH> <release-meta.toml> <unit 路徑> [<commit-ish>]
#   release-meta.toml  手動維護的欄位（min_kairos、min_upgrade_from、breaking、remote_features），
#                      每行 key = "value" 或 key = true／false 或 key = ["a", "b"]
#   unit 路徑           systemd unit（相對於 repo 根目錄）：unit.sha256 為 <commit-ish>（預設 HEAD）中的內容，
#                      unit.previous_sha256 為 git 歷史中這個檔案其他所有版本的雜湊（依字典序、不重複），
#                      tar 安裝的輔助程式以此判斷客戶有沒有改過 unit（REMOTE.md 第 4.4.1 節）
#
# 輸出只由參數、release-meta.toml 與 git 歷史決定（released_at 取自 SOURCE_DATE_EPOCH 的 UTC 日期），
# 同一個 commit 重複產生的內容相同（packaging/check-reproducible.sh）。
set -eu

die() {
    echo "錯誤：$*" >&2
    exit 1
}

[ $# -ge 6 ] || die "用法：sh packaging/release-manifest.sh <名稱> <版本> <commit> <SOURCE_DATE_EPOCH> <release-meta.toml> <unit 路徑> [<commit-ish>]"
NAME=$1
VERSION=$2
COMMIT=$3
EPOCH=$4
META=$5
UNIT=$6
TREEISH=${7:-HEAD}
[ -f "$META" ] || die "找不到 $META"
command -v git >/dev/null 2>&1 || die "找不到 git"

if command -v sha256sum >/dev/null 2>&1; then
    sha256() { sha256sum | cut -d' ' -f1; }
else
    sha256() { shasum -a 256 | cut -d' ' -f1; }
fi

# release-meta.toml 的一個值（去掉註解與前後空白；字串去掉引號）
meta() {
    v=$(sed -n "s/^[[:space:]]*$1[[:space:]]*=[[:space:]]*\([^#]*\).*/\1/p" "$META" | head -n 1 |
        sed 's/[[:space:]]*$//')
    [ -n "$v" ] || die "$META 缺少 $1"
    printf '%s' "$v"
}

semver() {
    printf '%s' "$2" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$' ||
        die "$1「$2」不是 SemVer（MAJOR.MINOR.PATCH[-pre]）"
}

unquote() {
    case "$2" in
    \"*\") printf '%s' "$2" | sed 's/^"//; s/"$//' ;;
    *) die "$META 的 $1 必須是字串（\"…\"）" ;;
    esac
}

MIN_KAIROS=$(unquote min_kairos "$(meta min_kairos)")
MIN_UPGRADE_FROM=$(unquote min_upgrade_from "$(meta min_upgrade_from)")
semver version "$VERSION"
semver min_kairos "$MIN_KAIROS"
semver min_upgrade_from "$MIN_UPGRADE_FROM"
BREAKING=$(meta breaking)
case "$BREAKING" in
true | false) ;;
*) die "$META 的 breaking 必須是 true 或 false" ;;
esac
# remote_features：["config", "upgrade"] → ["config","upgrade"]（只允許小寫英文字母、數字與 _ . -）
FEATURES=$(meta remote_features)
printf '%s' "$FEATURES" | grep -Eq '^\[[[:space:]]*("[a-z0-9][a-z0-9_.-]*"[[:space:]]*(,[[:space:]]*"[a-z0-9][a-z0-9_.-]*"[[:space:]]*)*)?\]$' ||
    die "$META 的 remote_features 必須是字串陣列，例如 [\"config\", \"upgrade\"]"
FEATURES=$(printf '%s' "$FEATURES" | tr -d ' \t')

printf '%s' "$COMMIT" | grep -Eq '^[0-9a-f]{7,40}$' || die "commit「$COMMIT」不是 7～40 個小寫十六進位"
RELEASED_AT=$(date -u -d "@$EPOCH" +%Y-%m-%d 2>/dev/null || date -u -r "$EPOCH" +%Y-%m-%d) ||
    die "無法換算 SOURCE_DATE_EPOCH $EPOCH"

# unit 的雜湊：git 物件的內容（repo 中儲存的 LF 版本，與發佈包內相同），不受工作目錄的換行設定影響
git cat-file -e "$TREEISH:$UNIT" 2>/dev/null || die "$TREEISH 中沒有 $UNIT"
UNIT_SHA=$(git cat-file -p "$TREEISH:$UNIT" | sha256)
PREVIOUS=""
for c in $(git log --format=%H "$TREEISH" -- "$UNIT"); do
    h=$(git cat-file -p "$c:$UNIT" 2>/dev/null | sha256) || continue
    [ "$h" = "$UNIT_SHA" ] && continue
    PREVIOUS="$PREVIOUS $h"
done
PREVIOUS_JSON=$(for h in $PREVIOUS; do echo "$h"; done | LC_ALL=C sort -u |
    sed 's/.*/"&"/' | paste -sd, -)

printf '{"schema":"kairos-pulse.release/v1","product":"%s","version":"%s","commit":"%s","released_at":"%s","min_kairos":"%s","min_upgrade_from":"%s","remote_features":%s,"unit":{"sha256":"%s","previous_sha256":[%s]},"breaking":%s}\n' \
    "$NAME" "$VERSION" "$COMMIT" "$RELEASED_AT" "$MIN_KAIROS" "$MIN_UPGRADE_FROM" "$FEATURES" \
    "$UNIT_SHA" "$PREVIOUS_JSON" "$BREAKING"
