# 由 run.sh 以 source 載入：註冊與憑證（S12～S15）測試用的第二個 Kairos。
#
# 與主要的 Kairos 分開，因為註冊端點（8444）需要 mTLS（auth.client_ca），而主要測試的假 agent 不帶用戶端憑證。
# 以 openssl 建立測試用 PKI：P-384 根 CA、localhost 的伺服器憑證（8443、8444、主控台共用）；簽發 CA 依
# INSTALL 的流程由 Kairos 產生私鑰與 CSR（kairos pki issuing-ca new）→ 以根 CA 簽發 → install（第一個自動啟用）。
#
# 需要的變數：TMP、KAIROS_BIN、PW、PKI_CONSOLE_PORT、PKI_AGENT_PORT、PKI_ENROLL_PORT
# 設定的變數：PKI_PID（cleanup 時結束）；匯出 KAIROS_PKI_URL、PKI_ENROLL_PORT、PKI_DIR、PKI_LOG

K2="$TMP/pki"
mkdir -p "$K2"
openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-384 -out "$K2/root.key" 2>/dev/null
openssl req -new -x509 -key "$K2/root.key" -subj "/CN=Kairos Web E2E Root CA" -days 3650 -sha384 \
    -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign,cRLSign" \
    -out "$K2/root.pem" 2>/dev/null
openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -out "$K2/server.key" 2>/dev/null
openssl req -new -key "$K2/server.key" -subj "/CN=localhost" -out "$K2/server.csr" 2>/dev/null
printf 'basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth\nsubjectAltName=DNS:localhost,IP:127.0.0.1\n' >"$K2/server.ext"
openssl x509 -req -in "$K2/server.csr" -CA "$K2/root.pem" -CAkey "$K2/root.key" -set_serial 0x1001 -days 365 \
    -sha384 -extfile "$K2/server.ext" -out "$K2/server.pem" 2>/dev/null
chmod 600 "$K2/root.key" "$K2/server.key"

CFG2="$K2/kairos.toml"
cat >"$CFG2" <<CONF
[server]
listen = "127.0.0.1:$PKI_AGENT_PORT"
cert_path = "$K2/server.pem"
key_path = "$K2/server.key"
reload_interval_secs = 1

[storage]
data_dir = "$K2/data"
max_total_bytes = 0

[auth]
client_ca = "$K2/root.pem"

[log]
level = "info"
stdout = false

[console]
listen = "127.0.0.1:$PKI_CONSOLE_PORT"

[enroll]
mode = "open"
listen = "127.0.0.1:$PKI_ENROLL_PORT"
poll_after_base_secs = 5

[pki]
mode = "online"
server_ca_path = "$K2/root.pem"

[accounts]
argon2_memory_kib = 8192
argon2_iterations = 1
CONF

"$KAIROS_BIN" pki issuing-ca new --out "$K2/issuing.csr" --config "$CFG2" >/dev/null 2>"$K2/issuing-new.err" \
    || { cat "$K2/issuing-new.err" >&2; exit 1; }
printf 'basicConstraints=critical,CA:TRUE,pathlen:0\nkeyUsage=critical,keyCertSign,cRLSign\nextendedKeyUsage=clientAuth\nsubjectKeyIdentifier=hash\nauthorityKeyIdentifier=keyid\n' >"$K2/issuing.ext"
openssl x509 -req -in "$K2/issuing.csr" -CA "$K2/root.pem" -CAkey "$K2/root.key" -set_serial 0x2001 -days 1825 \
    -sha384 -extfile "$K2/issuing.ext" -out "$K2/issuing.pem" 2>/dev/null
"$KAIROS_BIN" pki issuing-ca install "$K2/issuing.pem" --config "$CFG2" >/dev/null

for pair in admin:admin operator:operator auditor:auditor viewer:viewer; do
    printf '%s' "$PW" | "$KAIROS_BIN" users create "${pair%%:*}" --role "${pair##*:}" --password-stdin --config "$CFG2" >/dev/null
done

"$KAIROS_BIN" --config "$CFG2" >"$K2/kairos.out" 2>&1 &
PKI_PID=$!
for _ in $(seq 1 100); do
    if curl -sk "https://127.0.0.1:$PKI_CONSOLE_PORT/healthz" >/dev/null 2>&1; then break; fi
    sleep 0.2
done
curl -sk "https://127.0.0.1:$PKI_CONSOLE_PORT/healthz" >/dev/null || { cat "$K2/kairos.out" >&2; exit 1; }

export KAIROS_PKI_URL="https://localhost:$PKI_CONSOLE_PORT" PKI_ENROLL_PORT PKI_DIR="$K2" PKI_LOG="$K2/kairos.out" PULSE_BIN
