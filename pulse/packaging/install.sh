#!/bin/sh
# Pulse 一行安裝腳本（商業審計 O5）：下載 → 驗證簽章與雜湊 → 安裝 deb／rpm → pulse enroll → 啟動服務。
#
# 發佈時以 pulse-install.sh 的名稱放在發佈處，列在簽過章的 SHA256SUMS 中。完整說明見 Pulse 的
# INSTALL.md 第 A 節。建議的執行方式（先核對腳本本身的雜湊，再執行；雜湊取自已驗證的 SHA256SUMS
# 或供應商另外提供的管道）：
#
#   curl -fsSLO https://downloads.example.com/pulse/0.8.0/pulse-install.sh
#   echo "<pulse-install.sh 的 SHA-256>  pulse-install.sh" | sha256sum -c -
#   sudo PULSE_INSTALL_STRING='pulse-enroll:v1:…' sh pulse-install.sh \
#        --base-url https://downloads.example.com/pulse/0.8.0 --allowed-signers /etc/kairos-pulse/allowed_signers
#
# 用法：
#   sh pulse-install.sh (--base-url <URL> [--ca-file <檔案>] | --dir <目錄>) --allowed-signers <檔案>
#                       [--install-string-file <檔案>] [--version <版本>] [--no-enroll] [--no-start]
#                       [--no-remote-upgrade] [--no-remote-config]
#
#   --base-url <URL>          發佈處（含 SHA256SUMS、SHA256SUMS.sig 與套件的目錄）；也可以是 Kairos 註冊端點的
#                             下載端點 https://<註冊端點>:8444/agent（Kairos 0.9 起，enroll.download_dir）
#   --ca-file <檔案>          以這個 CA 驗證 --base-url 的 HTTPS 伺服器憑證（Kairos 的下載端點使用 Kairos 的根 CA，
#                             不在系統的信任清單中）。完整性仍以發佈公鑰驗證，不依賴 HTTPS
#   --dir <目錄>              離線安裝：已下載好的發佈檔案所在的目錄
#   --allowed-signers <檔案>  發佈公鑰（必須從另一個管道取得，INSTALL.md 第 2 節）；
#                             也可用環境變數 PULSE_ALLOWED_SIGNERS。0.9.0 起另外複製到
#                             /etc/kairos-pulse/allowed_signers（root 644），作為遠端升級的信任錨
#   --install-string-file <檔案>  從檔案讀取 Kairos 管理者提供的安裝字串（建議；或用環境變數
#                             PULSE_INSTALL_STRING）。沒有安裝字串時只安裝、不註冊
#   --install-string <字串>   同上，但不建議：命令列參數會留在 shell 歷史，執行期間其他帳號也能從 ps 看到
#   --version <版本>          要安裝的版本（目錄中有多個版本時必填）
#   --no-enroll               只安裝套件，不執行 pulse enroll
#   --no-start                不啟動服務
#   --no-remote-upgrade       不接受 Kairos 推送的升級（pulse.toml 寫入 [upgrade] enabled = false）
#   --no-remote-config        不接受 Kairos 下發的設定（pulse.toml 寫入 [remote_config] enabled = false）
#
# 結束代碼：0 已註冊並啟動（或 --no-enroll 時已安裝）；7 已安裝、等待 Kairos 核准（服務在背景接續）；
#   2 用法錯誤；3 不支援的系統；4 簽章或雜湊驗證失敗；5 下載失敗；6 安裝失敗；
#   其他為 pulse enroll 的結束代碼（pulse enroll --help）。
#
# 需要：root、curl 或 wget（--base-url 時）、sha256sum、OpenSSH 8.1 以上的 ssh-keygen、
#       dpkg／apt（Debian、Ubuntu）或 rpm／dnf（RHEL、Rocky、Alma）、systemd。
set -eu

NAMESPACE=kairos-pulse-release
SIGNER_ID=release@kairos-pulse

die() {
    code=$1
    shift
    echo "錯誤：$*" >&2
    exit "$code"
}

say() {
    echo "==> $*" >&2
}

BASE_URL=""
DIR=""
SIGNERS=${PULSE_ALLOWED_SIGNERS:-}
CA_FILE=""
INSTALL_STRING=${PULSE_INSTALL_STRING:-}
VERSION=""
ENROLL=1
START=1
REMOTE_UPGRADE=1
REMOTE_CONFIG=1
while [ $# -gt 0 ]; do
    case "$1" in
    --base-url) BASE_URL=${2:?--base-url 需要 URL}; shift 2 ;;
    --dir) DIR=${2:?--dir 需要目錄}; shift 2 ;;
    --allowed-signers) SIGNERS=${2:?--allowed-signers 需要檔案}; shift 2 ;;
    --ca-file) CA_FILE=${2:?--ca-file 需要檔案}; shift 2 ;;
    --install-string) INSTALL_STRING=${2:?--install-string 需要安裝字串}; shift 2 ;;
    --install-string-file)
        f=${2:?--install-string-file 需要檔案}
        [ -r "$f" ] || die 2 "無法讀取 $f"
        INSTALL_STRING=$(sed -n '/[^[:space:]]/{s/^[[:space:]]*//;s/[[:space:]]*$//;p;q;}' "$f")
        shift 2
        ;;
    --version) VERSION=${2:?--version 需要版本}; shift 2 ;;
    --no-enroll) ENROLL=0; shift ;;
    --no-start) START=0; shift ;;
    --no-remote-upgrade) REMOTE_UPGRADE=0; shift ;;
    --no-remote-config) REMOTE_CONFIG=0; shift ;;
    -h | --help) sed -n '2,41p' "$0"; exit 0 ;;
    *) die 2 "未知的參數「$1」（--help 查看用法）" ;;
    esac
done

[ "$(id -u)" = 0 ] || die 2 "需要 root（以 sudo 執行）"
[ -n "$BASE_URL" ] || [ -n "$DIR" ] || die 2 "需要 --base-url 或 --dir"
[ -z "$BASE_URL" ] || [ -z "$DIR" ] || die 2 "--base-url 與 --dir 只能擇一"
[ -z "$CA_FILE" ] || [ -r "$CA_FILE" ] || die 2 "無法讀取 --ca-file $CA_FILE"
[ -n "$SIGNERS" ] || die 2 "需要 --allowed-signers（發佈公鑰；從供應商另外提供的管道取得，不要和套件從同一處下載）"
[ -f "$SIGNERS" ] || die 2 "找不到 $SIGNERS"
if [ "$ENROLL" = 1 ] && [ -z "$INSTALL_STRING" ]; then
    echo "注意：沒有安裝字串（--install-string-file 或 PULSE_INSTALL_STRING），只安裝套件、不註冊" >&2
    ENROLL=0
fi
command -v sha256sum >/dev/null 2>&1 || die 3 "找不到 sha256sum"
command -v ssh-keygen >/dev/null 2>&1 || die 3 "找不到 ssh-keygen（OpenSSH 8.1 以上；RHEL 8 等較舊的系統請在管理主機驗證後以 --dir 安裝）"
command -v systemctl >/dev/null 2>&1 || die 3 "需要 systemd"

case "$(uname -m)" in
x86_64 | amd64) ARCH=x86_64 ;;
aarch64 | arm64) ARCH=aarch64 ;;
*) die 3 "不支援的架構 $(uname -m)（只有 x86_64、aarch64）" ;;
esac
if command -v dpkg >/dev/null 2>&1 && ! command -v rpm >/dev/null 2>&1; then
    EXT=deb
elif command -v rpm >/dev/null 2>&1 && ! command -v dpkg >/dev/null 2>&1; then
    EXT=rpm
elif command -v apt-get >/dev/null 2>&1; then
    EXT=deb
elif command -v dnf >/dev/null 2>&1 || command -v yum >/dev/null 2>&1; then
    EXT=rpm
else
    die 3 "找不到 dpkg 或 rpm；請改用 tar.gz 發佈包（INSTALL.md 第 3 節）"
fi

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT INT TERM

fetch() {
    # $1 檔名 → $WORK/$1
    if [ -n "$DIR" ]; then
        [ -f "$DIR/$1" ] || die 5 "$DIR 中沒有 $1"
        cp "$DIR/$1" "$WORK/$1"
    elif command -v curl >/dev/null 2>&1; then
        if [ -n "$CA_FILE" ]; then
            curl -fsSL --proto '=https' --tlsv1.2 --cacert "$CA_FILE" -o "$WORK/$1" "$BASE_URL/$1" ||
                die 5 "無法下載 $BASE_URL/$1"
        else
            curl -fsSL --proto '=https' --tlsv1.2 -o "$WORK/$1" "$BASE_URL/$1" || die 5 "無法下載 $BASE_URL/$1"
        fi
    elif command -v wget >/dev/null 2>&1; then
        if [ -n "$CA_FILE" ]; then
            wget -q --ca-certificate="$CA_FILE" -O "$WORK/$1" "$BASE_URL/$1" || die 5 "無法下載 $BASE_URL/$1"
        else
            wget -q -O "$WORK/$1" "$BASE_URL/$1" || die 5 "無法下載 $BASE_URL/$1"
        fi
    else
        die 3 "需要 curl 或 wget"
    fi
}

# 1. SHA256SUMS 的簽章：以從另一個管道取得的發佈公鑰驗證
say "驗證 SHA256SUMS 的簽章"
fetch SHA256SUMS
fetch SHA256SUMS.sig
ssh-keygen -Y verify -f "$SIGNERS" -I "$SIGNER_ID" -n "$NAMESPACE" \
    -s "$WORK/SHA256SUMS.sig" <"$WORK/SHA256SUMS" >&2 ||
    die 4 "SHA256SUMS 的簽章驗證失敗：不要安裝，請向供應商確認"

# 2. 從已驗證的清單中找出這台主機的套件（不接受 -dirty 建置）
PATTERN="pulse-${VERSION:-[0-9]*}-[0-9a-f]*-linux-$ARCH.$EXT"
PKG=$(awk '{print $2}' "$WORK/SHA256SUMS" | sed 's/^\*//' | while read -r f; do
    # shellcheck disable=SC2254 # PATTERN 刻意作為 glob 比對
    case "$f" in
    *-dirty-*) ;;
    $PATTERN) echo "$f" ;;
    esac
done)
[ -n "$PKG" ] || die 2 "SHA256SUMS 中沒有符合 $PATTERN 的套件"
[ "$(echo "$PKG" | wc -l)" -eq 1 ] || die 2 "有多個版本符合，請以 --version 指定：$(echo "$PKG" | tr '\n' ' ')"

# 3. 下載套件並核對雜湊
say "下載並核對 $PKG"
fetch "$PKG"
(cd "$WORK" && grep -E "^[0-9a-f]{64}  \*?$PKG\$" SHA256SUMS | sha256sum -c - >&2) ||
    die 4 "$PKG 的 SHA-256 與已簽章的 SHA256SUMS 不符：不要安裝"

# 4. 安裝（升級時保留 pulse.toml、identity/、agent.id）
say "安裝 $PKG"
export PULSE_INSTALL_QUIET=1
if [ "$EXT" = deb ]; then
    if command -v apt-get >/dev/null 2>&1; then
        apt-get install -y "$WORK/$PKG" >&2 || die 6 "apt-get install 失敗"
    else
        dpkg -i "$WORK/$PKG" >&2 || die 6 "dpkg -i 失敗"
    fi
else
    if command -v dnf >/dev/null 2>&1; then
        dnf install -y "$WORK/$PKG" >&2 || die 6 "dnf install 失敗"
    else
        rpm -Uvh "$WORK/$PKG" >&2 || die 6 "rpm -Uvh 失敗"
    fi
fi
/opt/pulse/pulse --version >&2

# 4b. 遠端管理（crates/kairos-proto/REMOTE.md 第 4.2.1、5.6 節）：信任錨放在預設位置（root 擁有、
#     其他人不可寫）；退出選項寫進 pulse.toml（只能在本機改，輔助程式也只讀 pulse.toml）
ANCHOR=/etc/kairos-pulse/allowed_signers
if [ "$(readlink -f "$SIGNERS")" != "$ANCHOR" ]; then
    install -d -o root -g root -m 755 /etc/kairos-pulse
    install -o root -g root -m 644 "$SIGNERS" "$ANCHOR"
    say "已放置遠端升級的信任錨 $ANCHOR"
fi
add_section() {
    # $1 = 區段名稱；已有該區段時不重複加入（請手動編輯）
    if grep -q "^\[$1\]" /opt/pulse/pulse.toml 2>/dev/null; then
        echo "注意：/opt/pulse/pulse.toml 已有 [$1]，請手動設定 enabled = false" >&2
    else
        printf '\n[%s]\nenabled = false\n' "$1" >> /opt/pulse/pulse.toml
        say "已在 /opt/pulse/pulse.toml 停用 [$1]"
    fi
}
[ "$REMOTE_UPGRADE" = 1 ] || add_section upgrade
[ "$REMOTE_CONFIG" = 1 ] || add_section remote_config

# 5. 註冊（私鑰在本機產生，不會離開這台主機）；需人工核准時不等待，服務在背景接續
code=0
if [ "$ENROLL" = 1 ]; then
    say "pulse enroll"
    # 安裝字串（含 token）經 stdin 傳給 pulse，不放在命令列（執行期間其他帳號可從 ps 看到命令列）
    printf '%s\n' "$INSTALL_STRING" |
        /opt/pulse/pulse enroll --install-string-file - --non-interactive --no-wait \
            -c /opt/pulse/pulse.toml || code=$?
    case "$code" in
    0) ;;
    7) echo "已送出註冊申請，等待 Kairos 管理者核准（申請編號見上方）；核准後服務自動連線" >&2 ;;
    11) echo "這台主機已註冊過：沿用既有的身分（重新註冊：pulse enroll --force）" >&2; code=0 ;;
    *) die "$code" "pulse enroll 失敗（結束代碼 $code）；修正後重新執行本腳本或 pulse enroll" ;;
    esac
fi

# 6. 啟動服務
if [ "$START" = 1 ] && { [ "$ENROLL" = 1 ] || [ -s /opt/pulse/identity/client.pem ]; }; then
    say "啟動 pulse.service"
    systemctl enable --now pulse.service >&2 || die 6 "systemctl enable --now pulse 失敗（journalctl -u pulse）"
    sleep 3
    /opt/pulse/pulse status --brief >&2 || true
fi
exit "$code"
