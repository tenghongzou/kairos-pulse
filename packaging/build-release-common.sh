# shellcheck shell=sh
# Kairos／Pulse 發佈包打包的共用實作。
#
# 不要直接執行：由 kairos/packaging/build-release.sh、pulse/packaging/build-release.sh
# 設定好產品參數後以「.」載入。呼叫端必須先設定：
#   SCRIPT_DIR       產品的 packaging/ 目錄（絕對路徑）
#   NAME             執行檔名稱（Cargo 的 bin 名稱，也是發佈包名稱的開頭）
#   EXAMPLE_SRC      產品目錄中的範例設定；EXAMPLE_DST 為放進發佈包時的檔名
#   UNIT_SRC         systemd unit 範本（相對於產品目錄；只放進 Linux 的發佈包）
#   EXTRA_SRC        其他放進發佈包根目錄的檔案（相對於產品目錄，以空白分隔，可為空）
#   DEFAULT_TARGETS  預設建置的目標（Rust target，以空白分隔；可用環境變數 TARGETS 覆寫）
#   EXTRA_DIST       （可為空）直接放到輸出目錄的檔案，格式「<相對於產品目錄的來源>:<輸出檔名>」
#   RELEASE_META     （可為空）發佈清單的手動欄位（相對於產品目錄，例如 packaging/release-meta.toml）：
#                    設定時以 packaging/release-manifest.sh 產生 <名稱>-<版本>-<commit>-release.json
#                    （REMOTE.md 第 4.1.3 節）放到輸出目錄並列進 SHA256SUMS；-dirty 的建置不產生
#
# 在 docker 的 rust alpine image（以 @sha256 digest 固定）內建置 release 執行檔並打包。每個目標的產出
# （<基本名稱> = <名稱>-<版本>-<commit>[-dirty]-<os>-<arch>）：
#
#   | 目標 | 產出 |
#   |---|---|
#   | x86_64-unknown-linux-musl  | <基本名稱>.tar.gz、.deb、.rpm、.cdx.json（os-arch = linux-x86_64） |
#   | aarch64-unknown-linux-musl | 同上（linux-aarch64；cargo-zigbuild＋zig 交叉編譯，以 qemu-aarch64 執行 --version） |
#   | x86_64-pc-windows-gnu      | <基本名稱>.zip、.msi、.cdx.json（windows-x86_64；mingw-w64 交叉編譯、msitools 的 wixl 產生 MSI；
#   |                            | 無法在容器內執行，--version 的檢查改為確認是 PE 執行檔） |
#
#   <DIST_DIR>/SHA256SUMS   兩個產品共用的合併清單：只更新本次產出檔案的那幾列，
#                           保留其他產品的列；最後以 sha256sum -c 確認整份清單與目錄內容一致
#
# deb／rpm 以 nfpm（固定版本，下載後以 SHA-256 核對）依產品的 packaging/pkg/nfpm.yaml.in 產生：
# 版面與 tar.gz 相同（/opt/<名稱>），另有 systemd unit（/usr/lib/systemd/system）、/usr/bin/<名稱> 的
# 符號連結、預設設定（config|noreplace，升級不覆寫）與 packaging/pkg/ 的安裝前後腳本。
# MSI 以產品的 packaging/windows/<名稱>.wxs 產生（只有 Pulse）。
#
# 建置來源不是工作目錄，而是以 git -c core.autocrlf=false archive 匯出的 commit 內容（repo 中
# 儲存的 LF 版本），經 stdin 傳進容器。Windows 上 core.autocrlf=true 的工作目錄是 CRLF，直接掛載
# 會讓 tar 內的文件與 Pulse 以 include_str! 嵌入的 config.toml 和 Linux 上建置的不同。
# 工作目錄有未提交的變更時改用 git stash create 的結果（含已追蹤檔案的修改，不改變工作目錄與
# stash 清單；未追蹤的檔案不會進入建置），產物標為 -dirty。
#
# 可用的環境變數：
#   DIST_DIR      輸出目錄，預設為 repo 根目錄的 dist/（兩個產品共用，SHA256SUMS 才能合併）
#   TARGETS       要建置的目標（以空白分隔），預設為產品的 DEFAULT_TARGETS；例如只建 x86_64 Linux：
#                 TARGETS=x86_64-unknown-linux-musl
#   PACKAGES      Linux 目標要產生的套件格式（預設 "deb rpm"；空字串＝只產生 tar.gz）
#   BUILD_JOBS    容器內 cargo 的平行工作數（CARGO_BUILD_JOBS），預設 2；只影響建置時間，不影響產物
#   BUILD_VOLUME  CARGO_TARGET_DIR 使用的 docker volume，預設 sync-build-<repo 目錄名稱>；
#                 可重現性檢查以全新的 volume 指定，確保不是沿用上次的建置產物
#   SIGNING_KEY   簽章私鑰路徑（repo 之外）。有設定時打包後以 packaging/sign-release.sh 簽署
#                 SHA256SUMS；沒有設定時，因 SHA256SUMS 已改變，刪除舊的 SHA256SUMS.sig
#
# 發佈包內容（一層 <基本名稱>/ 目錄）：執行檔（755）、範例設定、README.md、PROTOCOL.md、
# CHANGELOG.md、INSTALL.md、THIRD-PARTY-LICENSES、<名稱>.cdx.json、systemd unit（Linux）與
# EXTRA_SRC（其餘皆 644）。可重現：tar 的擁有者一律為 0:0、檔案依名稱排序；tar、zip、deb、rpm、MSI
# 內所有檔案的時間戳記固定為 commit 時間（SOURCE_DATE_EPOCH；nfpm 的 mtime、zip 以 TZ=UTC、
# MSI 以 libfaketime）；MSI 的 PackageCode 與 ProductCode 由版本與 commit 推導；Windows 執行檔以
# --no-insert-timestamp 連結（PE 標頭沒有建置時間）並去掉符號表。PROTOCOL.md 取自兩個產品共用的唯一規格，
# CHANGELOG.md 取自 repo 根目錄。
#
# THIRD-PARTY-LICENSES（商業審計 B2）與 SBOM（D4）每個目標各自在容器內依 Cargo.lock 重新產生，
# 不會過期（Windows 的依賴與 Linux 不同，例如 windows-service、windows-sys）：
#   - cargo-about（設定 packaging/about.toml，targets 換成該目標；範本 packaging/about.hbs）以
#     --frozen 離線執行，授權全文只取自 crate 原始碼內的檔案；依賴出現 about.toml 未允許的授權時建置失敗。
#   - cargo-cyclonedx 產生 CycloneDX 1.5 JSON（只含目標平台、非建置期的依賴）；
#     SOURCE_DATE_EPOCH 讓 metadata.timestamp 固定為 commit 時間。
#   兩個工具與 cargo-zigbuild 以固定版本 cargo install --locked 安裝在 BUILD_VOLUME 的 tools/ 下，之後重複使用；
#   nfpm 下載到同一處。
#   已知限制：cargo metadata 會把只由弱依賴特性（dep?/feature）提到、實際未啟用的可選依賴
#   列入 resolve，SBOM 因此可能多列少數沒有編譯進執行檔的 crate（0.2.0 為 toml 的 indexmap、
#   toml_writer 與 indexmap 的 hashbrown、equivalent）；cargo-auditable 有同樣的限制。
#   THIRD-PARTY-LICENSES 由 cargo-about 依實際的 feature 解析產生，不受影響。

# ---- 建置環境（兩個產品共用；更新方式見 RELEASING.md「更新建置 image」）-----
# rust:1.99-alpine（Alpine 3.24.2、rustc 1.99.0）的 multi-arch index digest，2026-10-12 查得。
# 以 digest 固定後，rustc、gcc、musl-dev 等 image 內的檔案都不會再變動；tag 只供閱讀。
IMAGE=rust:1.99-alpine@sha256:0cce0a5e0e8ba67b455257a3a02a1d99005f382748789d6464460028810f1627
# image 內沒有、需要從 Alpine 套件庫安裝的套件；版本鎖定為 image 的 Alpine 版本（v3.24）當時的版本。
# Alpine 套件庫只保留最新的修訂版：任一版本被取代後 apk 會失敗，需依 RELEASING.md 一併更新。
#   tar：GNU tar（busybox tar 不支援 --sort、--owner 等可重現打包所需的選項）
#   zig、qemu-aarch64：aarch64 musl 的交叉編譯（cargo-zigbuild）與 --version 檢查
#   mingw-w64-gcc：x86_64-pc-windows-gnu 的連結器與 C 編譯器（ring、SQLite）
#   msitools、libfaketime、zip：Windows 的 MSI（wixl、msibuild）與 zip
APK_BASE="tar=1.35-r5"
APK_AARCH64="zig=0.16.0-r1 qemu-aarch64=11.0.3-r0"
APK_WINDOWS="mingw-w64-gcc=15.2.0-r0 msitools=0.106-r0 libfaketime=0.9.13-r0 zip=3.0-r13"
HOST_TRIPLE=x86_64-unknown-linux-musl
RUST_TOOLCHAIN=1.99.0
CARGO_VOLUME=sync-cargo
# 產生 THIRD-PARTY-LICENSES 與 SBOM 的工具、aarch64 的交叉編譯（cargo install --locked，固定版本）
CARGO_ABOUT_VERSION=0.9.2
CARGO_CYCLONEDX_VERSION=0.5.9
CARGO_ZIGBUILD_VERSION=0.23.4
# deb／rpm：nfpm（goreleaser/nfpm 的發佈檔，2026-06-20；SHA-256 取自該版的 checksums.txt 並與下載檔核對）
NFPM_VERSION=2.47.0
NFPM_SHA256=0660ca602b2d2d2ae4781a06c692b3eeb9d437ffea05b831d76e41f4a3188783
PROTOCOL_SRC=crates/kairos-proto/PROTOCOL.md  # 協定規格（相對於 repo 根目錄）
ABOUT_CONFIG=packaging/about.toml              # cargo-about 設定（相對於 repo 根目錄）
ABOUT_TEMPLATE=packaging/about.hbs             # THIRD-PARTY-LICENSES 範本（相對於 repo 根目錄）
CHANGELOG_SRC=CHANGELOG.md                     # 變更紀錄與升級步驟（相對於 repo 根目錄）
# ---------------------------------------------------------------------------

die() {
    echo "錯誤：$*" >&2
    exit 1
}

# 主機路徑轉成 docker -v 可用的形式：Git Bash 下 pwd -W 會給 F:/... 形式，其他環境用一般的 pwd
host_path() {
    (cd "$1" && { pwd -W 2>/dev/null || pwd; })
}

# ---- 前置檢查 -------------------------------------------------------------
command -v git >/dev/null 2>&1 || die "找不到 git"
command -v docker >/dev/null 2>&1 || die "找不到 docker，請先安裝 Docker（Windows 為 Docker Desktop）"
docker info >/dev/null 2>&1 || die "無法連線到 docker daemon，請確認 Docker 已啟動且目前使用者有權限"

REPO=$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel) || die "$SCRIPT_DIR 不在 git repo 中"
# 產品目錄（packaging/ 的上一層）相對於 repo 根目錄的路徑，例如 kairos；以 git 計算，避免
# Git Bash 下 pwd（/f/...）與 git（F:/...）的路徑格式不同
PKG_SUBDIR=$(cd "$SCRIPT_DIR/.." && git rev-parse --show-prefix) || die "無法取得產品目錄"
PKG_SUBDIR=${PKG_SUBDIR%/}
[ -n "$PKG_SUBDIR" ] || die "產品目錄不能是 repo 根目錄"
cd "$REPO" || exit 1
[ -f Cargo.lock ] || die "缺少 Cargo.lock（workspace 根目錄）"
[ -f "$PROTOCOL_SRC" ] || die "缺少 $PROTOCOL_SRC（協定規格）"
[ -f "$CHANGELOG_SRC" ] || die "缺少 $CHANGELOG_SRC（變更紀錄）"
[ -f "$ABOUT_CONFIG" ] || die "缺少 $ABOUT_CONFIG（cargo-about 設定）"
[ -f "$ABOUT_TEMPLATE" ] || die "缺少 $ABOUT_TEMPLATE（THIRD-PARTY-LICENSES 範本）"

# shellcheck disable=SC2086 # EXTRA_SRC 刻意以空白分隔成多個檔案
for f in Cargo.toml README.md "$EXAMPLE_SRC" packaging/INSTALL.md "$UNIT_SRC" ${EXTRA_SRC:-}; do
    [ -f "$PKG_SUBDIR/$f" ] || die "缺少 $PKG_SUBDIR/$f"
done

TARGETS=${TARGETS:-$DEFAULT_TARGETS}
PACKAGES=${PACKAGES-deb rpm}
BUILD_JOBS=${BUILD_JOBS:-2}
APK_PACKAGES=$APK_BASE
for t in $TARGETS; do
    case "$t" in
    x86_64-unknown-linux-musl) ;;
    aarch64-unknown-linux-musl) APK_PACKAGES="$APK_PACKAGES $APK_AARCH64" ;;
    x86_64-pc-windows-gnu)
        APK_PACKAGES="$APK_PACKAGES $APK_WINDOWS"
        for f in "$NAME.wxs" "$NAME.default.toml"; do
            [ -f "$PKG_SUBDIR/packaging/windows/$f" ] || die "缺少 $PKG_SUBDIR/packaging/windows/$f（Windows 目標的 MSI 需要）"
        done
        ;;
    *) die "不支援的目標 $t（x86_64-unknown-linux-musl、aarch64-unknown-linux-musl、x86_64-pc-windows-gnu）" ;;
    esac
done
for p in $PACKAGES; do
    case "$p" in
    deb | rpm) ;;
    *) die "不支援的套件格式 $p（deb、rpm）" ;;
    esac
done
if [ -n "$PACKAGES" ]; then
    for f in nfpm.yaml.in "$NAME.default.toml" preinstall.sh postinstall.sh preremove.sh postremove.sh; do
        [ -f "$PKG_SUBDIR/packaging/pkg/$f" ] || die "缺少 $PKG_SUBDIR/packaging/pkg/$f（deb／rpm 需要）"
    done
fi
for e in ${EXTRA_DIST:-}; do
    [ -f "$PKG_SUBDIR/${e%%:*}" ] || die "缺少 $PKG_SUBDIR/${e%%:*}"
done
[ -z "${RELEASE_META:-}" ] || [ -f "$PKG_SUBDIR/$RELEASE_META" ] || die "缺少 $PKG_SUBDIR/$RELEASE_META"

# ---- 版本與 commit ---------------------------------------------------------
# 版本取自產品 Cargo.toml [package] 的 version（第一個 version 欄位）；--version 也是這個值
VERSION=$(sed -n 's/^version[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' "$PKG_SUBDIR/Cargo.toml" | head -n 1)
echo "$VERSION" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$' \
    || die "$PKG_SUBDIR/Cargo.toml 的 version「$VERSION」不是 SemVer（MAJOR.MINOR.PATCH[-pre]）"

COMMIT=$(git rev-parse --short HEAD)
DIRTY=""
if [ -n "$(git status --porcelain --untracked-files=normal)" ]; then
    DIRTY="-dirty"
    echo "警告：工作目錄有未提交的變更，檔名與 --version 會加上 -dirty，不可作為正式發佈" >&2
fi
# 發佈 tag（<名稱>-v<版本>，見 RELEASING.md）若指向 HEAD，版本必須與 Cargo.toml 一致
for t in $(git tag --points-at HEAD "$NAME-v*"); do
    [ "$t" = "$NAME-v$VERSION" ] || die "HEAD 上的 tag $t 與 $PKG_SUBDIR/Cargo.toml 的版本 $VERSION 不一致"
done
# 建置來源（見檔案開頭）
TREEISH=HEAD
if [ -n "$DIRTY" ]; then
    TREEISH=$(git stash create)
    TREEISH=${TREEISH:-HEAD}
    if [ -n "$(git ls-files --others --exclude-standard)" ]; then
        echo "警告：未追蹤的檔案不會進入建置（請先 git add 或提交）" >&2
    fi
fi
# 所有檔案的時間戳記固定為該 commit 的時間，同一份原始碼重複打包時內容一致
SOURCE_DATE_EPOCH=$(git log -1 --format=%ct HEAD)
BUILD_COMMIT="$COMMIT$DIRTY"

# 發佈清單（REMOTE.md 第 4.1.3 節）：在主機上以 git 歷史產生（unit 的歷來雜湊），由容器寫進輸出目錄
MANIFEST_FILE=""
MANIFEST_JSON=""
if [ -n "${RELEASE_META:-}" ]; then
    if [ -n "$DIRTY" ]; then
        echo "注意：-dirty 的建置不產生發佈清單" >&2
    else
        MANIFEST_FILE="$NAME-$VERSION-$COMMIT-release.json"
        MANIFEST_JSON=$(sh packaging/release-manifest.sh "$NAME" "$VERSION" "$COMMIT" \
            "$SOURCE_DATE_EPOCH" "$PKG_SUBDIR/$RELEASE_META" "$PKG_SUBDIR/$UNIT_SRC" HEAD) ||
            die "無法產生發佈清單（$PKG_SUBDIR/$RELEASE_META）"
    fi
fi

# ---- 輸出目錄與 docker volume ----------------------------------------------
DIST_DIR=${DIST_DIR:-$REPO/dist}
mkdir -p "$DIST_DIR"
HOST_DIST=$(host_path "$DIST_DIR")
BUILD_VOLUME=${BUILD_VOLUME:-sync-build-$(basename "$REPO")}

echo "==> 建置 $NAME $VERSION（$BUILD_COMMIT）"
echo "    image=$IMAGE"
echo "    targets=$TARGETS packages=${PACKAGES:-（無）} 輸出=$HOST_DIST"
echo "    volumes: $BUILD_VOLUME（target）、$CARGO_VOLUME（registry）"

# 容器內執行的腳本（以 sh -c 傳入；變數用 -e 傳入，這裡的 $ 不在主機展開）
# shellcheck disable=SC2016
INNER='
set -eu
# 建置來源由 stdin 傳入（git archive 的 tar）。-m：檔案時間設為解壓當下，cargo 以 mtime 判斷
# 是否需要重新編譯，沿用 commit 時間可能讓切換到較舊的 commit 時誤用上次的建置產物
mkdir -p /src
tar -x -m -f - -C /src
# 網路暫時失敗（套件庫的 TLS 錯誤等）時重試；版本不存在時三次都會失敗
retry() {
    n=1
    until "$@"; do
        [ "$n" -lt 3 ] || return 1
        echo "注意：$1 失敗，10 秒後重試（第 $n 次）" >&2
        n=$((n + 1))
        sleep 10
    done
}
# shellcheck disable=SC2086
retry apk add --no-cache $APK_PACKAGES >/dev/null || {
    echo "錯誤：apk 無法安裝 $APK_PACKAGES（Alpine 套件庫可能已更新其中的套件），請依 RELEASING.md 更新 image digest 與套件版本" >&2
    exit 1
}

# 授權清單、SBOM、交叉編譯的工具：已安裝同版本時 cargo install 會直接略過
TOOLS=/build/tools
TOOL_LIST="cargo-about@$CARGO_ABOUT_VERSION --features cli
cargo-cyclonedx@$CARGO_CYCLONEDX_VERSION"
case " $TARGETS " in *" aarch64-unknown-linux-musl "*)
    TOOL_LIST="$TOOL_LIST
cargo-zigbuild@$CARGO_ZIGBUILD_VERSION" ;;
esac
echo "$TOOL_LIST" | while read -r tool; do
    # shellcheck disable=SC2086
    CARGO_TARGET_DIR=/build/tools-target cargo install --quiet --locked --root "$TOOLS" $tool
done
export PATH="$TOOLS/bin:$PATH"

# nfpm：固定版本的發佈檔，核對 SHA-256 後解開到 tools/
NFPM="$TOOLS/nfpm-$NFPM_VERSION/nfpm"
if [ -n "$PACKAGES" ] && [ ! -x "$NFPM" ]; then
    mkdir -p "$TOOLS/nfpm-$NFPM_VERSION"
    retry wget -q -O /tmp/nfpm.tar.gz "https://github.com/goreleaser/nfpm/releases/download/v$NFPM_VERSION/nfpm_${NFPM_VERSION}_Linux_x86_64.tar.gz"
    echo "$NFPM_SHA256  /tmp/nfpm.tar.gz" | sha256sum -c - >/dev/null || {
        echo "錯誤：nfpm $NFPM_VERSION 的 SHA-256 不符" >&2
        exit 1
    }
    tar -xzf /tmp/nfpm.tar.gz -C "$TOOLS/nfpm-$NFPM_VERSION" nfpm
fi

for T in $TARGETS; do
    [ "$T" = "$HOST_TRIPLE" ] || retry rustup target add "$T" >/dev/null
done

# 在成員目錄內執行 cargo build 只會建置該成員；Cargo.lock 取自 workspace 根目錄（/src）
SRC="/src/$PKG_SUBDIR"
# cargo-about／cargo-cyclonedx 會讀取所有平台的依賴資訊，先下載 Cargo.lock 中的全部 crate
(cd /src && cargo fetch --locked --quiet)

# SBOM 用的 workspace 複本（見下方 SBOM 的說明），所有目標共用
SBOM_SRC=/work/kairos-pulse
rm -rf "$SBOM_SRC"
mkdir -p "$SBOM_SRC"
tar -C /src -cf - . | tar -C "$SBOM_SRC" -xf -
sed -i "s|^members = .*|members = [\"$PKG_SUBDIR\", \"crates/*\"]|" "$SBOM_SRC/Cargo.toml"
grep -q "^members = \[\"$PKG_SUBDIR\", \"crates/\*\"\]" "$SBOM_SRC/Cargo.toml" || {
    echo "錯誤：無法在 SBOM 用的 workspace 複本中縮減 members（Cargo.toml 格式改變？）" >&2
    exit 1
}

# rpm 的 payload（cpio）起點：lead 96 bytes → 簽章 header（補齊到 8 的倍數）→ 主 header。
# header 為 magic(3)＋版本(1)＋保留(4)＋索引數(4, big-endian)＋資料長度(4)＋索引(16×N)＋資料
be32() { od -An -tu1 -v -j "$2" -N4 "$1" | awk "{print ((\$1*256+\$2)*256+\$3)*256+\$4}"; }
rpm_payload_offset() {
    off=96
    off=$((off + 16 + 16 * $(be32 "$1" $((off + 8))) + $(be32 "$1" $((off + 12)))))
    off=$(((off + 7) / 8 * 8))
    echo $((off + 16 + 16 * $(be32 "$1" $((off + 8))) + $(be32 "$1" $((off + 12)))))
}
MTIME=$(date -u -d "@$SOURCE_DATE_EPOCH" +%Y-%m-%dT%H:%M:%SZ)
FAKE_NOW=$(date -u -d "@$SOURCE_DATE_EPOCH" "+%Y-%m-%d %H:%M:%S")
# 依字串推導固定的 GUID（MSI 的 ProductCode／PackageCode）：SHA-256 的前 128 位元，標成第 4 版
guid_of() {
    h=$(printf "%s" "$1" | sha256sum | cut -c1-32 | tr "a-f" "A-F")
    printf "%s-%s-4%s-8%s-%s" "$(echo "$h" | cut -c1-8)" "$(echo "$h" | cut -c9-12)" \
        "$(echo "$h" | cut -c14-16)" "$(echo "$h" | cut -c18-20)" "$(echo "$h" | cut -c21-32)"
}
NEW_FILES=""

for T in $TARGETS; do
    case "$T" in
    x86_64-unknown-linux-musl) OS=linux; ARCH=x86_64; GOARCH=amd64; EXE="" ;;
    aarch64-unknown-linux-musl) OS=linux; ARCH=aarch64; GOARCH=arm64; EXE="" ;;
    x86_64-pc-windows-gnu) OS=windows; ARCH=x86_64; GOARCH=""; EXE=.exe ;;
    esac
    PKG="$NAME-$VERSION-$BUILD_COMMIT-$OS-$ARCH"
    echo "==> [$T] cargo build（$PKG）"
    cd "$SRC"
    case "$T" in
    aarch64-unknown-linux-musl)
        cargo zigbuild --release --locked --target "$T" ;;
    x86_64-pc-windows-gnu)
        # 可重現：PE 標頭不寫建置時間；去掉符號表（rustc 為 raw-dylib 產生的匯入程式庫放在隨機命名的
        # 暫存目錄 rustcXXXXXX，目錄名稱會進入 COFF 符號表，每次建置都不同）
        CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER=x86_64-w64-mingw32-gcc \
        CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUSTFLAGS="-C link-arg=-Wl,--no-insert-timestamp -C strip=symbols" \
            cargo build --release --locked --target "$T" ;;
    *)
        cargo build --release --locked --target "$T" ;;
    esac
    BIN="$CARGO_TARGET_DIR/$T/release/$NAME$EXE"
    [ -f "$BIN" ] || { echo "找不到建置產物 $BIN" >&2; exit 1; }

    EXPECTED="$NAME $VERSION ($BUILD_COMMIT)"
    case "$T" in
    x86_64-pc-windows-gnu)
        # 容器內無法執行 Windows 執行檔：確認是 PE（MZ）且內嵌的版本字串與檔名一致
        [ "$(head -c 2 "$BIN")" = MZ ] || { echo "錯誤：$BIN 不是 PE 執行檔" >&2; exit 1; }
        grep -aq "$EXPECTED" "$BIN" || { echo "錯誤：$BIN 中找不到版本字串「$EXPECTED」" >&2; exit 1; }
        echo "    （Windows 執行檔：已確認 PE 格式與內嵌的「$EXPECTED」，未執行）"
        ;;
    *)
        RUNNER=""
        [ "$T" = aarch64-unknown-linux-musl ] && RUNNER=qemu-aarch64
        ACTUAL=$($RUNNER "$BIN" --version) || {
            echo "錯誤：$NAME --version 執行失敗（發佈包必須能以 --version 確認版本與 commit）" >&2
            exit 1
        }
        echo "    $ACTUAL"
        [ "$ACTUAL" = "$EXPECTED" ] || {
            echo "錯誤：--version 輸出「$ACTUAL」，預期「$EXPECTED」" >&2
            exit 1
        }
        ;;
    esac

    STAGE=/tmp/stage
    rm -rf "$STAGE"
    mkdir -p "$STAGE/$PKG"
    chmod 755 "$STAGE/$PKG"
    install -m 755 "$BIN"                    "$STAGE/$PKG/$NAME$EXE"
    install -m 644 "$SRC/$EXAMPLE_SRC"       "$STAGE/$PKG/$EXAMPLE_DST"
    install -m 644 "$SRC/README.md"          "$STAGE/$PKG/README.md"
    install -m 644 "/src/$PROTOCOL_SRC"     "$STAGE/$PKG/PROTOCOL.md"
    install -m 644 "/src/$CHANGELOG_SRC"    "$STAGE/$PKG/CHANGELOG.md"
    install -m 644 "$SRC/packaging/INSTALL.md" "$STAGE/$PKG/INSTALL.md"
    [ "$OS" = windows ] || install -m 644 "$SRC/$UNIT_SRC" "$STAGE/$PKG/$(basename "$UNIT_SRC")"
    for f in $EXTRA_SRC; do
        case "$OS:$f" in windows:*.service | windows:*.timer) continue ;; esac
        install -m 644 "$SRC/$f" "$STAGE/$PKG/$(basename "$f")"
    done

    echo "==> [$T] THIRD-PARTY-LICENSES（cargo-about $CARGO_ABOUT_VERSION）"
    sed "s|^targets = .*|targets = [\"$T\"]|" "/src/$ABOUT_CONFIG" > /tmp/about.toml
    grep -q "^targets = \[\"$T\"\]" /tmp/about.toml || {
        echo "錯誤：無法在 $ABOUT_CONFIG 中替換 targets" >&2
        exit 1
    }
    (cd /src && "$TOOLS/bin/cargo-about" generate --frozen \
        -m "$PKG_SUBDIR/Cargo.toml" -c /tmp/about.toml \
        -o /tmp/THIRD-PARTY-LICENSES "$ABOUT_TEMPLATE")
    grep -q "^Used by:" /tmp/THIRD-PARTY-LICENSES || {
        echo "錯誤：產生的 THIRD-PARTY-LICENSES 沒有任何授權條目" >&2
        exit 1
    }
    install -m 644 /tmp/THIRD-PARTY-LICENSES "$STAGE/$PKG/THIRD-PARTY-LICENSES"
    sed -n "/^Overview/,/^$/p" /tmp/THIRD-PARTY-LICENSES | sed "s/^/    /"

    echo "==> [$T] SBOM（cargo-cyclonedx $CARGO_CYCLONEDX_VERSION）"
    # cargo-cyclonedx 把結果寫在 Cargo.toml 旁邊，因此在另一個固定路徑的 workspace 複本執行
    # （SBOM 的 bom-ref 含此路徑，固定才能重現），不影響 /src 的建置。
    # cargo metadata 會合併整個 workspace 的 feature（例如 Kairos 的 tokio "full" 會讓 Pulse 的
    # SBOM 多出 parking_lot），因此複本的 workspace 只保留本產品與共用 crate，與實際建置一致。
    SBOM="$SBOM_SRC/$PKG_SUBDIR/${NAME}_bin.cdx.json"
    rm -f "$SBOM"
    (cd "$SBOM_SRC/$PKG_SUBDIR" && CARGO_NET_OFFLINE=true "$TOOLS/bin/cargo-cyclonedx" cyclonedx \
        --quiet --format json --spec-version 1.5 --target "$T" \
        --describe binaries --no-build-deps)
    [ -s "$SBOM" ] || { echo "錯誤：找不到 cargo-cyclonedx 的輸出 $SBOM" >&2; exit 1; }
    grep -q "\"version\": \"$VERSION\"" "$SBOM" || {
        echo "錯誤：SBOM 中沒有版本 $VERSION" >&2
        exit 1
    }
    # 縮減 members 後 cargo 會從複本的 Cargo.lock 移除用不到的套件，但不得改變任何版本：
    # 複本中的每個「名稱 版本」都必須出現在原本的 Cargo.lock
    lock_pairs() { awk "/^name = /{n=\$3} /^version = /{print n, \$3}" "$1" | sort -u; }
    lock_pairs /src/Cargo.lock > /tmp/lock-src
    lock_pairs "$SBOM_SRC/Cargo.lock" > /tmp/lock-sbom
    if grep -Fxv -f /tmp/lock-src /tmp/lock-sbom; then
        echo "錯誤：產生 SBOM 時 Cargo.lock 的版本被改變（上面列出）" >&2
        exit 1
    fi
    install -m 644 "$SBOM" "$STAGE/$PKG/$NAME.cdx.json"
    echo "    $(grep -c "\"bom-ref\"" "$SBOM") 個 bom-ref"
    # SBOM 另外放一份在發佈包外，掃描工具（Dependency-Track 等）不必先解壓
    install -m 644 "$STAGE/$PKG/$NAME.cdx.json" "/out/$PKG.cdx.json"
    NEW_FILES="$NEW_FILES $PKG.cdx.json"

    # 所有檔案的時間戳記固定為 commit 時間（zip、MSI 的 CAB 取檔案的 mtime）
    find "$STAGE/$PKG" -exec touch -h -d "@$SOURCE_DATE_EPOCH" {} +

    if [ "$OS" = linux ]; then
        cd "$STAGE"
        tar --owner=0 --group=0 --numeric-owner --sort=name \
            --mtime="@$SOURCE_DATE_EPOCH" -cf - "$PKG" | gzip -n -9 > "/out/$PKG.tar.gz.tmp"
        mv "/out/$PKG.tar.gz.tmp" "/out/$PKG.tar.gz"
        NEW_FILES="$NEW_FILES $PKG.tar.gz"
        echo "==> [$T] tar 內容"
        tar tzvf "/out/$PKG.tar.gz"

        # deb／rpm（nfpm）：文件與範例逐一列出（執行檔、unit 與預設設定另外寫在範本中）
        if [ -n "$PACKAGES" ]; then
            PKGDIR="$SRC/packaging/pkg"
            : > /tmp/docs.yaml
            for f in $(cd "$STAGE/$PKG" && ls | LC_ALL=C sort); do
                case "$f" in "$NAME" | "$NAME.service") continue ;; esac
                printf "  - src: \"%s\"\n    dst: /opt/%s/%s\n    file_info: {mode: 0644, owner: root, group: root}\n" \
                    "$STAGE/$PKG/$f" "$NAME" "$f" >> /tmp/docs.yaml
            done
            sed -e "s|@ARCH@|$GOARCH|g" -e "s|@VERSION@|$VERSION|g" -e "s|@MTIME@|$MTIME|g" \
                -e "s|@STAGE@|$STAGE/$PKG|g" -e "s|@PKGDIR@|$PKGDIR|g" \
                -e "/@DOCS@/r /tmp/docs.yaml" -e "/@DOCS@/d" "$PKGDIR/nfpm.yaml.in" > /tmp/nfpm.yaml
            for p in $PACKAGES; do
                echo "==> [$T] $p（nfpm $NFPM_VERSION）"
                "$NFPM" package --config /tmp/nfpm.yaml --packager "$p" --target "/out/$PKG.$p.tmp" >/dev/null
                mv "/out/$PKG.$p.tmp" "/out/$PKG.$p"
                NEW_FILES="$NEW_FILES $PKG.$p"
                echo "    $PKG.$p（$(wc -c < "/out/$PKG.$p") bytes）"
                # 套件中的 systemd unit 必須與發佈包的 unit（$UNIT_SRC）逐位元相同：兩者只有一個來源，
                # 以這個檢查防止日後有人另外維護一份（例如 pulse.service 的序號複製只改了其中一份）
                UNIT_CHECK=/tmp/unit-check
                rm -rf "$UNIT_CHECK"
                mkdir -p "$UNIT_CHECK"
                case "$p" in
                deb)
                    ar p "/out/$PKG.$p" data.tar.gz |
                        tar -xzf - -C "$UNIT_CHECK" --wildcards "*usr/lib/systemd/system/$NAME.service"
                    ;;
                rpm)
                    tail -c +$(($(rpm_payload_offset "/out/$PKG.$p") + 1)) "/out/$PKG.$p" | gunzip |
                        (cd "$UNIT_CHECK" && cpio -idm 2>/dev/null)
                    ;;
                esac
                if ! cmp "$SRC/$UNIT_SRC" "$UNIT_CHECK/usr/lib/systemd/system/$NAME.service"; then
                    echo "錯誤：$PKG.$p 中的 /usr/lib/systemd/system/$NAME.service 與 $UNIT_SRC 不同" >&2
                    exit 1
                fi
                echo "    unit 與 $UNIT_SRC 相同"
            done
        fi
    else
        # Windows：zip（檔案依名稱排序；DOS 時間以 UTC 記錄）與 MSI
        cd "$STAGE"
        rm -f "/out/$PKG.zip.tmp"
        find "$PKG" | LC_ALL=C sort | TZ=UTC zip -X -D -9 -q "/out/$PKG.zip.tmp" -@
        mv "/out/$PKG.zip.tmp" "/out/$PKG.zip"
        NEW_FILES="$NEW_FILES $PKG.zip"
        echo "==> [$T] zip 內容"
        unzip -l "/out/$PKG.zip" | sed "s/^/    /"

        echo "==> [$T] msi（msitools wixl）"
        MSI_SRC=/tmp/msi-src
        rm -rf "$MSI_SRC"
        mkdir -p "$MSI_SRC"
        cp -p "$STAGE/$PKG"/* "$MSI_SRC/"
        install -m 644 "$SRC/packaging/windows/$NAME.default.toml" "$MSI_SRC/$NAME.toml"
        touch -d "@$SOURCE_DATE_EPOCH" "$MSI_SRC"/*
        # MSI 的 ProductVersion 只能是數字（去掉預發佈標記）
        MSI_VERSION=${VERSION%%-*}
        PRODUCT_CODE=$(guid_of "$NAME-msi-product-$MSI_VERSION")
        PACKAGE_CODE=$(guid_of "$NAME-msi-package-$VERSION-$BUILD_COMMIT")
        sed -e "s|@VERSION@|$MSI_VERSION|g" -e "s|@PRODUCT_CODE@|$PRODUCT_CODE|g" -e "s|@SRC@|$MSI_SRC|g" \
            "$SRC/packaging/windows/$NAME.wxs" > /tmp/product.wxs
        FAKE="NO_FAKE_STAT=1 LD_PRELOAD=/usr/lib/faketime/libfaketime.so.1 FAKETIME_DONT_FAKE_MONOTONIC=1"
        env $FAKE FAKETIME="@$FAKE_NOW" wixl -a x64 -o "/out/$PKG.msi.tmp" /tmp/product.wxs
        env $FAKE FAKETIME="@$FAKE_NOW" msibuild "/out/$PKG.msi.tmp" \
            -s "Pulse" "Kairos-Pulse" "x64;1033" "{$PACKAGE_CODE}"
        mv "/out/$PKG.msi.tmp" "/out/$PKG.msi"
        NEW_FILES="$NEW_FILES $PKG.msi"
        msiinfo suminfo "/out/$PKG.msi" | sed "s/^/    /"
        msiinfo export "/out/$PKG.msi" File | cut -f1,3,4 | sed "s/^/    /"
    fi
done

for e in $EXTRA_DIST; do
    install -m 644 "$SRC/${e%%:*}" "/out/${e#*:}"
    NEW_FILES="$NEW_FILES ${e#*:}"
done

# 發佈清單（主機上產生的一行 JSON）
if [ -n "$MANIFEST_FILE" ]; then
    printf "%s\n" "$MANIFEST_JSON" > "/out/$MANIFEST_FILE"
    chmod 644 "/out/$MANIFEST_FILE"
    NEW_FILES="$NEW_FILES $MANIFEST_FILE"
fi

# 合併版 SHA256SUMS：去掉本次產出檔案的舊列與已不存在的檔案，加上新列，依檔名排序
cd /out
: > SHA256SUMS.new
if [ -f SHA256SUMS ]; then
    while read -r sum file; do
        file=${file#\*}
        case " $NEW_FILES " in *" $file "*) continue ;; esac
        if [ ! -f "$file" ]; then
            echo "注意：SHA256SUMS 列出的 $file 已不在輸出目錄，移除該列" >&2
            continue
        fi
        printf "%s  %s\n" "$sum" "$file" >> SHA256SUMS.new
    done < SHA256SUMS
fi
# shellcheck disable=SC2086
sha256sum $NEW_FILES >> SHA256SUMS.new
LC_ALL=C sort -k 2 SHA256SUMS.new > SHA256SUMS.tmp
rm -f SHA256SUMS.new
mv SHA256SUMS.tmp SHA256SUMS
# 其他產品的列沿用舊值：確認它們的檔案沒有在兩次打包之間被替換
sha256sum -c SHA256SUMS >/dev/null || {
    echo "錯誤：SHA256SUMS 與輸出目錄的檔案不一致（上面列出不符的檔案），請清空輸出目錄後重新打包" >&2
    exit 1
}
if [ -n "${HOST_UID:-}" ]; then
    # shellcheck disable=SC2086
    chown "$HOST_UID:$HOST_GID" $NEW_FILES SHA256SUMS
fi
echo "==> 產出"
# shellcheck disable=SC2086
ls -l $NEW_FILES | sed "s/^/    /"
'

# Linux 主機上讓產出檔案屬於目前使用者（Windows/macOS 的 Docker Desktop 不需要）
HOST_UID=""
HOST_GID=""
case "$(uname -s)" in
    Linux) HOST_UID=$(id -u); HOST_GID=$(id -g) ;;
esac

# RUSTUP_TOOLCHAIN 覆寫 rust-toolchain.toml：image 內已有 1.99.0，但沒有該檔要求的
# rustfmt、clippy；不覆寫的話 rustup 每次都會在一次性容器裡重新下載這兩個元件。
# Windows Git Bash：MSYS_NO_PATHCONV=1 避免 /src、/out 等容器內路徑被自動轉換成 Windows
# 路徑。只設在 docker 這個指令上：git 等其他 Windows 程式仍需要正常的路徑轉換。
git -c core.autocrlf=false archive --format=tar "$TREEISH" |
    MSYS_NO_PATHCONV=1 docker run --rm -i \
    -v "$HOST_DIST:/out" \
    -v "$BUILD_VOLUME:/build" \
    -v "$CARGO_VOLUME:/usr/local/cargo/registry" \
    -e CARGO_TARGET_DIR=/build/target \
    -e CARGO_BUILD_JOBS="$BUILD_JOBS" \
    -e RUSTUP_TOOLCHAIN="$RUST_TOOLCHAIN-$HOST_TRIPLE" \
    -e APK_PACKAGES="$APK_PACKAGES" \
    -e BUILD_COMMIT="$BUILD_COMMIT" \
    -e NAME="$NAME" \
    -e VERSION="$VERSION" \
    -e TARGETS="$TARGETS" \
    -e PACKAGES="$PACKAGES" \
    -e HOST_TRIPLE="$HOST_TRIPLE" \
    -e PKG_SUBDIR="$PKG_SUBDIR" \
    -e EXAMPLE_SRC="$EXAMPLE_SRC" \
    -e EXAMPLE_DST="$EXAMPLE_DST" \
    -e UNIT_SRC="$UNIT_SRC" \
    -e PROTOCOL_SRC="$PROTOCOL_SRC" \
    -e CHANGELOG_SRC="$CHANGELOG_SRC" \
    -e ABOUT_CONFIG="$ABOUT_CONFIG" \
    -e ABOUT_TEMPLATE="$ABOUT_TEMPLATE" \
    -e CARGO_ABOUT_VERSION="$CARGO_ABOUT_VERSION" \
    -e CARGO_CYCLONEDX_VERSION="$CARGO_CYCLONEDX_VERSION" \
    -e CARGO_ZIGBUILD_VERSION="$CARGO_ZIGBUILD_VERSION" \
    -e NFPM_VERSION="$NFPM_VERSION" \
    -e NFPM_SHA256="$NFPM_SHA256" \
    -e EXTRA_SRC="${EXTRA_SRC:-}" \
    -e EXTRA_DIST="${EXTRA_DIST:-}" \
    -e MANIFEST_FILE="$MANIFEST_FILE" \
    -e MANIFEST_JSON="$MANIFEST_JSON" \
    -e SOURCE_DATE_EPOCH="$SOURCE_DATE_EPOCH" \
    -e HOST_UID="$HOST_UID" \
    -e HOST_GID="$HOST_GID" \
    "$IMAGE" sh -c "$INNER"

# 簽章：SHA256SUMS 每次打包都會改變，舊簽章一律失效
if [ -n "${SIGNING_KEY:-}" ]; then
    sh "$SCRIPT_DIR/../../packaging/sign-release.sh" sign "$DIST_DIR" "$SIGNING_KEY"
elif [ -f "$DIST_DIR/SHA256SUMS.sig" ]; then
    rm -f "$DIST_DIR/SHA256SUMS.sig"
    echo "注意：SHA256SUMS 已改變，刪除失效的 SHA256SUMS.sig（設定 SIGNING_KEY 可重新簽章）" >&2
fi

echo
echo "==> 完成：$HOST_DIST"
echo "    SHA256SUMS："
sed 's/^/      /' "$DIST_DIR/SHA256SUMS"
