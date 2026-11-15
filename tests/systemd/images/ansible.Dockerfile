# tests/systemd 情境 ansible 的控制端：ansible-core 與 ssh 用戶端（role 在控制端以 ssh-keygen 驗證簽章）。
# 映像裡沒有金鑰；SSH 金鑰由 run.sh 在測試時臨時產生、測完刪除。
FROM debian:12
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update \
    && apt-get install -y --no-install-recommends ansible-core openssh-client ca-certificates \
    && apt-get clean && rm -rf /var/lib/apt/lists/*
CMD ["sleep", "infinity"]
