#!/bin/sh
# 發佈包簽章與驗證（商業審計 D2）：以 ssh-keygen -Y 簽署合併版 SHA256SUMS。
#
# 用法（Windows 請用 Git Bash）：
#   sh packaging/sign-release.sh sign   <輸出目錄> <私鑰路徑>
#   sh packaging/sign-release.sh verify <輸出目錄> <allowed_signers 檔>
#
# sign：確認 SHA256SUMS 與目錄內容一致、沒有 -dirty 套件後，產生 SHA256SUMS.sig；
#       私鑰旁有 <私鑰>.pub 時，再以它驗證剛產生的簽章（建議保留 .pub）。
#       私鑰必須放在 git repo 之外（腳本會拒絕 repo 內的私鑰），可以有密碼（ssh-keygen 會詢問）
#       或放在 ssh-agent 中（此時 <私鑰路徑> 給 .pub 檔即可）。
# verify：與客戶在 INSTALL.md 第 2 節執行的步驟相同：驗簽章，再驗每個檔案的雜湊。
#
# 簽章參數（客戶驗證時必須使用相同的值，改變時要同步更新兩份 INSTALL.md 與 RELEASING.md）：
#   namespace  kairos-pulse-release
#   principal  release@kairos-pulse（allowed_signers 第一欄；可用環境變數 SIGNER_ID 覆寫）
#
# 需要 OpenSSH 8.1 以上的 ssh-keygen（Git for Windows、Windows 11 內建、macOS、
# Ubuntu 20.04+、Debian 11+、RHEL 9 皆可）。
set -eu

NAMESPACE=kairos-pulse-release
SIGNER_ID=${SIGNER_ID:-release@kairos-pulse}

die() {
    echo "錯誤：$*" >&2
    exit 1
}

usage() {
    sed -n '4,6p' "$0" >&2
    exit 2
}

# 驗證 SHA256SUMS 列出的每個檔案（在輸出目錄內執行）
check_sums() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum -c SHA256SUMS
    else
        shasum -a 256 -c SHA256SUMS
    fi
}

[ $# -eq 3 ] || usage
MODE=$1
DIR=$2
KEY_OR_SIGNERS=$3
command -v ssh-keygen >/dev/null 2>&1 || die "找不到 ssh-keygen（需要 OpenSSH 8.1 以上）"
[ -d "$DIR" ] || die "找不到輸出目錄 $DIR"
[ -f "$KEY_OR_SIGNERS" ] || die "找不到 $KEY_OR_SIGNERS"
# 轉成絕對路徑，之後 cd 到輸出目錄仍可使用
KEY_OR_SIGNERS=$(cd "$(dirname "$KEY_OR_SIGNERS")" && pwd)/$(basename "$KEY_OR_SIGNERS")
cd "$DIR"
[ -f SHA256SUMS ] || die "$DIR 中沒有 SHA256SUMS"

case "$MODE" in
sign)
    KEY=$KEY_OR_SIGNERS
    # 私鑰不得放在 git repo 內（即使有 .gitignore，也容易被複製或誤提交）
    if KEY_REPO=$(git -C "$(dirname "$KEY")" rev-parse --show-toplevel 2>/dev/null); then
        die "私鑰 $KEY 位於 git repo（$KEY_REPO）內，請移到 repo 之外（例如加密的隨身碟或密碼管理工具）"
    fi
    if grep -q -- '-dirty-' SHA256SUMS; then
        die "SHA256SUMS 含有 -dirty 套件（從有未提交變更的工作目錄建置），不可簽章"
    fi
    echo "==> 確認 SHA256SUMS 與目錄內容一致"
    check_sums
    # SHA256SUMS.sig 已存在時 ssh-keygen 會詢問是否覆寫，非互動環境下不寫入卻回傳 0，
    # 舊簽章會留下來；因此先刪除
    rm -f SHA256SUMS.sig
    echo "==> 簽章（namespace=$NAMESPACE）"
    ssh-keygen -Y sign -f "$KEY" -n "$NAMESPACE" SHA256SUMS
    [ -s SHA256SUMS.sig ] || die "ssh-keygen 沒有產生 SHA256SUMS.sig"
    PUB="${KEY%.pub}.pub"
    if [ -f "$PUB" ]; then
        TMP_SIGNERS=$(mktemp)
        trap 'rm -f "$TMP_SIGNERS"' EXIT
        printf '%s namespaces="%s" %s\n' "$SIGNER_ID" "$NAMESPACE" "$(cat "$PUB")" > "$TMP_SIGNERS"
        echo "==> 以 $PUB 驗證剛產生的簽章"
        ssh-keygen -Y verify -f "$TMP_SIGNERS" -I "$SIGNER_ID" -n "$NAMESPACE" \
            -s SHA256SUMS.sig < SHA256SUMS
        echo "    公鑰指紋：$(ssh-keygen -lf "$PUB")"
    else
        echo "注意：找不到 $PUB，未自動驗證簽章；請以 verify 模式與正式的 allowed_signers 驗證" >&2
    fi
    echo "==> 完成：$DIR/SHA256SUMS.sig"
    ;;
verify)
    SIGNERS=$KEY_OR_SIGNERS
    [ -f SHA256SUMS.sig ] || die "$DIR 中沒有 SHA256SUMS.sig"
    echo "==> 驗證 SHA256SUMS 的簽章"
    ssh-keygen -Y verify -f "$SIGNERS" -I "$SIGNER_ID" -n "$NAMESPACE" \
        -s SHA256SUMS.sig < SHA256SUMS || die "簽章驗證失敗，不要使用這些檔案"
    echo "==> 驗證檔案雜湊"
    check_sums || die "雜湊不符，不要使用這些檔案"
    ;;
*)
    usage
    ;;
esac
