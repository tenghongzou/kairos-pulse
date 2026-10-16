#!/bin/sh
# 可重現性檢查（商業審計 D4）：以兩個全新的 docker volume 各從頭建置一次 Kairos 與 Pulse，
# 比對兩次產物（tar.gz、SBOM）的 SHA-256。
#
# 用法（在 repo 任一目錄執行；Windows 請用 Git Bash）：
#   sh packaging/check-reproducible.sh
#
# 每次建置都使用新的 BUILD_VOLUME（target 目錄與 cargo-about／cargo-cyclonedx 都從頭編譯），
# 只共用 crates.io 的下載快取（sync-cargo volume，內容以 Cargo.lock 的 checksum 驗證）。
# 兩次產物放在 <repo>/dist/repro/1、2（或 REPRO_DIR），建置日誌為同目錄的 build-*.log；
# 結束時刪除兩個暫時的 volume。
# 不一致時列出 tar 內哪些檔案不同，並以非 0 結束。
set -eu

die() {
    echo "錯誤：$*" >&2
    exit 1
}

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
REPO=$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel) || die "$SCRIPT_DIR 不在 git repo 中"
cd "$REPO"
WORK=${REPRO_DIR:-$REPO/dist/repro}
RUN_ID="repro-$$-$(date +%s)"
VOLUMES=""

# shellcheck disable=SC2317,SC2329 # 由 trap 呼叫（shellcheck 0.10 以前報 SC2317，0.11 起改為 SC2329）
cleanup() {
    for v in $VOLUMES; do
        docker volume rm -f "$v" >/dev/null 2>&1 || true
    done
}
trap cleanup EXIT
trap 'exit 130' INT TERM

rm -rf "$WORK"
mkdir -p "$WORK"
for i in 1 2; do
    vol="sync-build-$RUN_ID-$i"
    VOLUMES="$VOLUMES $vol"
    echo "==> 第 $i 次建置（volume $vol）"
    for p in kairos pulse; do
        DIST_DIR="$WORK/$i" BUILD_VOLUME="$vol" SIGNING_KEY="" sh "$p/packaging/build-release.sh" \
            > "$WORK/build-$i-$p.log" 2>&1 || die "第 $i 次建置 $p 失敗，日誌見 $WORK/build-$i-$p.log"
    done
    sed 's/^/    /' "$WORK/$i/SHA256SUMS"
done

echo "==> 比對"
if cmp -s "$WORK/1/SHA256SUMS" "$WORK/2/SHA256SUMS"; then
    echo "    兩次建置的所有產物完全相同（可重現）"
    exit 0
fi

echo "    兩次建置的產物不同：" >&2
diff "$WORK/1/SHA256SUMS" "$WORK/2/SHA256SUMS" >&2 || true
# 找出 tar 內不同的檔案，方便追查原因
for t in "$WORK"/1/*.tar.gz; do
    name=$(basename "$t")
    [ -f "$WORK/2/$name" ] || continue
    cmp -s "$t" "$WORK/2/$name" && continue
    for i in 1 2; do
        rm -rf "$WORK/x$i"
        mkdir -p "$WORK/x$i"
        tar xzf "$WORK/$i/$name" -C "$WORK/x$i"
        (cd "$WORK/x$i" && find . -type f -exec sha256sum {} + | sort -k 2) > "$WORK/x$i.sums"
    done
    echo "    $name 內不同的檔案：" >&2
    diff "$WORK/x1.sums" "$WORK/x2.sums" >&2 || true
done
exit 1
