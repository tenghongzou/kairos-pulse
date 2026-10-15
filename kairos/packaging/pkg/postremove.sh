#!/bin/sh
# Kairos 套件：移除後（deb postrm／rpm %postun，nfpm 打包）。
#   deb：$1 = remove｜purge｜upgrade｜…；rpm：$1 = 0 移除、1 升級
# 資料（/opt/kairos/data：資料庫、data/pki 的私鑰）在任何情況都不刪除；purge 只刪除日誌。
set -e
case "${1:-}" in
    purge)
        rm -rf /opt/kairos/logs
        echo "Kairos 的資料 /opt/kairos/data（資料庫與 data/pki 的私鑰）保留未刪除；確定不再需要時自行刪除" >&2
        ;;
    remove | 0)
        if [ -d /run/systemd/system ]; then
            systemctl daemon-reload >/dev/null 2>&1 || true
        fi
        ;;
esac
exit 0
