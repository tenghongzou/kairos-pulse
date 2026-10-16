#!/bin/sh
# Kairos 套件：安裝前（deb preinst／rpm %pre，nfpm 打包）。建立系統帳號 kairos，套件中的檔案
# （/opt/kairos/kairos.toml 屬於 root:kairos）才能以正確的群組解開。
set -e
if ! getent group kairos >/dev/null 2>&1; then
    groupadd --system kairos
fi
if ! getent passwd kairos >/dev/null 2>&1; then
    useradd --system --gid kairos --home-dir /opt/kairos --no-create-home \
        --shell /usr/sbin/nologin --comment "Kairos monitoring server" kairos
fi
exit 0
