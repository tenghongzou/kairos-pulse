# systemd 測試主機（Rocky Linux 9）：systemd 為 PID 1（docker run --privileged --cgroupns=private）。
# 只裝 systemd 與 pulse-install.sh 需要的 ssh-keygen；刻意不另外安裝 cpio 等工具：套件的相依要自己帶進來
# （輔助程式解開 rpm 做預檢需要 rpm2cpio＋cpio，Rocky／RHEL 9 的最小安裝沒有 cpio）。
ARG BASE=rockylinux/rockylinux:9
FROM ${BASE}
ENV container=docker
RUN dnf -y install systemd openssh-clients procps-ng \
    && dnf clean all \
    && systemctl mask getty.target console-getty.service systemd-logind.service dnf-makecache.timer 2>/dev/null; true
COPY kp-undocker.service /etc/systemd/system/kp-undocker.service
RUN systemctl enable kp-undocker.service
STOPSIGNAL SIGRTMIN+3
CMD ["/sbin/init"]
