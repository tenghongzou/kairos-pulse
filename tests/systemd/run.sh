#!/bin/sh
# Pulse 遠端升級的 systemd 系統層測試（crates/kairos-proto/REMOTE.md 第 12.3 節；說明見同目錄的 README.md）。
#
# 在有 docker 的 Linux 主機（kp-build）上、repo 的 git clone 內執行：
#   sh tests/systemd/run.sh
# 環境變數：
#   WORK=<目錄>         工作目錄（發佈物、金鑰、日誌；預設 ~/kp-systest，不可在 git repo 內）
#   SKIP_BUILD=1        不執行 build.sh（沿用 WORK/dist）
#   SCENARIOS="…"       要執行的情境（預設全部，依序）：rollback-first upgrade rollback reboot reboot-rollback signature
#                       （none：只建立環境，搭配 KEEP=1 手動檢查）
#   REUSE=1             沿用 KEEP=1 留下的環境（不重建容器、不重新安裝），只執行 SCENARIOS
#   KEEP=1              結束時保留容器與網路（除錯用；之後以 CLEAN_ONLY=1 清除）
#   CLEAN_ONLY=1        只清除這個測試建立的容器、網路與映像
#   CLEAN_IMAGES=1      結束時另外刪除測試映像（預設保留以加速下次執行）
#   API_PORT=19443      Kairos 主控台 API 在主機上的連接埠（只綁 127.0.0.1）
#   HEALTH_TIMEOUT=120  推送的健康確認期限（秒，120～3600）
# 結束代碼：0 全部通過；1 有失敗（摘要在最後與 WORK/results.txt）。
#
# 需要：docker、git、curl、jq、ssh-keygen（OpenSSH 8.1 以上）、sha256sum。
set -eu

HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)
WORK=${WORK:-$HOME/kp-systest}
P=${PREFIX:-kp-systest}
NET=$P-net
API_PORT=${API_PORT:-19443}
HEALTH_TIMEOUT=${HEALTH_TIMEOUT:-120}
SCENARIOS=${SCENARIOS:-rollback-first upgrade rollback reboot reboot-rollback signature}
HOSTS="deb rpm tar"
V0=$(sed -n 's/^version[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' "$REPO/pulse/Cargo.toml" | head -n 1)
# 測試用的較新版本：<V0 的下一個 PATCH>-test.N（例如 0.10.0 → 0.10.1-test.1；與 build.sh 相同的算法），
# 一定比 HEAD 新，不會被當成降版
TV=$(echo "$V0" | awk -F. '{ printf "%d.%d.%d", $1, $2, $3 + 1 }')
T1=$TV-test.1
T2=$TV-test.2
T3=$TV-test.3
T4=$TV-test.4
T9=$TV-test.9

say() { printf '\n==> %s\n' "$*" >&2; }
info() { printf '    %s\n' "$*" >&2; }
die() {
    echo "錯誤：$*" >&2
    exit 1
}

cleanup() {
    for c in $(docker ps -aq --filter "name=^$P-"); do docker rm -f "$c" >/dev/null 2>&1 || true; done
    docker network rm "$NET" >/dev/null 2>&1 || true
    if [ -n "${CLEAN_IMAGES:-}" ]; then
        for i in debian12 ubuntu2404 rocky9; do docker rmi "$P-$i" >/dev/null 2>&1 || true; done
    fi
}

if [ -n "${CLEAN_ONLY:-}" ]; then
    CLEAN_IMAGES=1 cleanup
    echo "已清除 $P-* 的容器、網路 $NET 與測試映像"
    exit 0
fi

for t in docker git curl jq ssh-keygen sha256sum; do
    command -v "$t" >/dev/null 2>&1 || die "找不到 $t"
done
mkdir -p "$WORK"
WORK=$(cd "$WORK" && pwd)
LOGS=$WORK/logs
rm -rf "$LOGS"
mkdir -p "$LOGS"
RESULTS=$WORK/results.txt
: >"$RESULTS"
FAILS=0

pass() {
    echo "PASS  $*" | tee -a "$RESULTS" >&2
}
fail() {
    echo "FAIL  $*" | tee -a "$RESULTS" >&2
    FAILS=$((FAILS + 1))
}
check() {
    # check <說明> <實際> <預期（grep -E）>
    if printf '%s' "$2" | grep -Eq -- "$3"; then
        pass "$1：$2"
    else
        fail "$1：實際「$2」，預期 /$3/"
    fi
}

finish() {
    code=$?
    if [ -z "${KEEP:-}" ]; then
        collect_logs final >/dev/null 2>&1 || true
        cleanup
    else
        echo "KEEP=1：保留容器（docker ps --filter name=$P-）；清除：CLEAN_ONLY=1 sh $0" >&2
    fi
    exit "$code"
}

kx() { docker exec -w /opt/kairos "$P-kairos" setpriv --reuid kairos --regid kairos --init-groups ./kairos "$@" --config kairos.toml; }
# api <方法> <路徑> [JSON 本文] → stdout 為 HTTP 狀態碼，本文在 $WORK/api.json
api() {
    if [ $# -ge 3 ]; then
        curl -sk -o "$WORK/api.json" -w '%{http_code}' -X "$1" -H "Authorization: Bearer $TOKEN" \
            -H 'Content-Type: application/json' --data-binary "$3" "https://127.0.0.1:$API_PORT$2"
    else
        curl -sk -o "$WORK/api.json" -w '%{http_code}' -X "$1" -H "Authorization: Bearer $TOKEN" \
            "https://127.0.0.1:$API_PORT$2"
    fi
}
body() { cat "$WORK/api.json"; }

# import_release <目錄> <版本>：建立版本並上傳 tar.gz、deb、rpm；stdout 為建立版本的 HTTP 狀態碼
import_release() {
    manifest=$(ls "$1"/pulse-*-release.json)
    req=$(jq -n --rawfile s "$1/SHA256SUMS" --rawfile g "$1/SHA256SUMS.sig" --rawfile m "$manifest" \
        '{sha256sums: $s, signature: $g, manifest: $m}')
    s=$(api POST /api/v1/releases "$req")
    if [ "$s" = 201 ]; then
        for f in "$1"/pulse-*-linux-x86_64.tar.gz "$1"/pulse-*-linux-x86_64.deb "$1"/pulse-*-linux-x86_64.rpm; do
            n=$(basename "$f")
            u=$(curl -sk -o "$WORK/upload.json" -w '%{http_code}' -X PUT -H "Authorization: Bearer $TOKEN" \
                -H 'Content-Type: application/octet-stream' --data-binary "@$f" \
                "https://127.0.0.1:$API_PORT/api/v1/releases/$2/artifacts/$n")
            [ "$u" = 200 ] || die "上傳 $n 失敗（$u）：$(cat "$WORK/upload.json")"
        done
    fi
    echo "$s"
}

image_of() {
    case "$1" in deb) echo "$P-debian12" ;; rpm) echo "$P-rocky9" ;; tar) echo "$P-ubuntu2404" ;; esac
}
hx() {
    h=$1
    shift
    docker exec "$P-$h" "$@"
}
wait_boot() {
    i=0
    while [ $i -lt 90 ]; do
        s=$(docker exec "$P-$1" systemctl is-system-running 2>/dev/null || true)
        case "$s" in running | degraded) return 0 ;; esac
        sleep 1
        i=$((i + 1))
    done
    die "$P-$1 的 systemd 沒有完成開機（$s）"
}

agent_of() { eval "echo \$AGENT_$1"; }

# collect_logs <標籤>：每台主機的 Pulse 日誌、輔助程式日誌、狀態
collect_logs() {
    for h in $HOSTS; do
        f=$LOGS/$1-$h.log
        {
            echo "## pulse --version"
            hx "$h" /opt/pulse/pulse --version 2>&1 || true
            echo "## pulse upgrade status"
            hx "$h" /opt/pulse/pulse upgrade status 2>&1 || true
            echo "## systemctl status"
            hx "$h" systemctl status pulse.service pulse-upgrade.service pulse-upgrade.path --no-pager 2>&1 || true
            echo "## journalctl -u pulse-upgrade"
            hx "$h" journalctl -u pulse-upgrade --no-pager -o short-iso 2>&1 | tail -n 200 || true
            echo "## journalctl -u pulse"
            hx "$h" journalctl -u pulse --no-pager -o short-iso 2>&1 | tail -n 60 || true
            echo "## /var/lib/pulse-upgrade/*/upgrade.log"
            hx "$h" sh -c 'cat /var/lib/pulse-upgrade/*/upgrade.log' 2>&1 || true
            echo "## logs/pulse-*.log（upgrade）"
            hx "$h" sh -c 'grep -h -E "upgrade|升級|回滾" /opt/pulse/logs/pulse-*.log | tail -n 80' 2>&1 || true
        } >"$f"
    done
    docker exec "$P-kairos" sh -c 'tail -n 400 /opt/kairos/logs/stdout.log' >"$LOGS/$1-kairos.log" 2>&1 || true
}

pkg_version() {
    # shellcheck disable=SC2016 # ${Version} 是 dpkg-query 的格式，不是 shell 變數
    case "$1" in
    deb) hx deb dpkg-query -W -f='${Version}' pulse 2>&1 ;;
    rpm) hx rpm rpm -q --qf '%{VERSION}-%{RELEASE}' pulse 2>&1 ;;
    tar) echo "-" ;;
    esac
}

# rollout <名稱> <版本>：建立並送出推送（單一批次、失敗不暫停），stdout 為推送 ID
rollout() {
    # shellcheck disable=SC2086 # AGENTS 刻意以空白分隔
    ids=$(printf '%s\n' $AGENTS | jq -R . | jq -s -c .)
    spec=$(jq -n -c --arg n "$1" --arg v "$2" --argjson ids "$ids" --argjson t "$HEALTH_TIMEOUT" \
        '{name: $n, target_version: $v, selector: {agent_ids: $ids},
          waves: [{size: "rest", max_failure_ratio: 1}], options: {health_timeout_secs: $t}}')
    s=$(api POST /api/v1/rollouts/preview "$spec")
    [ "$s" = 200 ] || die "預覽推送 $1 失敗（$s）：$(body)"
    n=$(body | jq -r .data.eligible)
    [ "$n" = 3 ] || die "推送 $1 可升級的主機不是 3 台（$n）：$(body | jq -c .data)"
    s=$(api POST /api/v1/rollouts "$spec")
    [ "$s" = 201 ] || die "建立推送 $1 失敗（$s）：$(body)"
    id=$(body | jq -r .data.id)
    s=$(api POST "/api/v1/rollouts/$id/submit" "$(jq -n -c --arg c "升級 $n 台" '{confirm: $c}')")
    [ "$s" = 200 ] || die "送出推送 $1 失敗（$s）：$(body)"
    echo "$id"
}

# host_state <推送> <主機>：「state code detail」
host_state() {
    api GET "/api/v1/rollouts/$1/hosts" >/dev/null
    body | jq -r --arg a "$(agent_of "$2")" '.data[] | select(.agent_id == $a) | "\(.state) \(.code // "-") \(.detail // "")"'
}

terminal() {
    case "$1" in
    succeeded* | failed* | rolled_back* | rollback_failed* | declined* | expired* | skipped* | cancelled* | unknown*) return 0 ;;
    esac
    return 1
}

# wait_rollout <推送> <秒>：等到所有主機到達終止狀態
wait_rollout() {
    deadline=$(($(date +%s) + $2))
    while [ "$(date +%s)" -lt "$deadline" ]; do
        done_all=1
        for h in $HOSTS; do
            terminal "$(host_state "$1" "$h")" || done_all=0
        done
        [ "$done_all" = 1 ] && return 0
        sleep 5
    done
    return 1
}

# reboot_when <主機> <輔助程式的步驟>：輔助程式的 state.json 到達該步驟時重新開機（docker restart，
# STOPSIGNAL SIGRTMIN+3 讓 systemd 正常關機）
reboot_when() {
    deadline=$(($(date +%s) + 600))
    while [ "$(date +%s)" -lt "$deadline" ]; do
        if hx "$1" sh -c "grep -qs '\"step\": \"$2\"' /var/lib/pulse-upgrade/*/state.json" 2>/dev/null; then
            info "$1：輔助程式在 $2，重新開機（docker restart）"
            docker restart -t 60 "$P-$1" >/dev/null
            wait_boot "$1"
            echo "$1" >>"$WORK/rebooted"
            return 0
        fi
        sleep 1
    done
    info "$1：等不到輔助程式的 $2"
    return 1
}

# verify <情境> <推送> <主機> <預期狀態> <預期代碼> <預期版本> [<套件資料庫的預期版本>]
verify() {
    st=$(host_state "$2" "$3")
    check "$1/$3：Kairos 的主機狀態" "$st" "^$4 $5( |\$)"
    v=$(hx "$3" /opt/pulse/pulse --version 2>&1 | awk '{print $2}')
    check "$1/$3：/opt/pulse/pulse --version" "$v" "^$6\$"
    check "$1/$3：systemctl is-active pulse" "$(hx "$3" systemctl is-active pulse.service 2>&1)" '^active$'
    if [ "$3" != tar ]; then
        pv=$(pkg_version "$3")
        # deb／rpm 的套件版本：0.10.1-test.1 → 0.10.1~test.1-1（nfpm）
        pre=$(printf '%s' "${7:-$6}" | sed 's/-/[~-]/; s/\./\\./g')
        check "$1/$3：套件資料庫的版本" "$pv" "^$pre-1\$"
    fi
    api GET "/api/v1/hosts/$(agent_of "$3")/upgrade" >/dev/null
    check "$1/$3：Kairos 看到的 agent_version" "$(body | jq -r .data.agent_version)" "^$6\$"
    check "$1/$3：request.json 已刪除" \
        "$(hx "$3" sh -c 'test -e /var/lib/pulse/upgrade/request.json && echo exists || echo absent')" '^absent$'
}

helper_log() {
    hx "$1" sh -c 'cat /var/lib/pulse-upgrade/*/upgrade.log' 2>/dev/null || true
}

scenario_rollback_first() {
    need $T2
    say "情境 b0：第一次遠端升級就失敗 $V0 → $T2（本機沒有 $V0 的已驗證發佈 → 以 pulse.prev 換回執行檔）"
    id=$(rollout "systest rollback first" $T2)
    wait_rollout "$id" 1200 || fail "b0：推送沒有在 1200 秒內結束"
    collect_logs b0
    for h in $HOSTS; do
        # deb／rpm：執行檔回到舊版，套件資料庫仍是失敗的那一版（REMOTE.md 第 17.3 節第 1 項）
        verify b0 "$id" "$h" rolled_back PLS-W8407 "$V0" $T2
        check "b0/$h：輔助程式的回滾方式" "$(helper_log "$h" | grep -o '以替換前保留的 [^ ]*' | head -n 1)" \
            '以替換前保留的 /opt/pulse/pulse.prev'
    done
    for h in deb rpm; do
        check "b0/$h：detail 註明套件資料庫的版本" "$(host_state "$id" "$h")" "binary_restored_package_db_shows_$T2"
    done
}

scenario_upgrade() {
    need $T1
    say "情境 a：成功升級 $V0 → $T1（deb：dpkg -i、rpm：rpm -U、tar：pulse.prev＋改名；接在 b0 之後時套件資料庫為 $T2）"
    id=$(rollout "systest upgrade" $T1)
    wait_rollout "$id" 900 || fail "a：推送沒有在 900 秒內結束"
    collect_logs a
    for h in $HOSTS; do
        verify a "$id" "$h" succeeded PLS-I8416 $T1
    done
    check "a/tar：pulse.prev 為舊版" "$(hx tar /opt/pulse/pulse.prev --version 2>&1 | awk '{print $2}')" "^$V0\$"
    api GET "/api/v1/rollouts/$id" >/dev/null
    check "a：推送狀態" "$(body | jq -r .data.state)" '^completed$'
}

scenario_rollback() {
    need $T2
    say "情境 b：失敗回滾 $T1 → $T2（新版一啟動就崩潰 → 自動回滾，PLS-W8407）"
    id=$(rollout "systest rollback" $T2)
    wait_rollout "$id" 1200 || fail "b：推送沒有在 1200 秒內結束"
    collect_logs b
    for h in $HOSTS; do
        verify b "$id" "$h" rolled_back PLS-W8407 $T1
        check "b/$h：健康確認失敗的原因" "$(host_state "$id" "$h")" 'crash_restart|exit_code|health_timeout'
    done
    for h in deb rpm; do
        # 第一次遠端升級時保存了 test.1 的已驗證 SHA256SUMS，這次以套件管理員裝回（不是 pulse.prev）
        d=$(host_state "$id" "$h")
        if printf '%s' "$d" | grep -q binary_restored; then
            fail "b/$h：應以 rollback_artifact 經套件管理員回滾，實際只換回執行檔：$d"
        else
            pass "b/$h：以 rollback_artifact 經套件管理員回滾（detail 沒有 binary_restored）"
        fi
        check "b/$h：輔助程式的回滾方式" "$(helper_log "$h" | grep -o '經套件管理員裝回.*' | head -n 1)" \
            "經套件管理員裝回 $T1（(dpkg -i|rpm -U --oldpackage --replacepkgs)）"
    done
}

scenario_reboot() {
    need $T3
    say "情境 c：重新開機接續 $T1 → $T3（健康確認期間重新開機 → 開機後接續健康確認 → succeeded）"
    rm -f "$WORK/rebooted"
    id=$(rollout "systest reboot" $T3)
    pids=""
    for h in $HOSTS; do
        reboot_when "$h" replaced &
        pids="$pids $!"
    done
    for p in $pids; do wait "$p" || true; done
    for h in $HOSTS; do
        if grep -qx "$h" "$WORK/rebooted" 2>/dev/null; then
            pass "c/$h：已在健康確認期間重新開機"
        else
            fail "c/$h：沒有在健康確認期間重新開機"
        fi
    done
    wait_rollout "$id" 1200 || fail "c：推送沒有在 1200 秒內結束"
    collect_logs c
    for h in $HOSTS; do
        verify c "$id" "$h" succeeded PLS-I8416 $T3
        check "c/$h：輔助程式接續" "$(helper_log "$h" | grep -c '接續上次中斷的升級')" '^[1-9]'
    done
}

scenario_reboot_rollback() {
    need $T4
    say "情境 c2：回滾期間重新開機 $T3 → $T4（崩潰 → 回滾中重新開機 → 開機後接續回滾確認 → rolled_back）"
    rm -f "$WORK/rebooted"
    id=$(rollout "systest reboot rollback" $T4)
    pids=""
    for h in $HOSTS; do
        reboot_when "$h" rolling_back &
        pids="$pids $!"
    done
    for p in $pids; do wait "$p" || true; done
    for h in $HOSTS; do
        if grep -qx "$h" "$WORK/rebooted" 2>/dev/null; then
            pass "c2/$h：已在回滾確認期間重新開機"
        else
            fail "c2/$h：沒有在回滾確認期間重新開機"
        fi
    done
    wait_rollout "$id" 1200 || fail "c2：推送沒有在 1200 秒內結束"
    collect_logs c2
    for h in $HOSTS; do
        verify c2 "$id" "$h" rolled_back PLS-W8407 $T3
        check "c2/$h：輔助程式接續回滾" "$(helper_log "$h" | grep -c '接續上次中斷的回滾')" '^[1-9]'
        check "c2/$h：接續後仍回報原本的回滾原因" "$(host_state "$id" "$h")" 'crash_restart.*; resumed'
    done
}

scenario_signature() {
    need evil-a $T9
    say "情境 d：簽章錯誤"
    s=$(import_release "$DIST/evil-a" $T1)
    check "d1：Kairos 拒絕沒有信任的金鑰簽的發佈（HTTP 狀態碼）" "$s" '^400$'
    check "d1：錯誤碼" "$(body | jq -r '.error.code // .code // empty')" 'release_signature_invalid'
    s=$(import_release "$DIST/$T9" $T9)
    check "d2：Kairos 匯入只有 Kairos 信任的金鑰簽的發佈" "$s" '^201$'
    [ "$s" = 201 ] || return 0
    before=$(for h in $HOSTS; do hx "$h" /opt/pulse/pulse --version | awk '{print $2}'; done | tr '\n' ' ')
    id=$(rollout "systest signature" $T9)
    # 第一台回報 PLS-E8401 時整個推送就暫停（REMOTE.md 第 4.7.4 節第 3 項），其餘主機可能還沒被提議
    deadline=$(($(date +%s) + 600))
    rstate=running
    while [ "$(date +%s)" -lt "$deadline" ]; do
        api GET "/api/v1/rollouts/$id" >/dev/null
        rstate=$(body | jq -r .data.state)
        [ "$rstate" = running ] || break
        sleep 5
    done
    sleep 10
    collect_logs d
    api GET "/api/v1/rollouts/$id" >/dev/null
    check "d2：推送自動暫停" "$(body | jq -r '"\(.data.state) \(.data.pause_reason // "")"')" '^paused .*PLS-E8401'
    rejected=0
    for h in $HOSTS; do
        st=$(host_state "$id" "$h")
        case "$st" in
        "failed PLS-E8401"*)
            rejected=$((rejected + 1))
            pass "d2/$h：Pulse 拒絕：$st"
            check "d2/$h：Pulse 的日誌" "$(hx "$h" sh -c 'grep -h "upgrade.verify_failed PLS-E8401" /opt/pulse/logs/pulse-*.log | tail -n 1' | grep -o 'upgrade.verify_failed PLS-E8401[^（]*')" \
                'upgrade.verify_failed PLS-E8401'
            ;;
        pending* | offered*) pass "d2/$h：推送暫停後沒有再提議：$st" ;;
        *) fail "d2/$h：預期 failed PLS-E8401 或尚未提議，實際「$st」" ;;
        esac
    done
    check "d2：至少一台回報 PLS-E8401" "$rejected" '^[1-9]'
    after=$(for h in $HOSTS; do hx "$h" /opt/pulse/pulse --version | awk '{print $2}'; done | tr '\n' ' ')
    check "d2：版本沒有改變" "$after" "^$before\$"
}

# ---- 0. 發佈物 --------------------------------------------------------------
if [ -z "${SKIP_BUILD:-}" ]; then
    say "建置發佈物（tests/systemd/build.sh）"
    (cd "$REPO" && sh tests/systemd/build.sh "$WORK")
fi
DIST=$WORK/dist
KEYS=$WORK/keys
need() {
    for d in "$@"; do
        [ -f "$DIST/$d/SHA256SUMS" ] || die "缺少 $DIST/$d（先執行 build.sh）"
    done
}
need kairos "$V0"

setup_images() {
    say "建立測試映像"
    docker build -q -t "$P-debian12" --build-arg BASE=debian:12 -f "$HERE/images/debian.Dockerfile" "$HERE/images" >/dev/null
    docker build -q -t "$P-ubuntu2404" --build-arg BASE=ubuntu:24.04 -f "$HERE/images/debian.Dockerfile" "$HERE/images" >/dev/null
    docker build -q -t "$P-rocky9" -f "$HERE/images/rocky.Dockerfile" "$HERE/images" >/dev/null
    info "$P-debian12（deb）、$P-rocky9（rpm）、$P-ubuntu2404（tar）"
}

setup_kairos() {
    say "啟動 Kairos（kairos.test：8080 回報、8444 註冊、9443 主控台 → 127.0.0.1:$API_PORT）"
    docker network create "$NET" >/dev/null
    docker run -d --name "$P-kairos" --hostname kairos.test --network "$NET" --network-alias kairos.test \
        -p "127.0.0.1:$API_PORT:9443" debian:12 sleep infinity >/dev/null
    docker cp "$(ls "$DIST"/kairos/kairos-*-linux-x86_64.tar.gz)" "$P-kairos:/tmp/kairos.tar.gz"
    docker cp "$KEYS/kairos_allowed_signers" "$P-kairos:/tmp/kairos_allowed_signers"
    docker exec "$P-kairos" sh -euc '
useradd --system --user-group --home-dir /opt/kairos --no-create-home --shell /usr/sbin/nologin kairos
install -d -o root -g kairos -m 750 /opt/kairos
tar xzf /tmp/kairos.tar.gz -C /opt/kairos --strip-components=1
install -o root -g kairos -m 644 /tmp/kairos_allowed_signers /opt/kairos/allowed_signers
install -d -o kairos -g kairos -m 750 /opt/kairos/data /opt/kairos/logs
install -d -o kairos -g kairos -m 700 /opt/kairos/root-ca
cat > /opt/kairos/kairos.toml <<EOF
[server]
listen = "0.0.0.0:8080"
cert_path = "data/pki/server.pem"
key_path = "data/pki/server.key"

[storage]
data_dir = "data"

[auth]
client_ca = "data/pki/issuing-cas.pem"

[enroll]
mode = "open"
advertise = "kairos.test:8444"

[pki]
mode = "online"
server_ca_path = "data/pki/root-ca.pem"

[console]
listen = "0.0.0.0:9443"

[log]
level = "debug"

[remote]
allowed_signers = "/opt/kairos/allowed_signers"
EOF
chown root:kairos /opt/kairos/kairos.toml
chmod 640 /opt/kairos/kairos.toml
cd /opt/kairos
setpriv --reuid kairos --regid kairos --init-groups ./kairos pki init --config kairos.toml \
    --name kairos.test --root-dir /opt/kairos/root-ca >/tmp/pki-init.log 2>&1
'
    docker exec -d -w /opt/kairos "$P-kairos" sh -c \
        'exec setpriv --reuid kairos --regid kairos --init-groups ./kairos --config kairos.toml >>logs/stdout.log 2>&1'
    i=0
    until curl -sk "https://127.0.0.1:$API_PORT/healthz" >/dev/null 2>&1 || [ $i -ge 60 ]; do
        sleep 1
        i=$((i + 1))
    done
    [ $i -lt 60 ] || die "Kairos 沒有啟動：$(docker exec "$P-kairos" tail -20 /opt/kairos/logs/stdout.log)"
}

setup_hosts() {
    say "啟動 systemd 測試主機並安裝 Pulse $V0"
    for h in $HOSTS; do
        docker run -d --name "$P-$h" --hostname "pulse-$h" --privileged --cgroupns=private --network "$NET" \
            "$(image_of "$h")" >/dev/null
    done
    for h in $HOSTS; do
        wait_boot "$h"
        docker cp "$DIST/$V0" "$P-$h:/root/dist"
        docker cp "$KEYS/allowed_signers" "$P-$h:/root/allowed_signers"
    done
    # deb、rpm：一行安裝腳本（離線 --dir：驗證簽章 → 安裝套件 → 信任錨 → pulse enroll → 啟動）
    for h in deb rpm; do
        docker exec -e PULSE_INSTALL_STRING="$INSTALL_STRING" "$P-$h" \
            sh /root/dist/pulse-install.sh --dir /root/dist --allowed-signers /root/allowed_signers \
            >"$LOGS/install-$h.log" 2>&1 || die "$h：pulse-install.sh 失敗（$LOGS/install-$h.log）"
    done
    # tar：INSTALL.md 第 3、4、5.0、7、10.1 節
    docker exec -e IS="$INSTALL_STRING" "$P-tar" sh -euc '
cd /root/dist
mkdir -p pkg
tar xzf pulse-*-linux-x86_64.tar.gz -C pkg --strip-components=1
pkg/pulse --version
useradd --system --user-group --home-dir /opt/pulse --no-create-home --shell /usr/sbin/nologin pulse
install -d -o root -g pulse -m 751 /opt/pulse
install -d -o pulse -g pulse -m 750 /opt/pulse/logs
install -d -o pulse -g pulse -m 750 /opt/pulse/buffer
install -d -o pulse -g pulse -m 700 /opt/pulse/identity
cd /root/dist/pkg
install -o root -g root -m 755 pulse /opt/pulse/pulse
install -o root -g root -m 644 pulse.example.toml INSTALL.md README.md PROTOCOL.md CHANGELOG.md ERROR-CODES.md \
    THIRD-PARTY-LICENSES /opt/pulse/
cd /opt/pulse
./pulse --print-agent-id
chown root:pulse agent.id && chmod 640 agent.id
printf "%s\n" "$IS" | ./pulse enroll --install-string-file - --non-interactive
[ ! -f pulse.toml ] || { chown root:pulse pulse.toml; chmod 640 pulse.toml; }
install -o root -g root -m 644 /root/dist/pkg/pulse.service /etc/systemd/system/pulse.service
install -d -m 755 /etc/kairos-pulse
install -o root -g root -m 644 /root/allowed_signers /etc/kairos-pulse/allowed_signers
install -o root -g root -m 644 /root/dist/pkg/pulse-upgrade.path /root/dist/pkg/pulse-upgrade.service /etc/systemd/system/
systemctl daemon-reload
systemctl enable --now pulse-upgrade.path
systemctl enable --now pulse.service
' >"$LOGS/install-tar.log" 2>&1 || die "tar：安裝失敗（$LOGS/install-tar.log）"
}

if [ -z "${REUSE:-}" ]; then
    cleanup
fi
trap finish EXIT
trap 'exit 130' INT TERM

if [ -z "${REUSE:-}" ]; then
    setup_images
    setup_kairos
fi
TOKEN=$(kx api-token create --name "systest-$(date +%s)" --role admin --json | jq -r .data.token)
[ -n "$TOKEN" ] && [ "$TOKEN" != null ] || die "無法建立 API token"

say "匯入版本庫：$V0、$T1、$T2、$T3、$T4（臨時金鑰 release 簽章）"
for v in "$V0" $T1 $T2 $T3 $T4; do
    [ -f "$DIST/$v/SHA256SUMS.sig" ] || continue
    s=$(import_release "$DIST/$v" "$v")
    case "$s" in
    201) ;;
    409) [ -n "${REUSE:-}" ] || die "匯入 $v 失敗（$s）：$(body)" ;;
    *) die "匯入 $v 失敗（$s）：$(body)" ;;
    esac
done
info "已匯入"

if [ -z "${REUSE:-}" ]; then
    SUBNET=$(docker network inspect "$NET" -f "{{(index .IPAM.Config 0).Subnet}}")
    # 自動核准的 token 必須限定來源網段：測試網路的子網路
    INSTALL_STRING=$(kx enroll-token create --name systest --group default --max-uses 20 --approval auto \
        --allow-cidr "$SUBNET" --json | jq -r .data.install_string)
    case "$INSTALL_STRING" in pulse-enroll:v1:*) ;; *) die "無法建立註冊 token：$INSTALL_STRING" ;; esac
    setup_hosts
fi

AGENTS=""
for h in $HOSTS; do
    a=$(hx "$h" cat /opt/pulse/agent.id | tr -d '\r\n ')
    eval "AGENT_$h=$a"
    AGENTS="$AGENTS $a"
done

say "確認 hello 宣告 upgrade（install.upgrade = ready）"
ver_re=$V0
[ -z "${REUSE:-}" ] || ver_re=".+"
for h in $HOSTS; do
    a=$(agent_of "$h")
    i=0
    state=""
    while [ $i -lt 90 ]; do
        api GET "/api/v1/hosts/$a/upgrade" >/dev/null
        state="$(body | jq -r '[.data.install.method, .data.install.upgrade, (.data.upgrade_negotiated|tostring), .data.agent_version] | join(" ")' 2>/dev/null || true)"
        if printf "%s" "$state" | grep -Eq "^$h ready true $ver_re$"; then break; fi
        sleep 2
        i=$((i + 1))
    done
    check "$h：install（method upgrade negotiated version）" "$state" "^$h ready true $ver_re$"
done
[ "$FAILS" = 0 ] || die "測試主機沒有就緒（$(hx deb /opt/pulse/pulse upgrade status 2>&1 | tail -5)）"

for s in $SCENARIOS; do
    case "$s" in
    rollback-first) scenario_rollback_first ;;
    upgrade) scenario_upgrade ;;
    rollback) scenario_rollback ;;
    reboot) scenario_reboot ;;
    reboot-rollback) scenario_reboot_rollback ;;
    signature) scenario_signature ;;
    none) ;;
    *) die "未知的情境 $s" ;;
    esac
done

# 段落請求不超過 Kairos 每條連線的訊息速率（可連續 50 則、之後每秒 20 則）：正常的下載不應觸發 WARN
check "Kairos 沒有「送出訊息超過每秒 20 則」的 WARN" \
    "$(docker exec "$P-kairos" sh -c 'grep -c "送出訊息超過每秒" /opt/kairos/logs/stdout.log || true')" '^0$'

say "結果（$RESULTS；日誌在 $LOGS）"
info "通過 $(grep -c '^PASS' "$RESULTS")、失敗 $FAILS"
[ "$FAILS" = 0 ]
