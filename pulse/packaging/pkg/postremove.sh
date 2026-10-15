#!/bin/sh
# Pulse 套件：移除後（deb postrm／rpm %postun，nfpm 打包）。
#   deb：$1 = remove｜purge｜upgrade｜…；rpm：$1 = 0 移除、1 升級
# deb 的 purge 刪除這台主機的身分（identity/、agent.id）、緩衝、日誌與狀態檔；
# remove 與 rpm 的移除都保留（重新安裝後沿用同一個身分）。
set -e
case "${1:-}" in
    purge)
        rm -rf /opt/pulse/identity /opt/pulse/buffer /opt/pulse/logs /var/lib/pulse
        rm -f /opt/pulse/agent.id /opt/pulse/pulse.toml.bak-*
        rmdir /opt/pulse 2>/dev/null || true
        ;;
    remove | 0)
        if [ -d /run/systemd/system ]; then
            systemctl daemon-reload >/dev/null 2>&1 || true
        fi
        ;;
esac
exit 0
