# systemd 測試主機（Debian／Ubuntu）：systemd 為 PID 1（docker run --privileged --cgroupns=private）。
# 只裝 Pulse 安裝與遠端升級需要的東西（pulse-install.sh：ssh-keygen、sha256sum；輔助程式：dpkg-deb、tar）。
ARG BASE=debian:12
FROM ${BASE}
ENV container=docker DEBIAN_FRONTEND=noninteractive
RUN apt-get update \
    && apt-get install -y --no-install-recommends systemd systemd-sysv dbus openssh-client ca-certificates procps \
    && apt-get clean && rm -rf /var/lib/apt/lists/* \
    && systemctl mask getty.target console-getty.service systemd-logind.service systemd-timesyncd.service \
        apt-daily.timer apt-daily-upgrade.timer e2scrub_all.timer fstrim.timer 2>/dev/null; true
COPY kp-undocker.service /etc/systemd/system/kp-undocker.service
RUN systemctl enable kp-undocker.service
STOPSIGNAL SIGRTMIN+3
CMD ["/sbin/init"]
