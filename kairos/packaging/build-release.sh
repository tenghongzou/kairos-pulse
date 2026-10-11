#!/bin/sh
# Kairos 發佈包打包腳本
#
# 以 docker 的 rust:1.99-alpine 建置 musl 靜態連結的 release 執行檔，
# 產出 dist/<名稱>-<commit>[-dirty]-linux-x86_64.tar.gz 與 dist/SHA256SUMS
# （dist/ 位於本產品目錄下，例如 monorepo 的 kairos/dist/）。
#
# 用法（在 repo 任一目錄執行皆可；Windows 請用 Git Bash）：
#   sh packaging/build-release.sh          （在產品目錄下）
#   sh kairos/packaging/build-release.sh   （在 monorepo 根目錄）
#
# 本產品是 Cargo workspace 的成員：Cargo.lock 與 rust-toolchain.toml 在 workspace 根目錄
# （git repo 根目錄），整個 workspace 以唯讀方式掛進容器，在產品目錄內建置。
#
# 快取：
#   - CARGO_TARGET_DIR 放在 docker volume「sync-build-<repo 目錄名稱>」
#   - cargo registry 快取放在 docker volume「sync-cargo」（各專案共用）
#
# tar 內容（一層 <套件名稱>/ 目錄）：執行檔（755）、範例設定、README.md、
# PROTOCOL.md、INSTALL.md、systemd unit（其餘皆 644），擁有者一律為 0:0。
# PROTOCOL.md 取自兩個產品共用的唯一規格（PROTOCOL_SRC，相對於 repo 根目錄），
# 產品目錄下的 PROTOCOL.md 只是指向它的說明。
set -eu

# ---- 專案設定 -------------------------------------------------------------
NAME=kairos                          # 執行檔名稱（Cargo 的 bin 名稱）
EXAMPLE_SRC=kairos.example.toml      # repo 中的範例設定
EXAMPLE_DST=kairos.example.toml      # 放進 tar 時的檔名
PROTOCOL_SRC=crates/kairos-proto/PROTOCOL.md  # 協定規格（相對於 repo 根目錄）
UNIT_SRC=packaging/kairos.service    # systemd unit 範本
# ---------------------------------------------------------------------------

IMAGE=rust:1.99-alpine
TARGET=x86_64-unknown-linux-musl
CARGO_VOLUME=sync-cargo

die() {
    echo "錯誤：$*" >&2
    exit 1
}

# ---- 前置檢查 -------------------------------------------------------------
command -v git >/dev/null 2>&1 || die "找不到 git"
command -v docker >/dev/null 2>&1 || die "找不到 docker，請先安裝 Docker（Windows 為 Docker Desktop）"
docker info >/dev/null 2>&1 || die "無法連線到 docker daemon，請確認 Docker 已啟動且目前使用者有權限"

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
REPO=$(git -C "$SCRIPT_DIR" rev-parse --show-toplevel) || die "$SCRIPT_DIR 不在 git repo 中"
# 產品目錄（packaging/ 的上一層）相對於 repo 根目錄的路徑，例如 kairos；以 git 計算，避免
# Git Bash 下 pwd（/f/...）與 git（F:/...）的路徑格式不同
PKG_SUBDIR=$(cd "$SCRIPT_DIR/.." && git rev-parse --show-prefix) || die "無法取得產品目錄"
PKG_SUBDIR=${PKG_SUBDIR%/}
PKG_SUBDIR=${PKG_SUBDIR:-.}
cd "$REPO"
[ -f Cargo.lock ] || die "缺少 Cargo.lock（workspace 根目錄）"
[ -f "$PROTOCOL_SRC" ] || die "缺少 $PROTOCOL_SRC（協定規格）"
cd "$PKG_SUBDIR"

for f in Cargo.toml README.md "$EXAMPLE_SRC" packaging/INSTALL.md "$UNIT_SRC"; do
    [ -f "$f" ] || die "缺少 $f"
done

COMMIT=$(git rev-parse --short HEAD)
DIRTY=""
if [ -n "$(git status --porcelain --untracked-files=normal)" ]; then
    DIRTY="-dirty"
    echo "警告：工作目錄有未提交的變更，檔名會加上 -dirty" >&2
fi
# tar 內檔案的時間戳記固定為該 commit 的時間，同一份原始碼重複打包時內容一致
SOURCE_DATE_EPOCH=$(git log -1 --format=%ct HEAD)
PKG="$NAME-$COMMIT$DIRTY-linux-x86_64"
# 傳給 build.rs 的 BUILD_COMMIT 也帶 -dirty，--version 才不會把未提交的建置標成乾淨的 commit
BUILD_VOLUME="sync-build-$(basename "$REPO")"

mkdir -p dist
# docker 需要主機路徑；Git Bash 下 pwd -W 會給 F:/... 形式，其他環境用一般的 pwd
HOST_PKG=$(pwd -W 2>/dev/null || pwd)
HOST_REPO=$(cd "$REPO" && { pwd -W 2>/dev/null || pwd; })

echo "==> 建置 $PKG"
echo "    image=$IMAGE target=$TARGET"
echo "    volumes: $BUILD_VOLUME（target）、$CARGO_VOLUME（registry）"

# 容器內執行的腳本（以 sh -c 傳入；變數用 -e 傳入，這裡的 $ 不在主機展開）
INNER='
set -eu
apk add --no-cache musl-dev tar >/dev/null
# 在成員目錄內執行 cargo build 只會建置該成員；Cargo.lock 取自 workspace 根目錄（/src）
SRC="/src/$PKG_SUBDIR"
cd "$SRC"
cargo build --release --locked --target "$TARGET"
BIN="$CARGO_TARGET_DIR/$TARGET/release/$NAME"
[ -x "$BIN" ] || { echo "找不到建置產物 $BIN" >&2; exit 1; }

echo "==> $NAME --version"
if ! "$BIN" --version; then
    echo "錯誤：$NAME --version 執行失敗（發佈包必須能以 --version 確認版本與 commit）" >&2
    exit 1
fi

STAGE=/tmp/stage
rm -rf "$STAGE"
mkdir -p "$STAGE/$PKG"
chmod 755 "$STAGE/$PKG"
install -m 755 "$BIN"                    "$STAGE/$PKG/$NAME"
install -m 644 "$SRC/$EXAMPLE_SRC"       "$STAGE/$PKG/$EXAMPLE_DST"
install -m 644 "$SRC/README.md"          "$STAGE/$PKG/README.md"
install -m 644 "/src/$PROTOCOL_SRC"     "$STAGE/$PKG/PROTOCOL.md"
install -m 644 "$SRC/packaging/INSTALL.md" "$STAGE/$PKG/INSTALL.md"
install -m 644 "$SRC/$UNIT_SRC"          "$STAGE/$PKG/$(basename "$UNIT_SRC")"

cd "$STAGE"
tar --owner=0 --group=0 --numeric-owner --sort=name \
    --mtime="@$SOURCE_DATE_EPOCH" -cf - "$PKG" | gzip -n -9 > "/out/$PKG.tar.gz.tmp"
mv "/out/$PKG.tar.gz.tmp" "/out/$PKG.tar.gz"
cd /out
sha256sum "$PKG.tar.gz" > SHA256SUMS
if [ -n "${HOST_UID:-}" ]; then
    chown "$HOST_UID:$HOST_GID" "$PKG.tar.gz" SHA256SUMS
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
    -v "$HOST_PKG/dist:/out" \
    -v "$BUILD_VOLUME:/build" \
    -v "$CARGO_VOLUME:/usr/local/cargo/registry" \
    -e CARGO_TARGET_DIR=/build/target \
    -e RUSTUP_TOOLCHAIN=1.99.0-$TARGET \
    -e BUILD_COMMIT="$COMMIT$DIRTY" \
    -e NAME="$NAME" \
    -e TARGET="$TARGET" \
    -e PKG="$PKG" \
    -e PKG_SUBDIR="$PKG_SUBDIR" \
    -e EXAMPLE_SRC="$EXAMPLE_SRC" \
    -e EXAMPLE_DST="$EXAMPLE_DST" \
    -e UNIT_SRC="$UNIT_SRC" \
    -e PROTOCOL_SRC="$PROTOCOL_SRC" \
    -e SOURCE_DATE_EPOCH="$SOURCE_DATE_EPOCH" \
    -e HOST_UID="$HOST_UID" \
    -e HOST_GID="$HOST_GID" \
    "$IMAGE" sh -c "$INNER"

echo
echo "==> 完成"
echo "    dist/$PKG.tar.gz"
echo "    dist/SHA256SUMS："
sed 's/^/      /' dist/SHA256SUMS
