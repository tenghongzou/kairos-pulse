#!/bin/sh
# Pulse 套件：安裝後（deb postinst／rpm %post，nfpm 打包）。
#   deb：$1 = configure，$2 = 先前的版本（新安裝時為空）；rpm：$1 = 1 新安裝、2 升級
# 新安裝：建立可寫目錄與 agent.id、啟用（不啟動）服務，提示 pulse enroll；
# 升級：重新啟動執行中的服務。兩者都記下安裝方式（/opt/pulse/.install-method）並啟用遠端升級的
# pulse-upgrade.path（crates/kairos-proto/REMOTE.md 第 4.3、4.4.1 節）。
set -e
upgrade=0
method=rpm
case "${1:-}" in
    configure) method=deb; [ -z "${2:-}" ] || upgrade=1 ;;
    2) upgrade=1 ;;
    abort-*) exit 0 ;;
esac

# 安裝方式：hello 的 install.method 與遠端升級選擇發佈檔的依據（不屬於套件，purge 時刪除）
echo "$method" > /opt/pulse/.install-method
chmod 644 /opt/pulse/.install-method

# 服務只能寫 logs/、buffer/、identity/（unit 的 ProtectSystem=strict，INSTALL.md 第 4 節）
install -d -o pulse -g pulse -m 750 /opt/pulse/logs /opt/pulse/buffer
install -d -o pulse -g pulse -m 700 /opt/pulse/identity
# agent.id：/opt/pulse 對服務唯讀，必須在這裡先產生（已存在時沿用，重新安裝不換身分）
if [ ! -e /opt/pulse/agent.id ]; then
    (cd /opt/pulse && ./pulse --print-agent-id >/dev/null 2>&1) || true
fi
if [ -f /opt/pulse/agent.id ]; then
    chown root:pulse /opt/pulse/agent.id
    chmod 640 /opt/pulse/agent.id
fi

if [ -d /run/systemd/system ]; then
    systemctl daemon-reload >/dev/null 2>&1 || true
    if [ "$upgrade" = 1 ]; then
        systemctl try-restart pulse.service >/dev/null 2>&1 || true
    else
        systemctl enable pulse.service >/dev/null 2>&1 || true
    fi
    # 遠端升級：監看 /var/lib/pulse/upgrade/request.json（不要遠端升級時以 pulse.toml 的
    # [upgrade] enabled = false 關閉；也可 systemctl disable --now pulse-upgrade.path）
    systemctl enable --now pulse-upgrade.path >/dev/null 2>&1 || true
fi

if [ "$upgrade" = 0 ] && [ -z "${PULSE_INSTALL_QUIET:-}" ]; then
    cat <<'MSG'

Pulse 已安裝（服務已啟用，尚未啟動）。以 Kairos 管理者提供的安裝字串註冊後啟動：
  sudo pulse enroll --install-string 'pulse-enroll:v1:…'
  sudo systemctl start pulse
詳見 /opt/pulse/INSTALL.md 第 A 節（一行安裝腳本 install.sh 會自動完成這兩步）
MSG
fi
exit 0
