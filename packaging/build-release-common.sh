# shellcheck shell=sh
# Kairos／Pulse 發佈包打包的共用實作。
#
# 不要直接執行：由 kairos/packaging/build-release.sh、pulse/packaging/build-release.sh
# 設定好產品參數後以「.」載入。呼叫端必須先設定：
#   SCRIPT_DIR   產品的 packaging/ 目錄（絕對路徑）
#   NAME         執行檔名稱（Cargo 的 bin 名稱，也是發佈包名稱的開頭）
#   EXAMPLE_SRC  產品目錄中的範例設定；EXAMPLE_DST 為放進 tar 時的檔名
#   UNIT_SRC     systemd unit 範本（相對於產品目錄）
#   EXTRA_SRC    其他放進 tar 根目錄的檔案（相對於產品目錄，以空白分隔，可為空）
#
# 以 docker 的 rust alpine image（以 @sha256 digest 固定）建置 musl 靜態連結的 release 執行檔，
# 產出：
#   <DIST_DIR>/<名稱>-<版本>-<commit>[-dirty]-linux-x86_64.tar.gz
#   <DIST_DIR>/<名稱>-<版本>-<commit>[-dirty]-linux-x86_64.cdx.json   SBOM（與 tar 內的相同）
#   <DIST_DIR>/SHA256SUMS   兩個產品共用的合併清單：只更新本次產出檔案的那幾列，
#                           保留其他產品的列；最後以 sha256sum -c 確認整份清單與目錄內容一致
#
# 可用的環境變數：
#   DIST_DIR      輸出目錄，預設為 repo 根目錄的 dist/（兩個產品共用，SHA256SUMS 才能合併）
#   BUILD_VOLUME  CARGO_TARGET_DIR 使用的 docker volume，預設 sync-build-<repo 目錄名稱>；
#                 可重現性檢查以全新的 volume 指定，確保不是沿用上次的建置產物
#   SIGNING_KEY   簽章私鑰路徑（repo 之外）。有設定時打包後以 packaging/sign-release.sh 簽署
#                 SHA256SUMS；沒有設定時，因 SHA256SUMS 已改變，刪除舊的 SHA256SUMS.sig
#
# tar 內容（一層 <套件名稱>/ 目錄）：執行檔（755）、範例設定、README.md、PROTOCOL.md、
# CHANGELOG.md、INSTALL.md、THIRD-PARTY-LICENSES、<名稱>.cdx.json、systemd unit 與
# EXTRA_SRC（其餘皆 644），擁有者一律為 0:0，時間戳記固定為 commit 時間（SOURCE_DATE_EPOCH），
# 檔案依名稱排序。PROTOCOL.md 取自兩個產品共用的唯一規格，CHANGELOG.md 取自 repo 根目錄。
#
# THIRD-PARTY-LICENSES（商業審計 B2）與 SBOM（D4）每次建置都在容器內依 Cargo.lock 重新產生，
# 不會過期：
#   - cargo-about（設定 packaging/about.toml、範本 packaging/about.hbs）以 --frozen 離線執行，
#     授權全文只取自 crate 原始碼內的檔案；依賴出現 about.toml 未允許的授權時建置失敗。
#   - cargo-cyclonedx 產生 CycloneDX 1.5 JSON（只含目標平台、非建置期的依賴）；
#     SOURCE_DATE_EPOCH 讓 metadata.timestamp 固定為 commit 時間。
#   兩個工具以固定版本 cargo install --locked 安裝在 BUILD_VOLUME 的 tools/ 下，之後重複使用。
#   已知限制：cargo metadata 會把只由弱依賴特性（dep?/feature）提到、實際未啟用的可選依賴
#   列入 resolve，SBOM 因此可能多列少數沒有編譯進執行檔的 crate（0.2.0 為 toml 的 indexmap、
#   toml_writer 與 indexmap 的 hashbrown、equivalent）；cargo-auditable 有同樣的限制。
#   THIRD-PARTY-LICENSES 由 cargo-about 依實際的 feature 解析產生，不受影響。

# ---- 建置環境（兩個產品共用；更新方式見 RELEASING.md「更新建置 image」）-----
# rust:1.99-alpine（Alpine 3.24.2、rustc 1.99.0）的 multi-arch index digest，2026-10-12 查得。
# 以 digest 固定後，rustc、gcc、musl-dev 等 image 內的檔案都不會再變動；tag 只供閱讀。
IMAGE=rust:1.99-alpine@sha256:0cce0a5e0e8ba67b455257a3a02a1d99005f382748789d6464460028810f1627
# image 內沒有 GNU tar（busybox tar 不支援 --sort、--owner 等可重現打包所需的選項），
# 只有這個套件需要從 Alpine 套件庫安裝；版本鎖定為 image 的 Alpine 版本（v3.24）當時的版本。
# Alpine 套件庫只保留最新的修訂版：此版本被取代後 apk 會失敗，需依 RELEASING.md 一併更新。
APK_PACKAGES="tar=1.35-r5"
TARGET=x86_64-unknown-linux-musl
RUST_TOOLCHAIN=1.99.0
CARGO_VOLUME=sync-cargo
# 產生 THIRD-PARTY-LICENSES 與 SBOM 的工具（cargo install --locked，固定版本）
CARGO_ABOUT_VERSION=0.9.2
CARGO_CYCLONEDX_VERSION=0.5.9
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
# tar 內檔案的時間戳記固定為該 commit 的時間，同一份原始碼重複打包時內容一致
SOURCE_DATE_EPOCH=$(git log -1 --format=%ct HEAD)
BUILD_COMMIT="$COMMIT$DIRTY"
PKG="$NAME-$VERSION-$BUILD_COMMIT-linux-x86_64"

# ---- 輸出目錄與 docker volume ----------------------------------------------
DIST_DIR=${DIST_DIR:-$REPO/dist}
mkdir -p "$DIST_DIR"
HOST_DIST=$(host_path "$DIST_DIR")
HOST_REPO=$(host_path "$REPO")
BUILD_VOLUME=${BUILD_VOLUME:-sync-build-$(basename "$REPO")}

echo "==> 建置 $PKG"
echo "    image=$IMAGE"
echo "    target=$TARGET 輸出=$HOST_DIST"
echo "    volumes: $BUILD_VOLUME（target）、$CARGO_VOLUME（registry）"

# 容器內執行的腳本（以 sh -c 傳入；變數用 -e 傳入，這裡的 $ 不在主機展開）
# shellcheck disable=SC2016
INNER='
set -eu
apk add --no-cache $APK_PACKAGES >/dev/null || {
    echo "錯誤：apk 無法安裝 $APK_PACKAGES（Alpine 套件庫可能已更新該套件），請依 RELEASING.md 更新 image digest 與 APK_PACKAGES" >&2
    exit 1
}

# 授權清單與 SBOM 的工具：已安裝同版本時 cargo install 會直接略過
TOOLS=/build/tools
for tool in "cargo-about@$CARGO_ABOUT_VERSION --features cli" "cargo-cyclonedx@$CARGO_CYCLONEDX_VERSION"; do
    # shellcheck disable=SC2086
    CARGO_TARGET_DIR=/build/tools-target cargo install --quiet --locked --root "$TOOLS" $tool
done

# 在成員目錄內執行 cargo build 只會建置該成員；Cargo.lock 取自 workspace 根目錄（/src）
SRC="/src/$PKG_SUBDIR"
# cargo-about／cargo-cyclonedx 會讀取所有平台的依賴資訊，先下載 Cargo.lock 中的全部 crate
(cd /src && cargo fetch --locked --quiet)
cd "$SRC"
cargo build --release --locked --target "$TARGET"
BIN="$CARGO_TARGET_DIR/$TARGET/release/$NAME"
[ -x "$BIN" ] || { echo "找不到建置產物 $BIN" >&2; exit 1; }

echo "==> $NAME --version"
EXPECTED="$NAME $VERSION ($BUILD_COMMIT)"
ACTUAL=$("$BIN" --version) || {
    echo "錯誤：$NAME --version 執行失敗（發佈包必須能以 --version 確認版本與 commit）" >&2
    exit 1
}
echo "    $ACTUAL"
[ "$ACTUAL" = "$EXPECTED" ] || {
    echo "錯誤：--version 輸出「$ACTUAL」，預期「$EXPECTED」" >&2
    exit 1
}

STAGE=/tmp/stage
rm -rf "$STAGE"
mkdir -p "$STAGE/$PKG"
chmod 755 "$STAGE/$PKG"
install -m 755 "$BIN"                    "$STAGE/$PKG/$NAME"
install -m 644 "$SRC/$EXAMPLE_SRC"       "$STAGE/$PKG/$EXAMPLE_DST"
install -m 644 "$SRC/README.md"          "$STAGE/$PKG/README.md"
install -m 644 "/src/$PROTOCOL_SRC"     "$STAGE/$PKG/PROTOCOL.md"
install -m 644 "/src/$CHANGELOG_SRC"    "$STAGE/$PKG/CHANGELOG.md"
install -m 644 "$SRC/packaging/INSTALL.md" "$STAGE/$PKG/INSTALL.md"
install -m 644 "$SRC/$UNIT_SRC"          "$STAGE/$PKG/$(basename "$UNIT_SRC")"
for f in $EXTRA_SRC; do
    install -m 644 "$SRC/$f" "$STAGE/$PKG/$(basename "$f")"
done

echo "==> THIRD-PARTY-LICENSES（cargo-about $CARGO_ABOUT_VERSION）"
(cd /src && "$TOOLS/bin/cargo-about" generate --frozen \
    -m "$PKG_SUBDIR/Cargo.toml" -c "$ABOUT_CONFIG" \
    -o /tmp/THIRD-PARTY-LICENSES "$ABOUT_TEMPLATE")
grep -q "^Used by:" /tmp/THIRD-PARTY-LICENSES || {
    echo "錯誤：產生的 THIRD-PARTY-LICENSES 沒有任何授權條目" >&2
    exit 1
}
install -m 644 /tmp/THIRD-PARTY-LICENSES "$STAGE/$PKG/THIRD-PARTY-LICENSES"
sed -n "/^Overview/,/^$/p" /tmp/THIRD-PARTY-LICENSES | sed "s/^/    /"

echo "==> SBOM（cargo-cyclonedx $CARGO_CYCLONEDX_VERSION）"
# cargo-cyclonedx 把結果寫在 Cargo.toml 旁邊，/src 是唯讀的，因此複製一份 workspace
# （不含建置產物）到固定路徑；SBOM 的 bom-ref 含此路徑，固定才能重現。
# cargo metadata 會合併整個 workspace 的 feature（例如 Kairos 的 tokio "full" 會讓 Pulse 的
# SBOM 多出 parking_lot），因此複本的 workspace 只保留本產品與共用 crate，與實際建置一致。
SBOM_SRC=/work/kairos-pulse
rm -rf "$SBOM_SRC"
mkdir -p "$SBOM_SRC"
tar -C /src --exclude=./.git --exclude=target --exclude=dist -cf - . | tar -C "$SBOM_SRC" -xf -
sed -i "s|^members = .*|members = [\"$PKG_SUBDIR\", \"crates/*\"]|" "$SBOM_SRC/Cargo.toml"
grep -q "^members = \[\"$PKG_SUBDIR\", \"crates/\*\"\]" "$SBOM_SRC/Cargo.toml" || {
    echo "錯誤：無法在 SBOM 用的 workspace 複本中縮減 members（Cargo.toml 格式改變？）" >&2
    exit 1
}
(cd "$SBOM_SRC/$PKG_SUBDIR" && CARGO_NET_OFFLINE=true "$TOOLS/bin/cargo-cyclonedx" cyclonedx \
    --quiet --format json --spec-version 1.5 --target "$TARGET" \
    --describe binaries --no-build-deps)
SBOM="$SBOM_SRC/$PKG_SUBDIR/${NAME}_bin.cdx.json"
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

cd "$STAGE"
tar --owner=0 --group=0 --numeric-owner --sort=name \
    --mtime="@$SOURCE_DATE_EPOCH" -cf - "$PKG" | gzip -n -9 > "/out/$PKG.tar.gz.tmp"
mv "/out/$PKG.tar.gz.tmp" "/out/$PKG.tar.gz"
# SBOM 另外放一份在 tar 外，掃描工具（Dependency-Track 等）不必先解壓
install -m 644 "$STAGE/$PKG/$NAME.cdx.json" "/out/$PKG.cdx.json"

# 合併版 SHA256SUMS：去掉本次產出檔案的舊列與已不存在的檔案，加上新列，依檔名排序
cd /out
NEW_FILES="$PKG.tar.gz $PKG.cdx.json"
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
echo "==> tar 內容"
tar tzvf "$PKG.tar.gz"
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
MSYS_NO_PATHCONV=1 docker run --rm \
    -v "$HOST_REPO:/src:ro" \
    -v "$HOST_DIST:/out" \
    -v "$BUILD_VOLUME:/build" \
    -v "$CARGO_VOLUME:/usr/local/cargo/registry" \
    -e CARGO_TARGET_DIR=/build/target \
    -e RUSTUP_TOOLCHAIN="$RUST_TOOLCHAIN-$TARGET" \
    -e APK_PACKAGES="$APK_PACKAGES" \
    -e BUILD_COMMIT="$BUILD_COMMIT" \
    -e NAME="$NAME" \
    -e VERSION="$VERSION" \
    -e TARGET="$TARGET" \
    -e PKG="$PKG" \
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
    -e EXTRA_SRC="${EXTRA_SRC:-}" \
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
echo "==> 完成"
echo "    $HOST_DIST/$PKG.tar.gz"
echo "    $HOST_DIST/$PKG.cdx.json"
echo "    $HOST_DIST/SHA256SUMS："
sed 's/^/      /' "$DIST_DIR/SHA256SUMS"
