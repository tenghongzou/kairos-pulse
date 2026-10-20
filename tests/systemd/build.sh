#!/bin/sh
# systemd 系統層測試（tests/systemd/README.md）的發佈物：以目前 commit 打包「舊版」與幾個測試用的
# 較新版本，全部以測試時臨時產生的 ssh 金鑰簽章（不使用正式的發佈金鑰）。
#
# 用法（在 kp-build 等有 docker 的 Linux 主機、repo 的 git clone 內）：
#   sh tests/systemd/build.sh <工作目錄>
#
# 產出（<工作目錄>/dist/<名稱>/，每個目錄是一次「發佈」：SHA256SUMS、SHA256SUMS.sig、發佈清單與發佈檔）：
#   kairos         Kairos（x86_64 musl tar.gz，不簽章；測試環境的 Kairos 伺服器）
#   0.9.0          Pulse HEAD（Cargo.toml 的版本）：tar.gz、deb、rpm、pulse-install.sh
#   0.9.1-test.1   同一份程式碼，只改版本號（升級成功）
#   0.9.1-test.2   另加「無參數啟動時立刻以結束代碼 1 結束」（服務一直崩潰：健康確認失敗 → 自動回滾）；
#                  --version、check-config、--print-agent-id 正常，所以會通過輔助程式的預檢
#   0.9.1-test.3   同 test.1，只改版本號（重新開機接續後升級成功）
#   0.9.1-test.4   同 test.2（崩潰），版本高於 test.3（回滾期間重新開機）
#   evil-a         test.1 的檔案，SHA256SUMS 改以「沒有人信任」的金鑰 evil-a 簽章（Kairos 匯入時拒絕）
#   0.9.1-test.9   test.1 的檔案改名為 test.9，以 evil-b 簽章：Kairos 的信任錨有 evil-b（匯入成功），
#                  Pulse 的信任錨沒有（Pulse 拒絕，PLS-E8401）
# 金鑰（<工作目錄>/keys/）：release（Pulse 與 Kairos 都信任）、evil-a、evil-b；allowed_signers（Pulse）、
# kairos_allowed_signers（Kairos：release＋evil-b）。
#
# 版本號的修改只在 <工作目錄>/src/ 的暫時 clone 中提交，不影響這個 repo。已建置過的版本沿用
# （REBUILD=1 全部重建）。docker volume：BUILD_VOLUME（預設 kp-systemd-test-build）與共用的 sync-cargo。
set -eu

die() {
    echo "錯誤：$*" >&2
    exit 1
}
say() {
    echo "==> $*" >&2
}

WORK=${1:?用法：sh tests/systemd/build.sh <工作目錄>}
mkdir -p "$WORK"
WORK=$(cd "$WORK" && pwd)
REPO=$(git rev-parse --show-toplevel) || die "請在 repo 的 git clone 內執行"
if git -C "$WORK" rev-parse --show-toplevel >/dev/null 2>&1; then
    die "$WORK 在 git repo 內（簽章腳本拒絕 repo 內的私鑰），請換一個工作目錄"
fi
[ -z "$(git -C "$REPO" status --porcelain --untracked-files=no)" ] ||
    echo "警告：工作目錄有未提交的修改，建置只使用 HEAD 的內容" >&2
BASE=$(git -C "$REPO" rev-parse HEAD)
BASE_VERSION=$(sed -n 's/^version[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' "$REPO/pulse/Cargo.toml" | head -n 1)
export BUILD_VOLUME="${BUILD_VOLUME:-kp-systemd-test-build}"
export TARGETS=x86_64-unknown-linux-musl
export BUILD_JOBS="${BUILD_JOBS:-2}"

# ---- 臨時金鑰 -------------------------------------------------------------
KEYS=$WORK/keys
mkdir -p "$KEYS"
chmod 700 "$KEYS"
for k in release evil-a evil-b; do
    [ -f "$KEYS/$k" ] || ssh-keygen -q -t ed25519 -N '' -C "kp systemd test $k" -f "$KEYS/$k"
done
signer() {
    printf 'release@kairos-pulse namespaces="kairos-pulse-release" %s\n' "$(cut -d' ' -f1,2 "$KEYS/$1.pub")"
}
signer release >"$KEYS/allowed_signers"
{
    signer release
    signer evil-b
} >"$KEYS/kairos_allowed_signers"

# ---- 原始碼（暫時的 clone；版本號與崩潰的修改只提交在這裡）-----------------------
# $1 名稱、$2 版本、$3 crash（1 = 無參數啟動時立刻結束）
prepare_src() {
    d=$WORK/src/$1
    rm -rf "$d"
    mkdir -p "$WORK/src"
    git clone -q --no-local "$REPO" "$d"
    git -C "$d" checkout -q --detach "$BASE"
    changed=0
    if [ "$2" != "$BASE_VERSION" ]; then
        sed -i "0,/^version = \"$BASE_VERSION\"/s//version = \"$2\"/" "$d/pulse/Cargo.toml"
        awk -v v="$2" '
            /^name = "pulse"$/ { print; hit = 1; next }
            hit && /^version = / { print "version = \"" v "\""; hit = 0; next }
            { hit = 0; print }
        ' "$d/Cargo.lock" >"$d/Cargo.lock.new"
        mv "$d/Cargo.lock.new" "$d/Cargo.lock"
        grep -q "^version = \"$2\"" "$d/pulse/Cargo.toml" || die "無法修改 pulse/Cargo.toml 的版本"
        changed=1
    fi
    if [ "${3:-0}" = 1 ]; then
        # 服務（沒有任何參數）一啟動就以 1 結束：Restart=always → NRestarts 增加 → 輔助程式判定崩潰
        sed -i 's|^fn main() -> ExitCode {$|&\n    if std::env::args_os().len() == 1 {\n        eprintln!("kp systemd test: crash build, exiting with 1");\n        return ExitCode::from(1);\n    }|' \
            "$d/pulse/src/main.rs"
        grep -q 'kp systemd test: crash build' "$d/pulse/src/main.rs" || die "無法加入崩潰的修改"
        changed=1
    fi
    if [ "$changed" = 1 ]; then
        git -C "$d" -c user.name="kp systemd test" -c user.email="kp-systemd-test@invalid" \
            commit -q -am "test: $1（只用於 tests/systemd，不提交到 repo）"
    fi
}

# $1 名稱（dist 目錄）、$2 版本、$3 crash
build_pulse() {
    out=$WORK/dist/$1
    if [ -f "$out/SHA256SUMS.sig" ] && [ -z "${REBUILD:-}" ] && [ "$(cat "$out/.base" 2>/dev/null)" = "$BASE" ]; then
        say "沿用 $out"
        return
    fi
    say "建置 Pulse $2（$1）"
    prepare_src "$1" "$2" "${3:-0}"
    rm -rf "$out"
    mkdir -p "$out"
    (cd "$WORK/src/$1" && DIST_DIR="$out" SIGNING_KEY="$KEYS/release" sh pulse/packaging/build-release.sh) >"$WORK/build-$1.log" 2>&1 ||
        die "建置失敗，見 $WORK/build-$1.log"
    echo "$BASE" >"$out/.base"
    rm -rf "$WORK/src/$1"
}

build_kairos() {
    out=$WORK/dist/kairos
    if ls "$out"/kairos-*-linux-x86_64.tar.gz >/dev/null 2>&1 && [ -z "${REBUILD:-}" ] && [ "$(cat "$out/.base" 2>/dev/null)" = "$BASE" ]; then
        say "沿用 $out"
        return
    fi
    say "建置 Kairos"
    prepare_src kairos "$BASE_VERSION" 0
    rm -rf "$out"
    mkdir -p "$out"
    (cd "$WORK/src/kairos" && DIST_DIR="$out" PACKAGES='' sh kairos/packaging/build-release.sh) >"$WORK/build-kairos.log" 2>&1 ||
        die "建置失敗，見 $WORK/build-kairos.log"
    echo "$BASE" >"$out/.base"
    rm -rf "$WORK/src/kairos"
}

build_kairos
build_pulse "$BASE_VERSION" "$BASE_VERSION"
build_pulse 0.9.1-test.1 0.9.1-test.1
build_pulse 0.9.1-test.2 0.9.1-test.2 1
build_pulse 0.9.1-test.3 0.9.1-test.3
build_pulse 0.9.1-test.4 0.9.1-test.4 1

# ---- 簽章錯誤的發佈 ---------------------------------------------------------
sums() {
    (cd "$1" && rm -f SHA256SUMS SHA256SUMS.sig && for f in pulse-*; do sha256sum "$f"; done >SHA256SUMS)
}
T1=$WORK/dist/0.9.1-test.1
say "簽章錯誤的發佈：evil-a（沒有人信任的金鑰）、0.9.1-test.9（只有 Kairos 信任的金鑰）"
rm -rf "$WORK/dist/evil-a" "$WORK/dist/0.9.1-test.9"
mkdir -p "$WORK/dist/evil-a" "$WORK/dist/0.9.1-test.9"
cp "$T1"/pulse-* "$T1/SHA256SUMS" "$WORK/dist/evil-a/"
sh "$REPO/packaging/sign-release.sh" sign "$WORK/dist/evil-a" "$KEYS/evil-a" >/dev/null
for f in "$T1"/pulse-0.9.1-test.1-*-linux-x86_64.tar.gz "$T1"/pulse-0.9.1-test.1-*-linux-x86_64.deb \
    "$T1"/pulse-0.9.1-test.1-*-linux-x86_64.rpm "$T1"/pulse-0.9.1-test.1-*-release.json; do
    n=$(basename "$f" | sed 's/0\.9\.1-test\.1/0.9.1-test.9/')
    case "$n" in
    *-release.json) sed 's/"version":"0\.9\.1-test\.1"/"version":"0.9.1-test.9"/' "$f" >"$WORK/dist/0.9.1-test.9/$n" ;;
    *) cp "$f" "$WORK/dist/0.9.1-test.9/$n" ;;
    esac
done
sums "$WORK/dist/0.9.1-test.9"
sh "$REPO/packaging/sign-release.sh" sign "$WORK/dist/0.9.1-test.9" "$KEYS/evil-b" >/dev/null

say "完成"
for d in "$WORK"/dist/*/; do
    echo "  $(basename "$d")：$(cd "$d" && printf '%s ' *)" >&2
done
