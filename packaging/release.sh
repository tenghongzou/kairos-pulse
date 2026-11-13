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
#   SKIP_CHECKS=1 略過 fmt／clippy／test（含端對端測試）／deny（只用於已在同一個 commit 通過這些檢查的 CI job）
#   ALLOW_DIRTY=1 允許工作目錄有未提交的變更（產物帶 -dirty，不能簽章；只用於測試打包腳本）
#   TEST_FEATURES 不可設定：測試用的 Cargo feature（tests/systemd 的故障注入）只能由 tests/systemd/build.sh
#                 經 build-release-common.sh 開啟；這裡看到就拒絕執行
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

# 測試用的 feature（tests/systemd 的故障注入，crates/kairos-proto/REMOTE.md 第 17.9.9 節）不得進入正式發佈：
# 環境中有 TEST_FEATURES 時（例如從測試用的 shell 執行）直接拒絕，不是悄悄忽略
[ -z "${TEST_FEATURES:-}" ] ||
    die "環境變數 TEST_FEATURES=$TEST_FEATURES：測試用的 feature 不可用於正式發佈（請 unset TEST_FEATURES）"

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
    # 與 CI 的 check job 相同：先跑端對端測試以外的測試，再建置兩個執行檔、單獨跑端對端測試（tests/e2e）
    echo "==> cargo test（端對端測試以外）"
    cargo test --workspace --locked --exclude kp-e2e
    echo "==> cargo build -p kairos -p pulse（端對端測試用）"
    cargo build -p kairos -p pulse --bins --locked
    echo "==> cargo test -p kp-e2e（端對端測試）"
    cargo test -p kp-e2e --locked
    echo "==> cargo deny check"
    cargo deny check
fi

# ---- 清除舊產物並打包 ------------------------------------------------------
mkdir -p "$DIST_DIR"
rm -f "$DIST_DIR"/kairos-* "$DIST_DIR"/pulse-* "$DIST_DIR"/SHA256SUMS "$DIST_DIR"/SHA256SUMS.sig
# 兩個產品各自打包時不簽章，全部完成後只簽一次
(unset SIGNING_KEY; sh kairos/packaging/build-release.sh)
(unset SIGNING_KEY; sh pulse/packaging/build-release.sh)

# ---- 發佈檔不得是測試用建置 --------------------------------------------------
# 每個目標建置完成時已在容器內檢查過執行檔（build-release-common.sh）；這裡再對「要發佈的檔案」檢查一次：
# 取出 tar.gz 內的執行檔，確認沒有測試用 feature（故障注入）的程式。簽章之前執行，不通過就不簽
echo "==> 確認發佈檔不是測試用建置（packaging/check-release-binary.sh）"
for f in "$DIST_DIR"/kairos-*.tar.gz "$DIST_DIR"/pulse-*.tar.gz; do
    [ -f "$f" ] || die "找不到發佈檔 $f"
    sh packaging/check-release-binary.sh --archive "$f" || die "$f 不可發佈"
done

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
