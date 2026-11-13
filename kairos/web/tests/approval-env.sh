# 由 run.sh 以 source 載入：升級推送雙人覆核（ADR-0002 切片 S1～S3；e2e-approval.mjs）測試用的第三個 Kairos。
#
# 與主要的 Kairos 分開，因為 `[remote] rollout_approval` 只能在設定檔修改、需要重新啟動：主要的 Kairos 維持
# 預設的 `none`（0.9 的單人流程），這一個是 `required`，另有三位管理員（三人邊界：建立者與送出者都不能核准）。
#
# - 先以 `none` 啟動一次再改為 `required` 重新啟動：資料庫裡的前值是 `none`，所以這次啟動會記錄
#   「none → required」（稽核 settings.rollout_approval.change；S21 的「上一次改變」、S26 清單頁首）。
# - 測試的最後一項會把設定改回 `none` 並重新啟動（「雙人覆核已關閉」的稽核、告警事件與 S20／S21／S26 的顯示）；
#   目前的 PID 寫在 $APPROVAL_PID_FILE，run.sh 結束時依它清理。
# - 與主要的 Kairos 一樣沒有 mTLS：假 agent 不協商 upgrade，資格只依 hello 的版本與 install。
#
# 需要的變數：TMP、REPO、KAIROS_BIN、PW、APPROVAL_CONSOLE_PORT、APPROVAL_AGENT_PORT
# 設定的變數：APPROVAL_PID_FILE；匯出 KAIROS_APPROVAL_URL、APPROVAL_AGENT_PORT、APPROVAL_CFG、APPROVAL_DIR、
#             APPROVAL_LOG、APPROVAL_PID_FILE、KAIROS_BIN

K3="$TMP/approval"
mkdir -p "$K3"
CFG3="$K3/kairos.toml"
APPROVAL_PID_FILE="$K3/kairos.pid"

approval_cfg() { # rollout_approval 的值
    cat >"$CFG3" <<CONF
[server]
listen = "127.0.0.1:$APPROVAL_AGENT_PORT"
cert_path = "$REPO/kairos/testdata/pki/server.crt"
key_path = "$REPO/kairos/testdata/pki/server.key"

[storage]
data_dir = "$K3/data"
max_total_bytes = 0

[log]
level = "info"
stdout = false

[console]
listen = "127.0.0.1:$APPROVAL_CONSOLE_PORT"
cert_path = "$REPO/pulse/testdata/server.pem"
key_path = "$REPO/pulse/testdata/server.key"

[accounts]
argon2_memory_kib = 8192
argon2_iterations = 1

[alerts]
enabled = true

[remote]
rollout_approval = "$1"
CONF
}

approval_start() {
    "$KAIROS_BIN" --config "$CFG3" >>"$K3/kairos.out" 2>&1 &
    echo $! >"$APPROVAL_PID_FILE"
    for _ in $(seq 1 100); do
        if curl -sk "https://127.0.0.1:$APPROVAL_CONSOLE_PORT/healthz" >/dev/null 2>&1; then break; fi
        sleep 0.2
    done
    curl -sk "https://127.0.0.1:$APPROVAL_CONSOLE_PORT/healthz" >/dev/null || { cat "$K3/kairos.out" >&2; exit 1; }
}

approval_cfg none
# 三位管理員（alice、bob、carol）、維運者、稽核員、唯讀
for pair in alice:admin bob:admin carol:admin oscar:operator audrey:auditor vera:viewer; do
    printf '%s' "$PW" | "$KAIROS_BIN" users create "${pair%%:*}" --role "${pair##*:}" --password-stdin --config "$CFG3" >/dev/null
done
approval_start
pid="$(cat "$APPROVAL_PID_FILE")"
kill "$pid" 2>/dev/null || true
wait "$pid" 2>/dev/null || true
approval_cfg required
approval_start

export KAIROS_APPROVAL_URL="https://localhost:$APPROVAL_CONSOLE_PORT" APPROVAL_AGENT_PORT APPROVAL_CFG="$CFG3" APPROVAL_DIR="$K3" APPROVAL_LOG="$K3/kairos.out" APPROVAL_PID_FILE KAIROS_BIN
