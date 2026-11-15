# tests/systemd 情境 ansible 的受管主機：在 systemd 測試主機的映像（BASE）上加 sshd、sudo、Ansible 模組需要的
# python3（apt／dnf 的 Python 綁定），以及一個一般帳號 ops（sudo 免密碼）。控制端容器以 ops 經 SSH 連入、以
# become（sudo）取得 root，執行 packaging/ansible 的 role。ops 的 authorized_keys 由 run.sh 在測試時放入臨時
# 產生的公鑰；映像裡沒有任何金鑰或密碼（ops 的密碼是鎖住的）。
ARG BASE
FROM ${BASE}
RUN if command -v apt-get >/dev/null 2>&1; then \
        apt-get update \
        && apt-get install -y --no-install-recommends openssh-server sudo python3 python3-apt \
        && apt-get clean && rm -rf /var/lib/apt/lists/* /etc/ssh/ssh_host_*_key* \
        && systemctl enable ssh.service; \
    else \
        dnf -y install openssh-server sudo python3 python3-dnf \
        && dnf clean all \
        && systemctl enable sshd.service; \
    fi \
    && useradd --create-home --shell /bin/bash ops \
    && printf 'ops ALL=(ALL) NOPASSWD: ALL\n' >/etc/sudoers.d/ops \
    && chmod 440 /etc/sudoers.d/ops \
    && install -d -o ops -g ops -m 700 /home/ops/.ssh
# Rocky／RHEL 的 /etc/shadow、/etc/gshadow 是 0000：連 root 都要靠 CAP_DAC_OVERRIDE 才讀得到。非 root 帳號的
# 帳號檢查（sudo、sshd 的 PAM account）由 pam_unix 交給 setuid 的 /usr/sbin/unix_chkpwd 去讀 shadow。測試主機
# 是跑在 Ubuntu 上的容器，而 Ubuntu 24.04 的 AppArmor 內建一份掛在 /usr/sbin/unix_chkpwd 這個路徑上的設定檔
# （/etc/apparmor.d/unix-chkpwd）：--privileged 的容器雖然不受限，執行這個路徑的程式時仍會套用它。那份設定檔是
# 照 Debian／Ubuntu 的配置寫的（shadow 為 0640 root:shadow，不需要任何 capability），沒有放行 dac_override，所以
# 容器裡的 unix_chkpwd 以 root 開 0000 的 /etc/shadow 得到 EACCES，sudo 與 sshd 回報
# 「PAM account management error」。這是宿主機的 LSM 與 Rocky 的檔案權限不相容，不是 Rocky 或 Pulse 的問題；
# 真正的 Rocky 主機（SELinux）沒有這個情況。映像無法改宿主機的 AppArmor，所以把兩個檔案改成只有擁有者 root
# 可讀：讀取不再需要 capability，能讀的仍然只有 root。Debian 的映像不受影響（條件不成立）。
RUN for f in /etc/shadow /etc/gshadow; do \
        if [ "$(stat -c %a "$f")" = 0 ]; then chmod 0400 "$f"; fi; \
    done
