#!/bin/sh
# Pulse 套件：移除前（deb prerm／rpm %preun，nfpm 打包）。
#   deb：$1 = remove｜upgrade｜deconfigure｜failed-upgrade；rpm：$1 = 0 移除、1 升級
# 只在移除（不是升級）時停止並停用服務。不會自動通知 Kairos（pulse decommission）：
# 完全移除前請先手動執行，見 INSTALL.md 第 12 節。
set -e
case "${1:-}" in
    remove | 0)
        if [ -d /run/systemd/system ]; then
            systemctl disable --now pulse.service >/dev/null 2>&1 || true
        fi
        ;;
esac
exit 0
