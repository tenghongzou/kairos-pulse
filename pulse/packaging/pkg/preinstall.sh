#!/bin/sh
# Pulse 套件：安裝前（deb preinst／rpm %pre，nfpm 打包）。建立系統帳號 pulse，套件中的檔案
# （/opt/pulse/pulse.toml 屬於 root:pulse）才能以正確的群組解開。
set -e
if ! getent group pulse >/dev/null 2>&1; then
    groupadd --system pulse
fi
if ! getent passwd pulse >/dev/null 2>&1; then
    useradd --system --gid pulse --home-dir /opt/pulse --no-create-home \
        --shell /usr/sbin/nologin --comment "Pulse monitoring agent" pulse
fi
exit 0
