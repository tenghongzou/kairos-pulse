#!/bin/sh
# Kairos 套件：安裝後（deb postinst／rpm %post，nfpm 打包）。
#   deb：$1 = configure，$2 = 先前的版本（新安裝時為空）；rpm：$1 = 1 新安裝、2 升級
# 新安裝：建立可寫目錄、啟用（不啟動）服務，提示 kairos pki init；升級：重新啟動執行中的服務。
set -e
upgrade=0
case "${1:-}" in
    configure) [ -z "${2:-}" ] || upgrade=1 ;;
    2) upgrade=1 ;;
    abort-*) exit 0 ;;
esac

# 只有 data/、logs/ 可寫（unit 的 ProtectSystem=strict）；已存在時只修正擁有者與權限
install -d -o kairos -g kairos -m 750 /opt/kairos/data /opt/kairos/logs
# kairos pki init 寫根 CA 私鑰的位置（服務本身看不到：unit 的 InaccessiblePaths=）；
# init 之後要把內容移到離線保存
if [ "$upgrade" = 0 ] && [ ! -e /opt/kairos/root-ca ]; then
    install -d -o kairos -g kairos -m 700 /opt/kairos/root-ca
fi

if [ -d /run/systemd/system ]; then
    systemctl daemon-reload >/dev/null 2>&1 || true
    if [ "$upgrade" = 1 ]; then
        systemctl try-restart kairos.service >/dev/null 2>&1 || true
    else
        systemctl enable kairos.service >/dev/null 2>&1 || true
    fi
fi

if [ "$upgrade" = 0 ]; then
    cat <<'MSG'

Kairos 已安裝（服務已啟用，尚未啟動）。接下來（/opt/kairos/INSTALL.md 第 A 節）：
  1. 建立 PKI（根 CA、伺服器憑證、簽發 CA；--name 填 Pulse 連線時使用的名稱與 IP）：
       sudo -u kairos kairos pki init --config /opt/kairos/kairos.toml \
         --name monitor.example.com --name 10.0.0.5 --root-dir /opt/kairos/root-ca
  2. 啟動：systemctl start kairos
  3. 把 /opt/kairos/root-ca 移到離線保存後刪除（pki init 會再提醒一次）
MSG
fi
exit 0
