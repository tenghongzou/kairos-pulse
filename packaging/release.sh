#!/bin/sh
# 正式發佈：檢查 → 清除舊產物 → 打包 Kairos 與 Pulse → （提供私鑰時）簽署合併版 SHA256SUMS。
# 完整流程（含打 tag）見 repo 根目錄的 RELEASING.md。
#
# 用法（在 repo 任一目錄執行；Windows 請用 Git Bash）：
#   sh packaging/release.sh
#   SIGNING_KEY=/安全的位置/kairos-pulse-release sh packaging/release.sh
#
# 環境變數：
#   SIGNING_KEY   簽章私鑰路徑（必須在 repo 之外）；沒有設定時只打包不簽章（例如 CI）
#   SKIP_CHECKS=1 略過 fmt／clippy／test／deny（只用於已在同一個 commit 通過這些檢查的 CI job）
#   ALLOW_DIRTY=1 允許工作目錄有未提交的變更（產物帶 -dirty，不能簽章；只用於測試打包腳本）
#   DIST_DIR、BUILD_VOLUME 見 packaging/build-release-common.sh
#
# 輸出目錄（預設 <repo>/dist）中由本腳本產生的檔案（kairos-*、pulse-*、SHA256SUMS*）會先刪除，
# 確保 SHA256SUMS 與簽章只涵蓋這次建置的兩個發佈包。
set -eu

die() {
    echo "錯誤：$*" >&2
    exit 1
}

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
REPO=$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel) || die "$SCRIPT_DIR 不在 git repo 中"
cd "$REPO"
DIST_DIR=${DIST_DIR:-$REPO/dist}
export DIST_DIR

if [ -n "$(git status --porcelain --untracked-files=normal)" ]; then
    [ "${ALLOW_DIRTY:-}" = 1 ] || die "工作目錄有未提交的變更；正式發佈必須從乾淨的 commit 建置（測試打包可設 ALLOW_DIRTY=1）"
    [ -z "${SIGNING_KEY:-}" ] || die "工作目錄有未提交的變更，不能簽章"
fi
if [ -n "${SIGNING_KEY:-}" ]; then
    [ -f "$SIGNING_KEY" ] || die "找不到私鑰 $SIGNING_KEY"
    command -v ssh-keygen >/dev/null 2>&1 || die "找不到 ssh-keygen（需要 OpenSSH 8.1 以上）"
fi

echo "==> commit $(git rev-parse --short HEAD)；tag：$(git tag --points-at HEAD | tr '\n' ' ')"

# ---- 打包前檢查（審計 D3：打包前必須通過 test／clippy／fmt／deny）--------------
if [ "${SKIP_CHECKS:-}" = 1 ]; then
    echo "==> 略過檢查（SKIP_CHECKS=1）"
else
    command -v cargo >/dev/null 2>&1 || die "找不到 cargo"
    cargo deny --version >/dev/null 2>&1 || die "找不到 cargo-deny（cargo install cargo-deny --locked）"
    echo "==> cargo fmt --all --check"
    cargo fmt --all --check
    echo "==> cargo clippy"
    cargo clippy --workspace --all-targets --locked -- -D warnings
    echo "==> cargo test"
    cargo test --workspace --locked
    echo "==> cargo deny check"
    cargo deny check
fi

# ---- 清除舊產物並打包 ------------------------------------------------------
mkdir -p "$DIST_DIR"
rm -f "$DIST_DIR"/kairos-* "$DIST_DIR"/pulse-* "$DIST_DIR"/SHA256SUMS "$DIST_DIR"/SHA256SUMS.sig
# 兩個產品各自打包時不簽章，全部完成後只簽一次
(unset SIGNING_KEY; sh kairos/packaging/build-release.sh)
(unset SIGNING_KEY; sh pulse/packaging/build-release.sh)

if [ -n "${SIGNING_KEY:-}" ]; then
    sh packaging/sign-release.sh sign "$DIST_DIR" "$SIGNING_KEY"
else
    echo "==> 未設定 SIGNING_KEY，未簽章"
fi

echo
echo "==> 發佈檔案（$DIST_DIR）"
ls -l "$DIST_DIR"
echo
cat "$DIST_DIR/SHA256SUMS"
