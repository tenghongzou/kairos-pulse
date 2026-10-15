#!/usr/bin/env bash
# 主控台網頁介面的端對端測試（Playwright＋axe）：對真的 Kairos（測試用的資料庫與憑證）執行。
# 不在 cargo test 中；在 kp-build（或任何有 Node.js 與 Playwright 的 Linux）執行：
#
#   bash kairos/web/tests/run.sh
#
# 環境變數（皆有預設值）：
#   KAIROS_BIN   kairos 執行檔（預設先 cargo build -p kairos -p pulse，用 $CARGO_TARGET_DIR/debug/kairos）
#   PULSE_BIN    pulse 執行檔（預設與 KAIROS_BIN 同一個目錄；註冊與憑證的測試以它送出註冊申請）
#   NODE         node 執行檔（預設 ~/opt/node/bin/node，否則 PATH 中的 node）
#   PW_DIR       含 node_modules/playwright 與 node_modules/axe-core 的目錄（預設 ~/pw）
#   OUT_DIR      截圖與結果（預設 kairos/web/tests/out）
#   KEEP=1       結束後保留暫存目錄（資料庫、日誌）
#   ONLY=名稱    只執行名稱含這個字串的測試
#   連接埠       CONSOLE_PORT、AGENT_PORT、ENROLL_PORT（主要的 Kairos）、PKI_CONSOLE_PORT、PKI_AGENT_PORT、
#                PKI_ENROLL_PORT（第二個 Kairos）：指定時沿用；未指定時自動挑選目前沒有被使用的連接埠
#                （20000～31999，低於 Linux 的暫時連接埠範圍），同一台機器同時執行多份測試也不會互相干擾
#
# 流程：建立暫存目錄與 kairos.toml（主控台、agent 連接埠、註冊端點都只綁 127.0.0.1，未啟用 mTLS）
# → 以 kairos users create 建立測試帳號 → 啟動 Kairos → node e2e.mjs（假 agent、瀏覽器）。
# 註冊與憑證（S12～S15）的測試另外啟動第二個 Kairos（mTLS、註冊端點、線上簽發；以 openssl 建立測試用的
# 根 CA、伺服器憑證與簽發 CA；pki-env.sh、e2e-pki.mjs）。
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO="$(cd "$HERE/../../.." && pwd)"
NODE="${NODE:-$HOME/opt/node/bin/node}"
[ -x "$NODE" ] || NODE="$(command -v node)"
PW_DIR="${PW_DIR:-$HOME/pw}"
OUT_DIR="${OUT_DIR:-$HERE/out}"

# ---- 連接埠：指定的沿用，其餘自動挑選 ----
# pick_ports N [避開的連接埠...]：印出 N 個互不相同、目前可以在 127.0.0.1 綁定的連接埠（挑選期間全部占住，
# 避免挑到同一個）。範圍 20000～31999 低於 Linux 預設的暫時連接埠範圍（32768～60999），對外連線的來源連接埠
# 不會在 Kairos 啟動前搶走它。
pick_ports() {
    "$NODE" -e '
const net = require("net");
const n = Number(process.argv[1]);
const avoid = new Set(process.argv.slice(2).map(Number));
const held = [];
const out = [];
(async () => {
  for (let tries = 0; out.length < n && tries < 1000; tries += 1) {
    const p = 20000 + Math.floor(Math.random() * 12000);
    if (avoid.has(p) || out.includes(p)) continue;
    const s = net.createServer();
    const ok = await new Promise((r) => {
      s.once("error", () => r(false));
      s.listen(p, "127.0.0.1", () => r(true));
    });
    if (ok) {
      held.push(s);
      out.push(p);
    }
  }
  for (const s of held) s.close();
  if (out.length < n) process.exit(1);
  console.log(out.join("
"));
})();
' "$@"
}
PORT_VARS=(CONSOLE_PORT AGENT_PORT ENROLL_PORT PKI_CONSOLE_PORT PKI_AGENT_PORT PKI_ENROLL_PORT)
need=()
given=()
for v in "${PORT_VARS[@]}"; do
    if [ -n "${!v:-}" ]; then given+=("${!v}"); else need+=("$v"); fi
done
if [ "${#need[@]}" -gt 0 ]; then
    picked=()
    while IFS= read -r p; do picked+=("$p"); done < <(pick_ports "${#need[@]}" ${given[@]+"${given[@]}"} || true)
    [ "${#picked[@]}" -eq "${#need[@]}" ] || { echo "找不到足夠的空連接埠" >&2; exit 2; }
    for i in "${!need[@]}"; do printf -v "${need[$i]}" '%s' "${picked[$i]}"; done
fi
echo "連接埠：主控台 $CONSOLE_PORT、agent $AGENT_PORT、註冊端點 $ENROLL_PORT；第二個 Kairos：主控台 $PKI_CONSOLE_PORT、agent $PKI_AGENT_PORT、註冊端點 $PKI_ENROLL_PORT"

if [ -z "${KAIROS_BIN:-}" ]; then
    (cd "$REPO" && cargo build -p kairos -p pulse --locked --quiet)
    KAIROS_BIN="${CARGO_TARGET_DIR:-$REPO/target}/debug/kairos"
fi
[ -x "$KAIROS_BIN" ] || { echo "找不到 kairos 執行檔：$KAIROS_BIN" >&2; exit 2; }
PULSE_BIN="${PULSE_BIN:-$(dirname "$KAIROS_BIN")/pulse}"
[ -x "$PULSE_BIN" ] || { echo "找不到 pulse 執行檔：$PULSE_BIN" >&2; exit 2; }

TMP="$(mktemp -d -t kairos-web-XXXXXX)"
KAIROS_PID=""
PKI_PID=""
cleanup() {
    for pid in "$KAIROS_PID" "$PKI_PID"; do
        if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
            kill "$pid" 2>/dev/null || true
            wait "$pid" 2>/dev/null || true
        fi
    done
    if [ "${KEEP:-0}" = "1" ]; then echo "保留暫存目錄：$TMP"; else rm -rf "$TMP"; fi
}
trap cleanup EXIT

CFG="$TMP/kairos.toml"
cat >"$CFG" <<EOF
[server]
listen = "127.0.0.1:$AGENT_PORT"
cert_path = "$REPO/kairos/testdata/pki/server.crt"
key_path = "$REPO/kairos/testdata/pki/server.key"

[storage]
data_dir = "$TMP/data"
max_total_bytes = 0

[log]
level = "info"
stdout = false

[console]
listen = "127.0.0.1:$CONSOLE_PORT"
cert_path = "$REPO/pulse/testdata/server.pem"
key_path = "$REPO/pulse/testdata/server.key"

[accounts]
lockout_threshold = 3
lockout_minutes = 15
argon2_memory_kib = 8192
argon2_iterations = 1

[alerts]
enabled = true

# S21 的測試：設定檔的值（主控台修改後「改用設定檔的值」）；模式維持內建預設（關閉）
[enroll]
listen = "127.0.0.1:$ENROLL_PORT"
auto_pause_after_hours = 36
EOF

PW='correct horse battery staple'
create() { # 帳號 角色...
    local user="$1"; shift
    local args=()
    for r in "$@"; do args+=(--role "$r"); done
    printf '%s' "$PW" | "$KAIROS_BIN" users create "$user" "${args[@]}" --password-stdin --config "$CFG" >/dev/null
}
create admin admin
create operator operator
create auditor auditor
create viewer viewer
create mfauser viewer
create lockme viewer
create sessuser viewer
# 沒有任何角色的帳號：看不到任何監控畫面
create noroles viewer
"$KAIROS_BIN" users revoke-role noroles --role viewer --yes --config "$CFG" >/dev/null
# 管理員建立、沒有自行指定密碼：一次性密碼只在 stdout，第一次登入必須變更
NEWBIE_PW="$("$KAIROS_BIN" users create newbie --role viewer --config "$CFG" 2>/dev/null | tail -n 1 | tr -d '[:space:]')"

# S21 的測試：環境變數鎖定的設定（主控台唯讀、API 409 locked_by_environment）
KAIROS__ENROLL__AUTO_PAUSE_ENABLED=true "$KAIROS_BIN" --config "$CFG" >"$TMP/kairos.out" 2>&1 &
KAIROS_PID=$!
for _ in $(seq 1 100); do
    if curl -sk "https://127.0.0.1:$CONSOLE_PORT/healthz" >/dev/null 2>&1; then break; fi
    sleep 0.2
done
curl -sk "https://127.0.0.1:$CONSOLE_PORT/healthz" >/dev/null || { cat "$TMP/kairos.out" >&2; exit 1; }

# 註冊與憑證（S12～S15）用的第二個 Kairos
# shellcheck source=pki-env.sh
. "$HERE/pki-env.sh"

mkdir -p "$OUT_DIR"
export KAIROS_URL="https://localhost:$CONSOLE_PORT" AGENT_PORT PW NEWBIE_PW OUT_DIR PW_DIR KAIROS_LOG="$TMP/kairos.out"
NODE_PATH="$PW_DIR/node_modules" "$NODE" "$HERE/e2e.mjs"
