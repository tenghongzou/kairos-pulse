#!/bin/sh
# systemd 系統層測試（tests/systemd/README.md）的發佈物：以目前 commit 打包「舊版」與幾個測試用的
# 較新版本，全部以測試時臨時產生的 ssh 金鑰簽章（不使用正式的發佈金鑰）。
#
# 用法（在 kp-build 等有 docker 的 Linux 主機、repo 的 git clone 內）：
#   sh tests/systemd/build.sh <工作目錄>
#
# 產出（<工作目錄>/dist/<名稱>/，每個目錄是一次「發佈」：SHA256SUMS、SHA256SUMS.sig、發佈清單與發佈檔）：
#   kairos         Kairos（x86_64 musl tar.gz，不簽章；測試環境的 Kairos 伺服器）
#   <V0>           Pulse HEAD（Cargo.toml 的版本，例如 0.10.0）：tar.gz、deb、rpm、pulse-install.sh
#   <N>-test.1     同一份程式碼，只改版本號（升級成功）。<N> 是 <V0> 的下一個 PATCH（0.10.0 → 0.10.1），
#                  所以測試用的版本一定比 HEAD 新（不會被當成降版）
#   <N>-test.2     另加「無參數啟動時立刻以結束代碼 1 結束」（服務一直崩潰：健康確認失敗 → 自動回滾）；
#                  --version、check-config、--print-agent-id 正常，所以會通過輔助程式的預檢
#   <N>-test.3     同 test.1，只改版本號（重新開機接續後升級成功）
#   <N>-test.4     同 test.2（崩潰），版本高於 test.3（回滾期間重新開機）
#   <N>-test.5     同 test.1，另開啟 Pulse 的編譯期 feature fault-injection（TEST_FEATURES；REMOTE.md 第 17.9.9 節）：
#   <N>-test.6     升級輔助程式可以在指定的當機點停住，讓 run.sh 終止整台主機（情境 crash-replace、
#                  crash-rollback）。**測試用建置**：`pulse --version` 結尾有 [fault-injection]，打包腳本確認
#                  執行檔含故障注入的標記；其他版本（含 <V0>）都是正式的建置，打包腳本確認它們不含
#   <N>-test.7     同 test.2（崩潰），版本高於 test.6（crash-rollback 的目標；不需要 fault-injection）
#   <N>-test.8     同 test.1，只改版本號，版本高於 test.7（late-resume 的目標：健康確認的期限已過才接續 →
#                  升級成功；正式的建置，接續的輔助程式就是它）
#   <N>-test.10    同 test.2（崩潰），版本高於 test.8，**以第二把發佈金鑰 release2 簽章**（金鑰輪替之後的發佈；
#                  情境 key-revoked），另外它的 prerm／%preun 在主機上有 /etc/kp-systest-prerm-fail 時以 1 結束
#                  （失敗的版本帶著壞掉的維護指令碼；情境 pkg-rollback-fails 的 rpm）
#   evil-a         test.1 的檔案，SHA256SUMS 改以「沒有人信任」的金鑰 evil-a 簽章（Kairos 匯入時拒絕）
#   <N>-test.9     test.1 的檔案改名為 test.9，以 evil-b 簽章：Kairos 的信任錨有 evil-b（匯入成功），
#                  Pulse 的信任錨沒有（Pulse 拒絕，PLS-E8401）
# 金鑰（<工作目錄>/keys/）：release、release2（Pulse 與 Kairos 都信任；release2 是輪替後的第二把）、evil-a、
# evil-b；allowed_signers（Pulse：release＋release2）、kairos_allowed_signers（Kairos：release＋release2＋evil-b）。
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
# 測試用的較新版本：<BASE_VERSION 的下一個 PATCH>-test.N（與 run.sh 相同的算法）
NEXT_VERSION=$(echo "$BASE_VERSION" | awk -F. '{ printf "%d.%d.%d", $1, $2, $3 + 1 }')
NEXT_RE=$(echo "$NEXT_VERSION" | sed 's/\./\\./g')
export BUILD_VOLUME="${BUILD_VOLUME:-kp-systemd-test-build}"
export TARGETS=x86_64-unknown-linux-musl
export BUILD_JOBS="${BUILD_JOBS:-2}"
# 測試用的 feature 只由下面的 build_pulse 逐次指定；環境中既有的值不沿用
unset TEST_FEATURES

# ---- 臨時金鑰 -------------------------------------------------------------
KEYS=$WORK/keys
mkdir -p "$KEYS"
chmod 700 "$KEYS"
for k in release release2 evil-a evil-b; do
    [ -f "$KEYS/$k" ] || ssh-keygen -q -t ed25519 -N '' -C "kp systemd test $k" -f "$KEYS/$k"
done
signer() {
    printf 'release@kairos-pulse namespaces="kairos-pulse-release" %s\n' "$(cut -d' ' -f1,2 "$KEYS/$1.pub")"
}
{
    signer release
    signer release2
} >"$KEYS/allowed_signers"
{
    signer release
    signer release2
    signer evil-b
} >"$KEYS/kairos_allowed_signers"

# ---- 原始碼（暫時的 clone；版本號與崩潰的修改只提交在這裡）-----------------------
# $1 名稱、$2 版本、$3 crash（1 = 無參數啟動時立刻結束）、$4 prerm（1 = 維護指令碼 prerm／%preun 在主機上有
# /etc/kp-systest-prerm-fail 時以 1 結束）
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
    if [ "${4:-0}" = 1 ]; then
        # 失敗的版本帶著壞掉的維護指令碼：主機上有記號檔時 prerm（deb）／%preun（rpm）以 1 結束。沒有記號檔時
        # 與正式的指令碼相同
        sed -i 's|^set -e$|&\nif [ -e /etc/kp-systest-prerm-fail ]; then\n    echo "kp systemd test: prerm fails on request" >\&2\n    exit 1\nfi|' \
            "$d/pulse/packaging/pkg/preremove.sh"
        grep -q 'kp systemd test: prerm fails' "$d/pulse/packaging/pkg/preremove.sh" || die "無法加入 prerm 的修改"
        changed=1
    fi
    if [ "$changed" = 1 ]; then
        git -C "$d" -c user.name="kp systemd test" -c user.email="kp-systemd-test@invalid" \
            commit -q -am "test: $1（只用於 tests/systemd，不提交到 repo）"
    fi
}

# $1 名稱（dist 目錄）、$2 版本、$3 crash、$4 測試用的 feature（TEST_FEATURES；空＝正式的建置）、
# $5 簽章的金鑰（預設 release）、$6 prerm（見 prepare_src）
build_pulse() {
    out=$WORK/dist/$1
    features=${4:-}
    key=${5:-release}
    variant="key=$key prerm=${6:-0}"
    if [ -f "$out/SHA256SUMS.sig" ] && [ -z "${REBUILD:-}" ] && [ "$(cat "$out/.base" 2>/dev/null)" = "$BASE" ] &&
        [ "$(cat "$out/.features" 2>/dev/null)" = "$features" ] &&
        [ "$(cat "$out/.variant" 2>/dev/null || echo "key=release prerm=0")" = "$variant" ]; then
        say "沿用 $out"
        return
    fi
    say "建置 Pulse $2（$1${features:+；測試用建置：$features}；$variant）"
    prepare_src "$1" "$2" "${3:-0}" "${6:-0}"
    rm -rf "$out"
    mkdir -p "$out"
    (cd "$WORK/src/$1" && DIST_DIR="$out" SIGNING_KEY="$KEYS/$key" TEST_FEATURES="$features" \
        sh pulse/packaging/build-release.sh) >"$WORK/build-$1.log" 2>&1 ||
        die "建置失敗，見 $WORK/build-$1.log"
    # 打包腳本的檢查（packaging/check-release-binary.sh）確實執行過：正式的建置不含故障注入、測試用建置含
    if [ -n "$features" ]; then
        grep -q '測試用建置（含故障注入的標記）' "$WORK/build-$1.log" ||
            die "$1 應該是測試用建置，但打包時沒有確認到故障注入的標記（$WORK/build-$1.log）"
    else
        grep -q '沒有測試用的 feature' "$WORK/build-$1.log" ||
            die "$1 打包時沒有執行測試用 feature 的檢查（$WORK/build-$1.log）"
    fi
    echo "$BASE" >"$out/.base"
    echo "$features" >"$out/.features"
    echo "$variant" >"$out/.variant"
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
build_pulse "$NEXT_VERSION-test.1" "$NEXT_VERSION-test.1"
build_pulse "$NEXT_VERSION-test.2" "$NEXT_VERSION-test.2" 1
build_pulse "$NEXT_VERSION-test.3" "$NEXT_VERSION-test.3"
build_pulse "$NEXT_VERSION-test.4" "$NEXT_VERSION-test.4" 1
build_pulse "$NEXT_VERSION-test.5" "$NEXT_VERSION-test.5" 0 fault-injection
build_pulse "$NEXT_VERSION-test.6" "$NEXT_VERSION-test.6" 0 fault-injection
build_pulse "$NEXT_VERSION-test.7" "$NEXT_VERSION-test.7" 1
build_pulse "$NEXT_VERSION-test.8" "$NEXT_VERSION-test.8"
build_pulse "$NEXT_VERSION-test.10" "$NEXT_VERSION-test.10" 1 "" release2 1

# 正式的建置與測試用建置都以「要安裝的檔案」再確認一次（取出 tar.gz 內的執行檔）
for d in "$BASE_VERSION" "$NEXT_VERSION-test.1" "$NEXT_VERSION-test.2" "$NEXT_VERSION-test.3" "$NEXT_VERSION-test.4" \
    "$NEXT_VERSION-test.7" "$NEXT_VERSION-test.8" "$NEXT_VERSION-test.10"; do
    sh "$REPO/packaging/check-release-binary.sh" --archive "$WORK/dist/$d"/pulse-*-linux-x86_64.tar.gz >/dev/null ||
        die "$d 應該是正式的建置，卻含測試用 feature 的程式"
done
for d in "$NEXT_VERSION-test.5" "$NEXT_VERSION-test.6"; do
    sh "$REPO/packaging/check-release-binary.sh" --test-build --archive "$WORK/dist/$d"/pulse-*-linux-x86_64.tar.gz >/dev/null ||
        die "$d 應該是測試用建置，卻沒有故障注入的標記"
done
# 設定了 TEST_FEATURES 時，正式發佈的腳本拒絕執行（在刪除或建置任何東西之前）
if (cd "$REPO" && TEST_FEATURES=fault-injection SKIP_CHECKS=1 DIST_DIR="$WORK/dist/never" sh packaging/release.sh) \
    >"$WORK/release-refused.log" 2>&1; then
    die "packaging/release.sh 沒有拒絕 TEST_FEATURES（$WORK/release-refused.log）"
fi
grep -q 'TEST_FEATURES' "$WORK/release-refused.log" || die "packaging/release.sh 不是因為 TEST_FEATURES 而失敗（$WORK/release-refused.log）"
[ ! -e "$WORK/dist/never" ] || die "packaging/release.sh 在拒絕之前建立了輸出目錄"

# ---- 簽章錯誤的發佈 ---------------------------------------------------------
sums() {
    (cd "$1" && rm -f SHA256SUMS SHA256SUMS.sig && for f in pulse-*; do sha256sum "$f"; done >SHA256SUMS)
}
T1=$WORK/dist/$NEXT_VERSION-test.1
T9=$WORK/dist/$NEXT_VERSION-test.9
say "簽章錯誤的發佈：evil-a（沒有人信任的金鑰）、$NEXT_VERSION-test.9（只有 Kairos 信任的金鑰）"
rm -rf "$WORK/dist/evil-a" "$T9"
mkdir -p "$WORK/dist/evil-a" "$T9"
cp "$T1"/pulse-* "$T1/SHA256SUMS" "$WORK/dist/evil-a/"
sh "$REPO/packaging/sign-release.sh" sign "$WORK/dist/evil-a" "$KEYS/evil-a" >/dev/null
for f in "$T1"/pulse-"$NEXT_VERSION"-test.1-*-linux-x86_64.tar.gz "$T1"/pulse-"$NEXT_VERSION"-test.1-*-linux-x86_64.deb \
    "$T1"/pulse-"$NEXT_VERSION"-test.1-*-linux-x86_64.rpm "$T1"/pulse-"$NEXT_VERSION"-test.1-*-release.json; do
    n=$(basename "$f" | sed "s/$NEXT_RE-test\\.1/$NEXT_VERSION-test.9/")
    case "$n" in
    *-release.json) sed "s/\"version\":\"$NEXT_RE-test\\.1\"/\"version\":\"$NEXT_VERSION-test.9\"/" "$f" >"$T9/$n" ;;
    *) cp "$f" "$T9/$n" ;;
    esac
done
sums "$T9"
sh "$REPO/packaging/sign-release.sh" sign "$T9" "$KEYS/evil-b" >/dev/null

say "完成"
for d in "$WORK"/dist/*/; do
    echo "  $(basename "$d")：$(cd "$d" && printf '%s ' *)" >&2
done
