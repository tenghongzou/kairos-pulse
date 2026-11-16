#!/bin/sh
# Pulse 遠端升級的 systemd 系統層測試（crates/kairos-proto/REMOTE.md 第 12.3 節；說明見同目錄的 README.md）。
#
# 在有 docker 的 Linux 主機（kp-build）上、repo 的 git clone 內執行：
#   sh tests/systemd/run.sh
# 環境變數：
#   WORK=<目錄>         工作目錄（發佈物、金鑰、日誌；預設 ~/kp-systest，不可在 git repo 內）
#   SKIP_BUILD=1        不執行 build.sh（沿用 WORK/dist）
#   SCENARIOS="…"       要執行的情境（預設全部，依序）：rollback-first upgrade rollback archive-tamper reboot
#                       reboot-rollback signature tamper degraded tamper-resume crash-replace crash-rollback
#                       unsafe-path late-resume key-revoked pkg-rollback-fails ansible
#                       （none：只建立環境，搭配 KEEP=1 手動檢查）
#   LATE_AFTER=30       情境 late-resume：主機在健康確認的期限過後幾秒才開機
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
SCENARIOS=${SCENARIOS:-rollback-first upgrade rollback archive-tamper reboot reboot-rollback signature tamper degraded tamper-resume crash-replace crash-rollback unsafe-path late-resume key-revoked pkg-rollback-fails ansible}
HOSTS="deb rpm tar"
V0=$(sed -n 's/^version[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' "$REPO/pulse/Cargo.toml" | head -n 1)
# 測試用的較新版本：<V0 的下一個 PATCH>-test.N（例如 0.10.0 → 0.10.1-test.1；與 build.sh 相同的算法），
# 一定比 HEAD 新，不會被當成降版
TV=$(echo "$V0" | awk -F. '{ printf "%d.%d.%d", $1, $2, $3 + 1 }')
T1=$TV-test.1
T2=$TV-test.2
T3=$TV-test.3
T4=$TV-test.4
# test.5、test.6：開啟 fault-injection 的測試用建置（情境 crash-replace、crash-rollback）；test.7：崩潰
T5=$TV-test.5
T6=$TV-test.6
T7=$TV-test.7
# test.8：正常的版本，高於 test.7（情境 late-resume 的目標）
T8=$TV-test.8
T9=$TV-test.9
# test.10：崩潰，以第二把發佈金鑰 release2 簽章，prerm／%preun 在有記號檔時失敗（情境 key-revoked、
# pkg-rollback-fails）
T10=$TV-test.10
# 情境 late-resume：主機在健康確認的期限過後多久才開機（秒）
LATE_AFTER=${LATE_AFTER:-30}
# 輔助程式 root 專用的工作目錄
HW=/var/lib/pulse-upgrade

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
        for i in debian12-managed rocky9-managed ansible debian12 ubuntu2404 rocky9; do docker rmi "$P-$i" >/dev/null 2>&1 || true; done
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
            echo "## /var/lib/pulse-upgrade（交易紀錄與指標）"
            hx "$h" sh -c 'ls -laR /var/lib/pulse-upgrade; for f in /var/lib/pulse-upgrade/active.json /var/lib/pulse-upgrade/*/state.json; do [ -f "$f" ] && { echo "# $f"; cat "$f"; echo; }; done' 2>&1 || true
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

# archive_entry <主機>：root 的本機封存索引中的條目「版本 來源 generation」（REMOTE.md 第 17.10 節；沒有時空的）
archive_entry() {
    hx "$1" sh -c "cat $HW/archive/index.json 2>/dev/null" |
        jq -r '.entries[]? | "\(.version) \(.source) \(.generation)"' 2>/dev/null || true
}

# rollback_declared <主機>：Kairos 看到的「install.rollback package_repair_needed」
rollback_declared() {
    api GET "/api/v1/hosts/$(agent_of "$1")/upgrade" >/dev/null
    body | jq -r '"\(.data.install.rollback // "-") \(.data.package_repair_needed)"' 2>/dev/null || true
}

# wait_rollback_declared <主機> <預期（grep -E）>：等 Kairos 看到的宣告符合（最多 180 秒）；stdout 為最後的值
wait_rollback_declared() {
    wr=0
    wrs=""
    while [ $wr -lt 90 ]; do
        wrs=$(rollback_declared "$1")
        if printf '%s' "$wrs" | grep -Eq -- "$2"; then break; fi
        sleep 2
        wr=$((wr + 1))
    done
    echo "$wrs"
}

# not_degraded <情境> <推送> <主機>：這台主機的結果不是降級回滾（detail 沒有 degraded:）
not_degraded() {
    if host_state "$2" "$3" | grep -q 'degraded:'; then
        fail "$1/$3：不應該是降級回滾：$(host_state "$2" "$3")"
    else
        pass "$1/$3：不是降級回滾（detail 沒有 degraded:）"
    fi
}

# 情境 b0（ADR-0003 階段 1、2；REMOTE.md 第 4.4.2 節）：**第一次**遠端升級就失敗。0.11.0 以前三台都只能以
# pulse.prev 換回執行檔（deb／rpm 的套件資料庫停在失敗的版本）；現在 deb／rpm 經套件管理員回滾：
#   deb：pulse-install.sh 安裝時留下的種子（root 的本機封存）
#   rpm：先移除種子（模擬手動安裝套件、主機上什麼紀錄都沒有），以提議的 rollback_bundle 驗證回滾檔
#   tar：pulse.prev（不需要回滾檔）
scenario_rollback_first() {
    need $T2
    say "情境 b0：第一次遠端升級就失敗 $V0 → $T2（deb：安裝時的種子封存；rpm：提議的 rollback_bundle；tar：pulse.prev）"
    v0_re=$(printf '%s' "$V0" | sed 's/\./\\./g')
    check "b0/deb：安裝腳本留下的種子封存" "$(archive_entry deb)" "^$v0_re seed 1\$"
    check "b0/deb：安裝腳本留下的本機紀錄（服務帳號擁有）" \
        "$(hx deb stat -c '%U %a' "/var/lib/pulse/upgrade/releases/$V0/SHA256SUMS" "/var/lib/pulse/upgrade/releases/$V0/SHA256SUMS.sig" 2>&1 | tr '\n' ' ')" \
        '^pulse 640 pulse 640 $'
    check "b0/deb：hello 宣告本機封存" "$(wait_rollback_declared deb '^archived false$')" '^archived false$'
    check "b0/deb：pulse upgrade status 的自動回滾" "$(hx deb /opt/pulse/pulse upgrade status 2>&1 | grep '^自動回滾')" '經套件管理員裝回'
    check "b0/tar：hello 宣告只換回執行檔" "$(wait_rollback_declared tar '^binary_only false$')" '^binary_only false$'
    # rpm：移除本機封存、本機紀錄與提示，重新啟動 Pulse
    hx rpm sh -c "rm -rf $HW/archive /var/lib/pulse/upgrade/releases /var/lib/pulse/upgrade/rollback.json"
    hx rpm systemctl restart pulse.service
    check "b0/rpm：移除種子後 hello 宣告只換回執行檔" "$(wait_rollback_declared rpm '^binary_only false$')" '^binary_only false$'
    check "b0/rpm：重新連線後仍宣告 upgrade" "$(wait_install rpm '^rpm ready true ')" '^rpm ready true '
    id=$(rollout "systest rollback first" $T2)
    wait_rollout "$id" 1200 || fail "b0：推送沒有在 1200 秒內結束"
    collect_logs b0
    for h in $HOSTS; do
        # deb／rpm：經套件管理員回滾，套件資料庫也回到 $V0
        verify b0 "$id" "$h" rolled_back PLS-W8407 "$V0"
        not_degraded b0 "$id" "$h"
    done
    check "b0/deb：回滾來源是安裝時的種子封存" \
        "$(helper_log deb | grep -o '回滾來源：本機封存的 .*來源 [a-z]*）' | head -n 1)" '來源 seed）$'
    check "b0/rpm：Pulse 以提議的 rollback_bundle 驗證回滾檔（本機沒有紀錄）" \
        "$(hx rpm sh -c 'cat /opt/pulse/logs/pulse-*.log | grep -c "以提議的 rollback_bundle 驗證通過" || true')" '^[1-9]'
    check "b0/rpm：輔助程式重新驗證回滾檔並綁定 commit 與執行檔" \
        "$(helper_log rpm | grep -c '已驗證（簽署者 .*commit 與套件中的執行檔都與目前安裝的相同）')" '^[1-9]'
    check "b0/rpm：沒有使用本機封存" "$(helper_log rpm | grep -c '回滾來源：本機封存')" '^0$'
    for h in deb rpm; do
        check "b0/$h：經套件管理員回滾" "$(helper_log "$h" | grep -o '經套件管理員裝回.*' | head -n 1)" \
            "經套件管理員裝回 $V0（(dpkg -i|rpm -U --oldpackage --replacepkgs)）"
    done
    check "b0/tar：以 pulse.prev 換回" "$(helper_log tar | grep -o '以替換前保留的 [^ ]*' | head -n 1)" \
        '以替換前保留的 /opt/pulse/pulse.prev'
    api GET "/api/v1/rollouts/$id" >/dev/null
    check "b0：推送沒有因為降級回滾而暫停" "$(body | jq -r '.data.pause_cause // "-"')" '^-$'
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
    # 升級成功後封存新版的套件（REMOTE.md 第 17.10 節）：只有新版一筆、來源 ota；目錄與索引只有 root 可讀寫
    t1_re=$(printf '%s' "$T1" | sed 's/\./\\./g')
    for h in deb rpm; do
        check "a/$h：升級成功後封存了 $T1 的套件" "$(archive_entry "$h")" "^$t1_re ota [0-9]+\$"
        check "a/$h：封存目錄 0700、索引 0600，都屬於 root" \
            "$(hx "$h" stat -c '%a %U:%G' $HW/archive $HW/archive/index.json 2>&1 | tr '\n' ' ')" \
            '^700 root:root 600 root:root $'
        check "a/$h：封存只有一份" "$(hx "$h" sh -c "ls $HW/archive | grep -c -v index.json")" '^1$'
        check "a/$h：hello 宣告本機封存" "$(wait_rollback_declared "$h" '^archived false$')" '^archived false$'
    done
    check "a/tar：不封存" "$(hx tar sh -c "ls -d $HW/archive 2>/dev/null | wc -l")" '^0$'
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
        # 回滾來源是 root 的本機封存（情境 a 升級成功時封存的那一份），不是服務帳號暫存的回滾檔
        check "b/$h：回滾來源是本機封存" \
            "$(helper_log "$h" | grep -o '回滾來源：本機封存的 .*來源 [a-z]*）' | head -n 1)" '來源 ota）$'
        not_degraded b "$id" "$h"
        # 失敗的 $T2 沒有通過健康確認：不進封存
        check "b/$h：封存仍是 $T1" "$(archive_entry "$h")" "^$(printf '%s' "$T1" | sed 's/\./\\./g') ota [0-9]+\$"
    done
}

# 情境 k（ADR-0003 階段 2；REMOTE.md 第 17.10.4 節）：本機封存被竄改。deb：封存的套件被改過（整包雜湊不符）；
# rpm：封存索引被改過（checksum 不符）。輔助程式不使用它，改評估下一個來源——提議附的回滾檔，以本機紀錄或
# rollback_bundle 的簽章資料重新驗證——照樣經套件管理員回滾。tar 不受影響
scenario_archive_tamper() {
    need $T2
    cur=$(hx deb /opt/pulse/pulse --version | awk '{print $2}')
    cur_re=$(printf '%s' "$cur" | sed 's/\./\\./g')
    say "情境 k：本機封存被竄改 $cur → $T2（崩潰 → 不使用封存 → 以驗證過的回滾檔經套件管理員回滾）"
    for h in deb rpm; do
        check "k/$h：竄改前有 $cur 的封存" "$(archive_entry "$h")" "^$cur_re [a-z]+ [0-9]+\$"
    done
    hx deb sh -c "for f in $HW/archive/*/package; do printf x >> \"\$f\"; done"
    hx rpm sh -c "sed -i 's/\"generation\": [0-9]*/\"generation\": 4242/' $HW/archive/index.json"
    id=$(rollout "systest archive tamper" $T2)
    wait_rollout "$id" 1200 || fail "k：推送沒有在 1200 秒內結束"
    collect_logs k
    for h in $HOSTS; do
        verify k "$id" "$h" rolled_back PLS-W8407 "$cur"
        not_degraded k "$id" "$h"
    done
    for h in deb rpm; do
        check "k/$h：輔助程式不使用被竄改的封存" "$(helper_log "$h" | grep -c '本機封存不使用（rollback_archive_unusable）')" '^[1-9]'
        check "k/$h：沒有以封存回滾" "$(helper_log "$h" | grep -c '回滾來源：本機封存')" '^0$'
        check "k/$h：改以重新驗證過的回滾檔回滾" \
            "$(helper_log "$h" | grep -c '已驗證（簽署者 .*commit 與套件中的執行檔都與目前安裝的相同）')" '^[1-9]'
        check "k/$h：經套件管理員回滾" "$(helper_log "$h" | grep -o '經套件管理員裝回.*' | head -n 1)" \
            "經套件管理員裝回 $cur（(dpkg -i|rpm -U --oldpackage --replacepkgs)）"
    done
}

# 情境 l（ADR-0003 階段 1、2；REMOTE.md 第 4.5.2 節）：回滾檔沒有通過輔助程式的驗證 → 升級照常 → 新版崩潰 →
# 以 pulse.prev 緊急復原＝**降級回滾**（rolled_back＋PLS-W8418＋degraded:），主機需要修復套件；修復後恢復；
# 再以 pulse upgrade archive-current 封存目前的版本。不經 Kairos 的推送（與情境 e 相同，以服務帳號自己寫
# request.json）：回滾檔所屬發佈的簽章換成不被信任的金鑰簽的（提示來源由服務帳號暫存，輔助程式重新驗證）
scenario_degraded() {
    need $T4 evil-a
    say "情境 l：回滾檔的簽章沒有通過驗證 → 降級回滾（pulse.prev）→ 需要修復套件 → 修復 → archive-current"
    for th in deb rpm; do
        cur=$(hx "$th" /opt/pulse/pulse --version | awk '{print $2}')
        cur_re=$(printf '%s' "$cur" | sed 's/\./\\./g')
        t4_pkg=$(printf '%s' "$T4" | sed 's/-/[~-]/; s/\./\\./g')
        cur_pkg=$(printf '%s' "$cur" | sed 's/-/[~-]/; s/\./\\./g')
        # 模擬沒有本機封存的主機（手動安裝的套件）
        hx "$th" sh -c "rm -rf $HW/archive /var/lib/pulse/upgrade/rollback.json"
        uid=$(cat /proc/sys/kernel/random/uuid)
        art=$(basename "$(ls "$DIST/$T4"/pulse-*-linux-x86_64."$th")")
        man=$(basename "$(ls "$DIST/$T4"/pulse-*-release.json)")
        rb=$(basename "$(ls "$DIST/$cur"/pulse-*-linux-x86_64."$th")")
        stage=$WORK/degraded-$th
        rm -rf "$stage"
        mkdir -p "$stage"
        cp "$DIST/$T4/$art" "$DIST/$T4/$man" "$DIST/$T4/SHA256SUMS" "$DIST/$T4/SHA256SUMS.sig" "$DIST/$cur/$rb" "$stage/"
        cp "$DIST/$cur/SHA256SUMS" "$stage/rollback.SHA256SUMS"
        # 簽章不是本機信任錨接受的金鑰簽的
        cp "$DIST/evil-a/SHA256SUMS.sig" "$stage/rollback.SHA256SUMS.sig"
        jq -n -c --arg id "$uid" --arg v "$T4" --arg c "$(jq -r .commit "$DIST/$T4/$man")" --arg m "$th" \
            --arg a "$art" --arg man "$man" --arg rb "$rb" --arg from "$cur" \
            --arg at "$(date -u +%Y-%m-%dT%H:%M:%SZ)" --argjson t "$HEALTH_TIMEOUT" \
            '{schema: "pulse.upgrade-request/v1", upgrade_id: $id, target: {version: $v, commit: $c},
              method: $m, artifact: $a, manifest: $man, rollback_artifact: $rb, health_timeout_secs: $t,
              allow_downgrade: false, from_version: $from, requested_at: $at}' >"$stage/request.json"
        chmod -R a+rX "$stage"
        hx "$th" rm -rf /tmp/degraded
        docker cp "$stage" "$P-$th:/tmp/degraded"
        if ! hx "$th" setpriv --reuid pulse --regid pulse --init-groups sh -c "
            set -e
            mkdir -p /var/lib/pulse/upgrade/$uid
            cp /tmp/degraded/pulse-* /tmp/degraded/SHA256SUMS /tmp/degraded/SHA256SUMS.sig /tmp/degraded/rollback.* /var/lib/pulse/upgrade/$uid/
            cp /tmp/degraded/request.json /var/lib/pulse/upgrade/request.json.tmp
            mv /var/lib/pulse/upgrade/request.json.tmp /var/lib/pulse/upgrade/request.json"; then
            fail "l/$th：無法以服務帳號寫入 request.json"
            continue
        fi
        i=0
        while [ $i -lt 240 ]; do
            if hx "$th" sh -c "grep -qs '\"step\": \"done\"' $HW/$uid/state.json" 2>/dev/null; then break; fi
            sleep 2
            i=$((i + 1))
        done
        collect_logs "l-$th"
        hx "$th" cat "$HW/$uid/state.json" >"$WORK/l-$th.state.json" 2>/dev/null || true
        check "l/$th：結果是降級回滾（state 仍是 rolled_back，代碼不同）" \
            "$(jq -r '"\(.result.state) \(.result.code)"' "$WORK/l-$th.state.json" 2>&1)" '^rolled_back PLS-W8418$'
        check "l/$th：detail 以 degraded: 與原因開頭" "$(jq -r '.result.detail' "$WORK/l-$th.state.json" 2>&1)" \
            '^degraded:rollback_not_verified,package_db_inconsistent; '
        check "l/$th：交易紀錄沒有回滾套件、記下不採用的原因、回滾方式是換回執行檔" \
            "$(jq -r '"\(.rollback) \(.rollback_declined | join(",")) \(.rollback_intent.via)"' "$WORK/l-$th.state.json" 2>&1)" \
            '^null rollback_not_verified binary$'
        check "l/$th：輔助程式沒有執行套件管理員回滾" \
            "$(hx "$th" sh -c "grep -c '經套件管理員裝回' $HW/$uid/upgrade.log || true")" '^0$'
        check "l/$th：執行檔回到 $cur" "$(hx "$th" /opt/pulse/pulse --version | awk '{print $2}')" "^$cur_re\$"
        check "l/$th：systemctl is-active pulse" "$(hx "$th" systemctl is-active pulse.service 2>&1)" '^active$'
        check "l/$th：套件資料庫停在失敗的版本（不一致）" "$(pkg_version "$th")" "^$t4_pkg-1\$"
        check "l/$th：輔助程式留下的提示是 degraded" \
            "$(hx "$th" cat /var/lib/pulse/upgrade/rollback.json 2>&1 | jq -r .state 2>&1)" '^degraded$'
        check "l/$th：pulse upgrade status 提示修復套件" \
            "$(hx "$th" /opt/pulse/pulse upgrade status 2>&1 | grep -c '需要修復套件（PLS-W8418）')" '^1$'
        check "l/$th：hello 宣告 degraded、Kairos 標示需要修復套件" \
            "$(wait_rollback_declared "$th" '^degraded true$')" '^degraded true$'
        check "l/$th：pulse status 以非 0 的結束代碼表示需要處理" \
            "$(hx "$th" sh -c '/opt/pulse/pulse status >/tmp/status.out 2>&1; echo $?; grep -c PLS-W8418 /tmp/status.out' | tr '\n' ' ')" '^[1-9][0-9]* [1-9]'
        # 修復：重新安裝目前版本的套件
        hx "$th" rm -rf /root/repair
        hx "$th" mkdir -p /root/repair
        docker cp "$DIST/$cur/$rb" "$P-$th:/root/repair/$rb"
        docker cp "$DIST/$cur/SHA256SUMS" "$P-$th:/root/repair/SHA256SUMS"
        docker cp "$DIST/$cur/SHA256SUMS.sig" "$P-$th:/root/repair/SHA256SUMS.sig"
        if [ "$th" = deb ]; then
            hx deb sh -c "DEBIAN_FRONTEND=noninteractive dpkg -i --force-confold /root/repair/$rb" >"$LOGS/l-$th-repair.log" 2>&1 || fail "l/$th：dpkg -i 失敗"
        else
            hx rpm rpm -U --replacepkgs --oldpackage "/root/repair/$rb" >"$LOGS/l-$th-repair.log" 2>&1 || fail "l/$th：rpm -U 失敗"
        fi
        hx "$th" systemctl restart pulse.service
        check "l/$th：修復後套件資料庫回到 $cur" "$(pkg_version "$th")" "^$cur_pkg-1\$"
        check "l/$th：修復後不再宣告 degraded" "$(wait_rollback_declared "$th" '^binary_only false$')" '^binary_only false$'
        check "l/$th：修復後 pulse upgrade status 不再提示" \
            "$(hx "$th" /opt/pulse/pulse upgrade status 2>&1 | grep -c '需要修復套件' || true)" '^0$'
        # pulse upgrade archive-current：竄改過的套件不被封存；官方的套件通過驗證後封存
        hx "$th" sh -c "mkdir -p /root/repair/bad && cp /root/repair/$rb /root/repair/bad/$rb && printf x >> /root/repair/bad/$rb"
        check "l/$th：archive-current 拒絕被改過的套件" \
            "$(hx "$th" sh -c "/opt/pulse/pulse upgrade archive-current --package /root/repair/bad/$rb --sums /root/repair/SHA256SUMS --signature /root/repair/SHA256SUMS.sig 2>&1; echo \" exit=\$?\"" | tr '\n' ' ')" \
            '沒有通過驗證.* exit=1'
        check "l/$th：被拒絕後沒有封存" "[$(archive_entry "$th")]" '^\[\]$'
        check "l/$th：archive-current 封存目前的版本" \
            "$(hx "$th" sh -c "/opt/pulse/pulse upgrade archive-current --package /root/repair/$rb --sums /root/repair/SHA256SUMS --signature /root/repair/SHA256SUMS.sig 2>&1; echo \" exit=\$?\"" | tr '\n' ' ')" \
            '已封存 .*generation 1.* exit=0'
        check "l/$th：封存索引" "$(archive_entry "$th")" "^$cur_re install 1\$"
        check "l/$th：以服務帳號執行 archive-current 被拒（需要 root）" \
            "$(hx "$th" setpriv --reuid pulse --regid pulse --init-groups sh -c "/opt/pulse/pulse upgrade archive-current --package /root/repair/$rb --sums /root/repair/SHA256SUMS --signature /root/repair/SHA256SUMS.sig >/dev/null 2>&1; echo \$?")" '^10$'
        hx "$th" systemctl restart pulse.service
        check "l/$th：封存後 hello 宣告 archived" "$(wait_rollback_declared "$th" '^archived false$')" '^archived false$'
        check "l/$th：重新連線後仍宣告 upgrade" "$(wait_install "$th" "^$th ready true ")" "^$th ready true "
        hx "$th" rm -rf /tmp/degraded /root/repair
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

# 情境 e（REMOTE.md 第 5.1 節；0.10.0 的安全性修正）：取得服務帳號的人自己寫 request.json——目標是有官方
# 簽章的較新版本（崩潰的建置，健康確認一定失敗），謊報 from_version 為較舊的版本並附上那個舊版有官方簽章
# 的套件當回滾檔，企圖讓 root 的輔助程式在「回滾」時以 dpkg -i／rpm -U --oldpackage 把主機降到舊版（繞過
# 本機的 allow_downgrade = false）。輔助程式以自己的版本為準：from_version 不符就拒絕，什麼都不改
scenario_tamper() {
    need "$V0" $T1 $T4
    say "情境 e：以服務帳號竄改 request.json（謊報 from_version＋較舊的已簽章回滾檔）→ 輔助程式拒絕"
    # 迴圈變數不用 h：collect_logs 會改寫它（sh 沒有區域變數）
    for th in deb rpm; do
        cur=$(hx "$th" /opt/pulse/pulse --version | awk '{print $2}')
        # 謊報的版本：比目前舊的 $V0；主機還在 $V0 時（單獨執行這個情境）改用 $T1
        old=$V0
        [ "$cur" != "$V0" ] || old=$T1
        pv=$(pkg_version "$th")
        uid=$(cat /proc/sys/kernel/random/uuid)
        art=$(basename "$(ls "$DIST/$T4"/pulse-*-linux-x86_64."$th")")
        man=$(basename "$(ls "$DIST/$T4"/pulse-*-release.json)")
        rb=$(basename "$(ls "$DIST/$old"/pulse-*-linux-x86_64."$th")")
        stage=$WORK/tamper-$th
        rm -rf "$stage"
        mkdir -p "$stage"
        cp "$DIST/$T4/$art" "$DIST/$T4/$man" "$DIST/$T4/SHA256SUMS" "$DIST/$T4/SHA256SUMS.sig" "$DIST/$old/$rb" "$stage/"
        cp "$DIST/$old/SHA256SUMS" "$stage/rollback.SHA256SUMS"
        cp "$DIST/$old/SHA256SUMS.sig" "$stage/rollback.SHA256SUMS.sig"
        jq -n -c --arg id "$uid" --arg v "$T4" --arg c "$(jq -r .commit "$DIST/$T4/$man")" --arg m "$th" \
            --arg a "$art" --arg man "$man" --arg rb "$rb" --arg from "$old" \
            --arg at "$(date -u +%Y-%m-%dT%H:%M:%SZ)" --argjson t "$HEALTH_TIMEOUT" \
            '{schema: "pulse.upgrade-request/v1", upgrade_id: $id, target: {version: $v, commit: $c},
              method: $m, artifact: $a, manifest: $man, rollback_artifact: $rb, health_timeout_secs: $t,
              allow_downgrade: false, from_version: $from, requested_at: $at}' >"$stage/request.json"
        chmod -R a+rX "$stage"
        hx "$th" rm -rf /tmp/tamper
        docker cp "$stage" "$P-$th:/tmp/tamper"
        # 以服務帳號放進暫存區，最後寫入 request.json（pulse-upgrade.path 觸發 root 的輔助程式）
        if ! hx "$th" setpriv --reuid pulse --regid pulse --init-groups sh -c "
            set -e
            mkdir -p /var/lib/pulse/upgrade/$uid
            cp /tmp/tamper/pulse-* /tmp/tamper/SHA256SUMS /tmp/tamper/SHA256SUMS.sig /tmp/tamper/rollback.* /var/lib/pulse/upgrade/$uid/
            cp /tmp/tamper/request.json /var/lib/pulse/upgrade/request.json.tmp
            mv /var/lib/pulse/upgrade/request.json.tmp /var/lib/pulse/upgrade/request.json"; then
            fail "e/$th：無法以服務帳號寫入 request.json"
            continue
        fi
        i=0
        line=""
        while [ $i -lt 90 ]; do
            line=$(hx "$th" sh -c "grep -h from_version_mismatch /var/lib/pulse-upgrade/$uid/upgrade.log 2>/dev/null | tail -n 1" || true)
            [ -z "$line" ] || break
            # 修正前的行為：輔助程式照做（替換 → 崩潰 → 以回滾檔降版）；出現結果就不必再等
            if hx "$th" sh -c "grep -qs '\"step\": \"done\"' /var/lib/pulse-upgrade/$uid/state.json" 2>/dev/null; then break; fi
            sleep 2
            i=$((i + 1))
        done
        collect_logs "e-$th"
        check "e/$th：輔助程式拒絕（PLS-E8402 from_version_mismatch）" \
            "$(printf '%s' "$line" | grep -o 'PLS-E8402.*' || true)" \
            'PLS-E8402.*from_version_mismatch: request says .*, installed version is '
        case "$line" in
        *"request says $old, installed version is $cur"*) pass "e/$th：拒絕的原因列出謊報的 $old 與實際的 $cur" ;;
        *) fail "e/$th：拒絕的原因沒有列出謊報的 $old 與實際的 $cur：$line" ;;
        esac
        after=$(hx "$th" /opt/pulse/pulse --version | awk '{print $2}')
        if [ "$after" = "$cur" ]; then pass "e/$th：/opt/pulse/pulse --version 沒有改變：$after"; else fail "e/$th：版本由 $cur 變成 $after"; fi
        pa=$(pkg_version "$th")
        if [ "$pa" = "$pv" ]; then pass "e/$th：套件資料庫的版本沒有改變：$pa"; else fail "e/$th：套件資料庫由 $pv 變成 $pa"; fi
        check "e/$th：沒有替換也沒有執行套件管理員" \
            "$(hx "$th" sh -c "grep -c -E '已替換|經套件管理員|pulse.prev' /var/lib/pulse-upgrade/$uid/upgrade.log || true")" '^0$'
        check "e/$th：沒有留下回滾檔" "$(hx "$th" sh -c "ls /var/lib/pulse-upgrade/$uid/rollback.pkg 2>/dev/null | wc -l")" '^0$'
        check "e/$th：request.json 已刪除" "$(hx "$th" sh -c 'ls /var/lib/pulse/upgrade/request.json 2>/dev/null | wc -l')" '^0$'
        check "e/$th：systemctl is-active pulse" "$(hx "$th" systemctl is-active pulse.service 2>&1)" '^active$'
        hx "$th" rm -rf /tmp/tamper
    done
}

# tamper_reboot <主機> <假的 upgrade_id>：輔助程式到回滾確認（rolling_back）時，先記下 root 的指標、交易紀錄
# 與它們的權限（$WORK/f-<主機>.*，由 scenario_tamper_resume 檢查），再以服務帳號把 request.json 的 upgrade_id
# 改成另一個 UUID、from_version 改成謊報的 0.0.1，然後重新開機
tamper_reboot() {
    deadline=$(($(date +%s) + 600))
    while [ "$(date +%s)" -lt "$deadline" ]; do
        if hx "$1" sh -c "grep -qs '\"step\": \"rolling_back\"' $HW/*/state.json" 2>/dev/null; then
            hx "$1" cat $HW/active.json >"$WORK/f-$1.active.json" 2>/dev/null || true
            tid=$(jq -r '.upgrade_id // empty' "$WORK/f-$1.active.json" 2>/dev/null || true)
            hx "$1" cat "$HW/$tid/state.json" >"$WORK/f-$1.state.json" 2>/dev/null || true
            hx "$1" stat -c '%a %U:%G' $HW "$HW/$tid" "$HW/$tid/state.json" $HW/active.json \
                >"$WORK/f-$1.stat" 2>&1 || true
            # 服務帳號讀不到 root 的工作目錄
            hx "$1" setpriv --reuid pulse --regid pulse --init-groups sh -c "ls $HW >/dev/null 2>&1 && echo readable || echo denied" \
                >"$WORK/f-$1.access" 2>&1 || true
            if hx "$1" setpriv --reuid pulse --regid pulse --init-groups sh -c "
                set -e
                cd /var/lib/pulse/upgrade
                sed -e 's/\"upgrade_id\": *\"[0-9a-f-]*\"/\"upgrade_id\":\"$2\"/' \
                    -e 's/\"from_version\": *\"[^\"]*\"/\"from_version\":\"0.0.1\"/' request.json > request.json.tmp
                grep -q '$2' request.json.tmp
                grep -q '\"from_version\":\"0.0.1\"' request.json.tmp
                mv request.json.tmp request.json"; then
                echo "$1" >>"$WORK/tampered"
            fi
            info "$1：輔助程式在 rolling_back，已改寫 request.json 的 upgrade_id 與 from_version，重新開機（docker restart）"
            docker restart -t 60 "$P-$1" >/dev/null
            wait_boot "$1"
            echo "$1" >>"$WORK/rebooted"
            return 0
        fi
        sleep 1
    done
    info "$1：等不到輔助程式的 rolling_back"
    return 1
}

# record_checksum_ok <state.json 或 active.json 的複本>：checksum 欄位等於「整份檔案、checksum 以 64 個 0 代入」
# 的 SHA-256（REMOTE.md 第 17.9.2 節）
record_checksum_ok() {
    want=$(jq -r '.checksum // empty' "$1" 2>/dev/null || true)
    zeros=0000000000000000000000000000000000000000000000000000000000000000
    got=$(sed "s/\"checksum\": \"[0-9a-f]*\"/\"checksum\": \"$zeros\"/" "$1" | sha256sum | awk '{print $1}')
    if [ -n "$want" ] && [ "$want" = "$got" ]; then echo ok; else echo "mismatch（檔案 $want、計算 $got）"; fi
}

# 情境 f（REMOTE.md 第 17.8、17.9 節；進行中交易的指標與交易紀錄）：升級進行中（新版崩潰、回滾確認期間）
# 服務帳號把 request.json 的 upgrade_id 換成另一個 UUID、from_version 改成謊報的版本，然後主機重新開機。
# 輔助程式仍接續原本的交易（以 root 的 active.json 與 state.json 為準）：回報原交易的 rolled_back，沒有為
# 假的 upgrade_id 開新的交易，pulse.prev 沒有被覆寫。另外檢查竄改當下 root 的指標與交易紀錄（schema v2、
# checksum、綁定的雜湊、權限）與交易結束後的清理（只留紀錄與日誌、只保留最近一筆）
scenario_tamper_resume() {
    need $T4
    cur=$(hx deb /opt/pulse/pulse --version | awk '{print $2}')
    cur_re=$(printf '%s' "$cur" | sed 's/\./\\./g')
    fake=$(cat /proc/sys/kernel/random/uuid)
    say "情境 f：升級中改寫 request.json 的 upgrade_id 與 from_version 後重新開機 $cur → $T4（崩潰 → 回滾確認中竄改並重新開機 → 接續原交易）"
    rm -f "$WORK/rebooted" "$WORK/tampered" "$WORK"/f-*.json "$WORK"/f-*.stat "$WORK"/f-*.access
    # 情境 d 留下暫停的推送（它的主機仍算在未結束的推送中，新的推送會 409 rollout_overlap）：先取消
    api GET "/api/v1/rollouts?state=paused" >/dev/null
    for rid in $(body | jq -r '.data[].id'); do
        api POST "/api/v1/rollouts/$rid/cancel" '{"reason":"systest：tamper-resume 之前取消暫停的推送"}' >/dev/null
    done
    id=$(rollout "systest tamper resume" $T4)
    pids=""
    for th in $HOSTS; do
        tamper_reboot "$th" "$fake" &
        pids="$pids $!"
    done
    for p in $pids; do wait "$p" || true; done
    for th in $HOSTS; do
        if grep -qx "$th" "$WORK/tampered" 2>/dev/null && grep -qx "$th" "$WORK/rebooted" 2>/dev/null; then
            pass "f/$th：已在回滾確認期間改寫 upgrade_id 並重新開機"
        else
            fail "f/$th：沒有在回滾確認期間改寫 upgrade_id 並重新開機"
        fi
    done
    wait_rollout "$id" 1200 || fail "f：推送沒有在 1200 秒內結束"
    collect_logs f
    for th in $HOSTS; do
        verify f "$id" "$th" rolled_back PLS-W8407 "$cur"
        check "f/$th：接續原本的交易並加註" "$(host_state "$id" "$th")" 'request_upgrade_id_ignored'
        check "f/$th：謊報的 from_version 被忽略並加註" "$(host_state "$id" "$th")" 'request_from_version_ignored'
        check "f/$th：輔助程式接續回滾" "$(helper_log "$th" | grep -c '接續上次中斷的回滾')" '^[1-9]'
        check "f/$th：沒有為假的 upgrade_id 開新的交易" "$(hx "$th" sh -c "ls -d $HW/$fake 2>/dev/null | wc -l")" '^0$'
        check "f/$th：進行中交易的指標已清除" "$(hx "$th" sh -c "ls $HW/active.json 2>/dev/null | wc -l")" '^0$'
        check "f/$th：pulse.prev 仍是替換前的版本" "$(hx "$th" /opt/pulse/pulse.prev --version 2>&1 | awk '{print $2}')" "^$cur_re\$"
        # 竄改當下（回滾確認中）root 的指標與交易紀錄（REMOTE.md 第 17.9.1、17.9.2 節）
        fa=$WORK/f-$th.active.json
        fs=$WORK/f-$th.state.json
        check "f/$th：回滾確認中的指標（schema、checksum）" \
            "$(jq -r '"\(.schema) \(.checksum | length)"' "$fa" 2>&1) $(record_checksum_ok "$fa")" \
            '^pulse\.upgrade-active/v1 64 ok$'
        check "f/$th：指標指向的交易紀錄（schema、步驟、checksum）" \
            "$(jq -r '"\(.schema) \(.step) \(.checksum | length)"' "$fs" 2>&1) $(record_checksum_ok "$fs")" \
            '^pulse\.upgrade-helper/v2 rolling_back 64 ok$'
        check "f/$th：紀錄與指標的 upgrade_id、binding_sha256 相同" \
            "$(jq -r -s 'if .[0].upgrade_id == .[1].upgrade_id and .[0].binding_sha256 == .[1].binding_sha256 and (.[0].binding_sha256 | length) == 64 then "same" else "different" end' "$fa" "$fs" 2>&1)" \
            '^same$'
        check "f/$th：紀錄綁定替換前的版本、新舊執行檔與發佈檔的雜湊，並記下回滾的意圖" \
            "$(jq -r '"\(.from_version) \(.target.version) \(.method) \(.old_binary_sha256 | length) \(.new_binary_sha256 | length) \(.artifact.sha256 | length) \(.prev_saved) \(.rollback_intent.cause)"' "$fs" 2>&1)" \
            "^$cur_re $(printf '%s' "$T4" | sed 's/\./\\./g') $th 64 64 64 true health\$"
        check "f/$th：工作目錄 0700、紀錄與指標 0600，都屬於 root" "$(tr '\n' ' ' <"$WORK/f-$th.stat")" \
            '^700 root:root 700 root:root 600 root:root 600 root:root $'
        check "f/$th：服務帳號讀不到 $HW" "$(cat "$WORK/f-$th.access")" '^denied$'
        # 交易結束後的清理（第 17.9.8 節）
        tid=$(jq -r '.upgrade_id // empty' "$fa" 2>/dev/null || true)
        check "f/$th：交易結束後紀錄為 done" "$(hx "$th" sh -c "grep -c '\"step\": \"done\"' $HW/$tid/state.json" 2>&1)" '^1$'
        check "f/$th：已結束的交易只留紀錄與日誌" "$(hx "$th" sh -c "ls $HW/$tid | tr '\n' ' '" 2>&1)" '^state\.json upgrade\.log $'
        # 交易的工作目錄以 UUID 命名；本機封存（archive/）不算
        check "f/$th：$HW 只保留最近一筆交易" "$(hx "$th" sh -c "ls -d $HW/*-*-*-*-*/ | wc -l" 2>&1)" '^1$'
    done
}

# hold_and_kill <主機> <checkpoint>：等開啟 fault-injection 的輔助程式停在指定的當機點（它把
# $HW/fault-inject 改名為 fault-inject.hit 後停住），記下當時磁碟上的狀態（$WORK/crash-<主機>.*），再
# docker kill（SIGKILL PID 1：所有行程立即終止，等同斷電但不遺失已寫入頁面快取的資料）並重新啟動容器
hold_and_kill() {
    deadline=$(($(date +%s) + 900))
    while [ "$(date +%s)" -lt "$deadline" ]; do
        if hx "$1" test -e $HW/fault-inject.hit 2>/dev/null; then
            hx "$1" cat $HW/fault-inject.hit >"$WORK/crash-$1.hit" 2>/dev/null || true
            hx "$1" cat $HW/active.json >"$WORK/crash-$1.active.json" 2>/dev/null || true
            cid=$(jq -r '.upgrade_id // empty' "$WORK/crash-$1.active.json" 2>/dev/null || true)
            hx "$1" cat "$HW/$cid/state.json" >"$WORK/crash-$1.state.json" 2>/dev/null || true
            {
                hx "$1" /opt/pulse/pulse --version 2>&1 | awk '{print $2}'
                hx "$1" /opt/pulse/pulse.prev --version 2>&1 | awk '{print $2}'
            } >"$WORK/crash-$1.versions"
            pkg_version "$1" >"$WORK/crash-$1.pkg" 2>&1 || true
            info "$1：輔助程式停在 $2，終止整台主機（docker kill）後重新啟動"
            docker kill "$P-$1" >/dev/null
            if [ -n "${3:-}" ]; then
                # 關機到交易紀錄的健康確認期限過後 $3 秒才開機（情境 late-resume）
                dl=$(jq -r '.deadline_unix // 0' "$WORK/crash-$1.state.json" 2>/dev/null || echo 0)
                [ "$dl" -gt 0 ] || {
                    info "$1：交易紀錄沒有健康確認的期限"
                    docker start "$P-$1" >/dev/null
                    return 1
                }
                info "$1：關機到健康確認的期限（$(date -d "@$dl" +%H:%M:%S)）過後 $3 秒"
                while [ "$(date +%s)" -le $((dl + $3)) ]; do sleep 2; done
                date +%s >"$WORK/crash-$1.started"
            fi
            docker start "$P-$1" >/dev/null
            wait_boot "$1"
            echo "$1" >>"$WORK/crashed"
            return 0
        fi
        sleep 1
    done
    info "$1：等不到輔助程式停在 $2"
    return 1
}

# arm_crash <checkpoint>：在每台主機放好故障注入的掛鉤（只有 root 可寫；開啟 fault-injection 的建置才會讀）
arm_crash() {
    for ah in $HOSTS; do
        hx "$ah" sh -c "rm -f $HW/fault-inject.hit && mkdir -p -m 700 $HW && printf '%s\n' '$1' > $HW/fault-inject && chmod 600 $HW/fault-inject"
    done
}

# crash_hosts <情境> <checkpoint> [<期限過後幾秒才開機>]：每台主機都在那個當機點被終止並重新啟動
crash_hosts() {
    rm -f "$WORK/crashed" "$WORK"/crash-*
    pids=""
    for ch in $HOSTS; do
        hold_and_kill "$ch" "$2" "${3:-}" &
        pids="$pids $!"
    done
    for p in $pids; do wait "$p" || true; done
    for ch in $HOSTS; do
        if grep -qx "$ch" "$WORK/crashed" 2>/dev/null; then
            pass "$1/$ch：輔助程式停在 $2 時終止整台主機並重新啟動"
        else
            fail "$1/$ch：輔助程式沒有停在 $2"
        fi
        check "$1/$ch：掛鉤只觸發一次（已改名為 fault-inject.hit）" \
            "$(hx "$ch" sh -c "ls $HW/fault-inject $HW/fault-inject.hit 2>/dev/null | tr '\n' ' '")" "^$HW/fault-inject\\.hit \$"
    done
}

# 需要開啟 fault-injection 的輔助程式：主機目前的版本不是測試用建置時，先升級到 test.5
ensure_injectable() {
    need $T5
    inj=1
    for ih in $HOSTS; do
        hx "$ih" /opt/pulse/pulse --version 2>&1 | grep -q '\[fault-injection\]$' || inj=0
    done
    [ "$inj" = 0 ] || return 0
    say "升級到開啟 fault-injection 的測試用建置 $T5（之後的輔助程式才能在當機點停住）"
    api GET "/api/v1/rollouts?state=paused" >/dev/null
    for rid in $(body | jq -r '.data[].id'); do
        api POST "/api/v1/rollouts/$rid/cancel" '{"reason":"systest：取消暫停的推送"}' >/dev/null
    done
    id=$(rollout "systest injectable" $T5)
    wait_rollout "$id" 900 || fail "$1：升級到 $T5 的推送沒有在 900 秒內結束"
    collect_logs "$1-prepare"
    for ih in $HOSTS; do
        verify "$1（準備）" "$id" "$ih" succeeded PLS-I8416 $T5
        check "$1（準備）/$ih：測試用建置的 --version 有標記" "$(hx "$ih" /opt/pulse/pulse --version 2>&1)" '\[fault-injection\]$'
    done
}

# 情境 g（REMOTE.md 第 17.9.4 節第 11 列）：執行檔已替換（deb／rpm：套件已安裝完成）、還沒記下 replaced 時
# 整台主機被終止。開機後輔助程式依 root 的紀錄與磁碟上的實況接續：不重讀暫存區、不覆寫 pulse.prev，
# 健康確認後 succeeded
scenario_crash_replace() {
    ensure_injectable g
    need $T6
    cur=$(hx deb /opt/pulse/pulse --version | awk '{print $2}')
    cur_re=$(printf '%s' "$cur" | sed 's/\./\\./g')
    t6_re=$(printf '%s' "$T6" | sed 's/\./\\./g')
    say "情境 g：替換後、記錄前終止主機 $cur → $T6（當機點 binary_replaced → 開機後接續 → succeeded）"
    arm_crash binary_replaced
    id=$(rollout "systest crash replace" $T6)
    crash_hosts g binary_replaced
    for ch in $HOSTS; do
        check "g/$ch：當機時的紀錄停在 replacing（checksum 相符）" \
            "$(jq -r '"\(.step) \(.prev_saved)"' "$WORK/crash-$ch.state.json" 2>&1) $(record_checksum_ok "$WORK/crash-$ch.state.json")" \
            '^replacing true ok$'
        check "g/$ch：當機時執行檔已是新版、pulse.prev 是舊版" "$(tr '\n' ' ' <"$WORK/crash-$ch.versions")" "^$t6_re $cur_re \$"
        check "g/$ch：當機時指標已存在（checksum 相符）" "$(record_checksum_ok "$WORK/crash-$ch.active.json")" '^ok$'
    done
    wait_rollout "$id" 1200 || fail "g：推送沒有在 1200 秒內結束"
    collect_logs g
    for ch in $HOSTS; do
        verify g "$id" "$ch" succeeded PLS-I8416 $T6
        check "g/$ch：輔助程式接續替換（不重新驗證暫存區）" "$(helper_log "$ch" | grep -c '接續上次中斷的替換（replacing）')" '^1$'
        check "g/$ch：接續時沒有再保留 pulse.prev" "$(helper_log "$ch" | grep -c 'pulse.prev 已是替換前的執行檔，不再覆寫')" '^[1-9]'
        check "g/$ch：pulse.prev 仍是替換前的版本" "$(hx "$ch" /opt/pulse/pulse.prev --version 2>&1 | awk '{print $2}')" "^$cur_re\$"
        check "g/$ch：進行中交易的指標已清除" "$(hx "$ch" sh -c "ls $HW/active.json 2>/dev/null | wc -l")" '^0$'
        check "g/$ch：$HW 只保留最近一筆交易" "$(hx "$ch" sh -c "ls -d $HW/*-*-*-*-*/ | wc -l" 2>&1)" '^1$'
        hx "$ch" rm -f $HW/fault-inject.hit
    done
}

# 情境 h（REMOTE.md 第 17.9.4 節第 17 列）：新版崩潰、輔助程式已還原（執行檔、套件）而還沒記下 rolling_back 時
# 整台主機被終止。開機後依紀錄的回滾意圖冪等地重做還原，確認舊版恢復 → rolled_back（detail 有 resumed）
scenario_crash_rollback() {
    ensure_injectable h
    need $T7
    cur=$(hx deb /opt/pulse/pulse --version | awk '{print $2}')
    cur_re=$(printf '%s' "$cur" | sed 's/\./\\./g')
    say "情境 h：還原後、記錄前終止主機 $cur → $T7（崩潰 → 當機點 rollback_restored → 開機後接續 → rolled_back）"
    arm_crash rollback_restored
    id=$(rollout "systest crash rollback" $T7)
    crash_hosts h rollback_restored
    for ch in $HOSTS; do
        check "h/$ch：當機時的紀錄停在 rollback_intent（先寫意圖再還原；checksum 相符）" \
            "$(jq -r '"\(.step) \(.rollback_intent.cause) \(.rollback_intent.via)"' "$WORK/crash-$ch.state.json" 2>&1) $(record_checksum_ok "$WORK/crash-$ch.state.json")" \
            "^rollback_intent health $([ "$ch" = tar ] && echo binary || echo package) ok\$"
        check "h/$ch：當機時執行檔已換回舊版、pulse.prev 是舊版" "$(tr '\n' ' ' <"$WORK/crash-$ch.versions")" "^$cur_re $cur_re \$"
    done
    wait_rollout "$id" 1200 || fail "h：推送沒有在 1200 秒內結束"
    collect_logs h
    for ch in $HOSTS; do
        verify h "$id" "$ch" rolled_back PLS-W8407 "$cur"
        check "h/$ch：輔助程式接續回滾並重新還原" "$(helper_log "$ch" | grep -c '接續上次中斷的回滾：重新還原')" '^1$'
        check "h/$ch：接續後仍回報原本的回滾原因" "$(host_state "$id" "$ch")" '(crash_restart|exit_code|health_timeout).*; resumed'
        check "h/$ch：pulse.prev 仍是替換前的版本" "$(hx "$ch" /opt/pulse/pulse.prev --version 2>&1 | awk '{print $2}')" "^$cur_re\$"
        check "h/$ch：進行中交易的指標已清除" "$(hx "$ch" sh -c "ls $HW/active.json 2>/dev/null | wc -l")" '^0$'
        hx "$ch" rm -f $HW/fault-inject.hit
    done
    for ch in deb rpm; do
        # 目前的版本是上一次遠端升級裝上的（本機有它的已驗證發佈）：以回滾套件經套件管理員裝回
        if host_state "$id" "$ch" | grep -q binary_restored; then
            fail "h/$ch：應以回滾套件經套件管理員回滾，實際只換回執行檔"
        else
            pass "h/$ch：以回滾套件經套件管理員回滾（detail 沒有 binary_restored）"
        fi
    done
}

# install_state <主機>：「method upgrade negotiated version」（Kairos 看到的 hello）
install_state() {
    api GET "/api/v1/hosts/$(agent_of "$1")/upgrade" >/dev/null
    body | jq -r '[.data.install.method, .data.install.upgrade, (.data.upgrade_negotiated|tostring), .data.agent_version] | join(" ")' 2>/dev/null || true
}

# wait_install <主機> <預期（grep -E）>：等 Kairos 看到的 install 符合（最多 180 秒）；stdout 為最後的狀態
wait_install() {
    wi=0
    wstate=""
    while [ $wi -lt 90 ]; do
        wstate=$(install_state "$1")
        if printf '%s' "$wstate" | grep -Eq -- "$2"; then break; fi
        sleep 2
        wi=$((wi + 1))
    done
    echo "$wstate"
}

# 情境 i（REMOTE.md 第 2.1、17.9.7 節；0.10.0）：Pulse 在 hello 事先回報升級輔助程式的路徑檢查結果。不經推送：
# 把 /opt/pulse 改成群組可寫 → 服務帳號的 Pulse 重新連線時回報 install.upgrade = unsafe_path、不宣告 upgrade →
# Kairos 的資格預覽把這台主機分類為 unsafe_path（不是可升級）；改回後恢復 ready。同時確認真實的安裝（deb、rpm、
# tar；服務帳號、unit 的沙箱）在合規時不會被預檢誤報——run.sh 一開始就要求三台都是 ready
scenario_unsafe_path() {
    need $T7
    say "情境 i：路徑預檢——/opt/pulse 群組可寫 → hello 回報 unsafe_path → 資格預覽分類 → 改回後恢復 ready"
    for h in $HOSTS; do
        a=$(agent_of "$h")
        # 目標版本只用來計算資格（不送出推送）；主機不會停在 test.7（崩潰的建置）
        spec=$(jq -n -c --arg v "$T7" --arg a "$a" \
            '{name: "systest unsafe path", target_version: $v, selector: {agent_ids: [$a]}, waves: [{size: "rest"}]}')
        mode=$(hx "$h" stat -c %a /opt/pulse)
        check "i/$h：改之前的資格預覽" "$(api POST /api/v1/rollouts/preview "$spec") $(body | jq -c '[.data.eligible, .data.skipped]')" '^200 \[1,\{\}\]$'
        hx "$h" chmod g+w /opt/pulse
        hx "$h" systemctl restart pulse.service
        check "i/$h：hello 回報 unsafe_path、不宣告 upgrade" "$(wait_install "$h" "^$h unsafe_path false ")" "^$h unsafe_path false "
        check "i/$h：pulse upgrade status 寫出路徑與原因" \
            "$(hx "$h" /opt/pulse/pulse upgrade status 2>&1 | grep -E '遠端升級|原因' | tr '\n' ' ')" \
            'PLS-E8405.*unsafe_path: /opt/pulse: writable by group or others'
        check "i/$h：資格預覽分類為 unsafe_path" "$(api POST /api/v1/rollouts/preview "$spec") $(body | jq -c '[.data.eligible, .data.skipped]')" '^200 \[0,\{"unsafe_path":1\}\]$'
        hx "$h" chmod "$mode" /opt/pulse
        hx "$h" systemctl restart pulse.service
        check "i/$h：改回後恢復 ready、宣告 upgrade" "$(wait_install "$h" "^$h ready true ")" "^$h ready true "
        check "i/$h：改回後的資格預覽" "$(api POST /api/v1/rollouts/preview "$spec") $(body | jq -c '[.data.eligible, .data.skipped]')" '^200 \[1,\{\}\]$'
    done
    collect_logs i
}

# 情境 j（REMOTE.md 第 17.9.4 節第 13 列、第 17.9.3 節「接續 replaced 時的健康確認」；0.10.1）：輔助程式已記下
# replaced（健康確認的期限已定）、還沒重新啟動服務時整台主機被終止，而且**關機到期限過後才開機**。0.10.0 的
# 輔助程式開機後不做任何檢查、直接回滾；0.10.1 起仍做與正常流程相同的健康確認——新版健康 → succeeded
scenario_late_resume() {
    ensure_injectable j
    need $T8
    cur=$(hx deb /opt/pulse/pulse --version | awk '{print $2}')
    cur_re=$(printf '%s' "$cur" | sed 's/\./\\./g')
    t8_re=$(printf '%s' "$T8" | sed 's/\./\\./g')
    say "情境 j：記下 replaced 後終止主機、期限過後 $LATE_AFTER 秒才開機 $cur → $T8（當機點 record_replaced → 期限已過才接續 → succeeded）"
    arm_crash record_replaced
    id=$(rollout "systest late resume" $T8)
    crash_hosts j record_replaced "$LATE_AFTER"
    for ch in $HOSTS; do
        check "j/$ch：當機時的紀錄停在 replaced、還沒重新啟動、有 boot_id（checksum 相符）" \
            "$(jq -r '"\(.step) \(.service_restarted) \(.boot_id | type)"' "$WORK/crash-$ch.state.json" 2>&1) $(record_checksum_ok "$WORK/crash-$ch.state.json")" \
            '^replaced false string ok$'
        check "j/$ch：當機時執行檔已是新版、pulse.prev 是舊版" "$(tr '\n' ' ' <"$WORK/crash-$ch.versions")" "^$t8_re $cur_re \$"
        dl=$(jq -r '.deadline_unix // 0' "$WORK/crash-$ch.state.json" 2>/dev/null || echo 0)
        st=$(cat "$WORK/crash-$ch.started" 2>/dev/null || echo 0)
        check "j/$ch：開機時健康確認的期限已過（期限 $dl、開機 $st）" "$([ "$dl" -gt 0 ] && [ "$st" -gt "$dl" ] && echo late || echo "not late")" '^late$'
    done
    wait_rollout "$id" 1200 || fail "j：推送沒有在 1200 秒內結束"
    collect_logs j
    for ch in $HOSTS; do
        verify j "$id" "$ch" succeeded PLS-I8416 $T8
        check "j/$ch：輔助程式接續 replaced" "$(helper_log "$ch" | grep -c '接續上次中斷的升級：已替換，重新做健康確認')" '^1$'
        check "j/$ch：期限已過仍做健康確認（不直接回滾）" "$(helper_log "$ch" | grep -c '健康確認的期限已過（[0-9]* 秒前）：仍以 120 秒做與正常流程相同的健康確認')" '^1$'
        check "j/$ch：補送還沒送出的重新啟動" "$(helper_log "$ch" | grep -c '上次還沒有重新啟動 pulse.service：現在重新啟動')" '^1$'
        check "j/$ch：健康確認通過、沒有回滾" \
            "$(helper_log "$ch" | grep -c '健康確認通過，已升級到') $(helper_log "$ch" | grep -c '健康確認失敗')" '^1 0$'
        check "j/$ch：pulse.prev 仍是替換前的版本" "$(hx "$ch" /opt/pulse/pulse.prev --version 2>&1 | awk '{print $2}')" "^$cur_re\$"
        check "j/$ch：進行中交易的指標已清除" "$(hx "$ch" sh -c "ls $HW/active.json 2>/dev/null | wc -l")" '^0$'
        hx "$ch" rm -f $HW/fault-inject.hit
    done
}

# cancel_paused <原因>：取消暫停中的推送（它的主機仍算在未結束的推送中，新的推送會 409 rollout_overlap）
cancel_paused() {
    api GET "/api/v1/rollouts?state=paused" >/dev/null
    for rid in $(body | jq -r '.data[].id'); do
        api POST "/api/v1/rollouts/$rid/cancel" "$(jq -n -c --arg r "systest：$1" '{reason: $r}')" >/dev/null
    done
}

# anchor_line <金鑰> [<選項>]：allowed_signers 的一行（與 build.sh 的 signer 相同的 principal 與 namespace）
anchor_line() {
    printf 'release@kairos-pulse namespaces="kairos-pulse-release"%s %s\n' "${2:+,$2}" "$(cut -d' ' -f1,2 "$KEYS/$1.pub")"
}

# set_anchor <keep｜expired｜revoked>：三台主機的信任錨中，第一把發佈金鑰 release 的那一行——照常、valid-before
# 已過（自然到期）、整行刪除（撤銷；REMOTE.md 第 4.2.3 節）。第二把 release2（輪替後的金鑰）一律保留。之後
# 重新啟動 Pulse 並等它重新連線
set_anchor() {
    case "$1" in
    keep)
        anchor_line release
        anchor_line release2
        ;;
    expired)
        anchor_line release 'valid-before="20200101"'
        anchor_line release2
        ;;
    revoked) anchor_line release2 ;;
    *) die "set_anchor：未知的狀態 $1" ;;
    esac >"$WORK/anchor.$1"
    for ah in $HOSTS; do
        docker cp "$WORK/anchor.$1" "$P-$ah:/tmp/anchor.new"
        hx "$ah" install -o root -g root -m 644 /tmp/anchor.new /etc/kairos-pulse/allowed_signers
        hx "$ah" rm -f /tmp/anchor.new
        hx "$ah" systemctl restart pulse.service
    done
    sleep 5
    for ah in $HOSTS; do
        wait_install "$ah" "^$ah ready true " >/dev/null
    done
}

# pkg_versions <主機>：套件資料庫中 pulse 的每一筆版本（rpm 的交易只完成一部分時會有兩筆），以空白分隔
pkg_versions() {
    # shellcheck disable=SC2016 # ${Version} 是 dpkg-query 的格式，不是 shell 變數
    case "$1" in
    deb) hx deb dpkg-query -W -f='${Version} ${db:Status-Abbrev}\n' pulse 2>&1 | tr '\n' ' ' ;;
    rpm) hx rpm rpm -q --qf '%{VERSION}-%{RELEASE}\n' pulse 2>&1 | sort | tr '\n' ' ' ;;
    esac
}

# repair_package <情境> <主機> <版本>：降級回滾之後的修復（REMOTE.md 第 4.5.2 節）——重新安裝目前版本的套件，
# 再以 pulse upgrade archive-current 封存；之後 hello 應該宣告 archived、不再需要修復
repair_package() {
    rp=$(basename "$(ls "$DIST/$3"/pulse-*-linux-x86_64."$2")")
    rp_re=$(printf '%s' "$3" | sed 's/-/[~-]/; s/\./\\./g')
    hx "$2" rm -rf /root/repair
    hx "$2" mkdir -p /root/repair
    docker cp "$DIST/$3/$rp" "$P-$2:/root/repair/$rp"
    docker cp "$DIST/$3/SHA256SUMS" "$P-$2:/root/repair/SHA256SUMS"
    docker cp "$DIST/$3/SHA256SUMS.sig" "$P-$2:/root/repair/SHA256SUMS.sig"
    if [ "$2" = deb ]; then
        hx deb sh -c "DEBIAN_FRONTEND=noninteractive dpkg -i --force-confold /root/repair/$rp" >"$LOGS/$1-$2-repair.log" 2>&1 ||
            fail "$1/$2：修復時 dpkg -i 失敗（$LOGS/$1-$2-repair.log）"
    else
        hx rpm rpm -U --replacepkgs --oldpackage "/root/repair/$rp" >"$LOGS/$1-$2-repair.log" 2>&1 ||
            fail "$1/$2：修復時 rpm -U 失敗（$LOGS/$1-$2-repair.log）"
    fi
    check "$1/$2：修復後套件資料庫只有 $3" "$(pkg_versions "$2")" "^$rp_re-1( ii ?)? \$"
    check "$1/$2：修復後以 archive-current 封存目前的版本" \
        "$(hx "$2" sh -c "/opt/pulse/pulse upgrade archive-current --package /root/repair/$rp --sums /root/repair/SHA256SUMS --signature /root/repair/SHA256SUMS.sig 2>&1; echo \" exit=\$?\"" | tr '\n' ' ')" \
        ' exit=0'
    hx "$2" systemctl restart pulse.service
    check "$1/$2：修復後 hello 宣告 archived、不再需要修復" "$(wait_rollback_declared "$2" '^archived false$')" '^archived false$'
    check "$1/$2：修復後仍宣告 upgrade" "$(wait_install "$2" "^$2 ready true ")" "^$2 ready true "
    hx "$2" rm -rf /root/repair
}

# rollout_pause <推送>：「state pause_cause」
rollout_pause() {
    api GET "/api/v1/rollouts/$1" >/dev/null
    body | jq -r '"\(.data.state) \(.data.pause_cause // "-")"'
}

# host_degraded <推送> <主機>：Kairos 的推送主機列記下的降級原因（以逗號分隔；沒有時是 -）
host_degraded() {
    api GET "/api/v1/rollouts/$1/hosts" >/dev/null
    body | jq -r --arg a "$(agent_of "$2")" '.data[] | select(.agent_id == $a) | ((.degraded // []) | join(",")) as $d | if $d == "" then "-" else $d end'
}

# degraded_checks <情境> <推送> <主機> <舊版> <原因（grep -E，detail 中 degraded: 之後的開頭）>：降級回滾的共同
# 檢查——結果與代碼、執行檔回到舊版而服務正常、沒有留下進行中的交易、本機與 Kairos 的顯示
degraded_checks() {
    dc_re=$(printf '%s' "$4" | sed 's/\./\\./g')
    check "$1/$3：Kairos 的主機狀態是降級回滾" "$(host_state "$2" "$3")" "^rolled_back PLS-W8418 degraded:$5"
    check "$1/$3：Kairos 的推送主機列記下降級的原因" "$(host_degraded "$2" "$3")" "^$5"
    check "$1/$3：執行檔回到 $4" "$(hx "$3" /opt/pulse/pulse --version 2>&1 | awk '{print $2}')" "^$dc_re\$"
    check "$1/$3：systemctl is-active pulse" "$(hx "$3" systemctl is-active pulse.service 2>&1)" '^active$'
    check "$1/$3：交易紀錄的回滾方式是換回執行檔" \
        "$(hx "$3" sh -c "cat $HW/*-*-*-*-*/state.json" 2>/dev/null | jq -r '"\(.step) \(.rollback_intent.via) \(.result.code)"' 2>&1)" \
        '^done binary PLS-W8418$'
    check "$1/$3：以 pulse.prev 換回" "$(helper_log "$3" | grep -c '以替換前保留的 /opt/pulse/pulse.prev 換回')" '^[1-9]'
    check "$1/$3：進行中交易的指標已清除、request.json 已刪除" \
        "$(hx "$3" sh -c "ls $HW/active.json /var/lib/pulse/upgrade/request.json 2>/dev/null | wc -l")" '^0$'
    check "$1/$3：輔助程式留下的提示是 degraded" \
        "$(hx "$3" cat /var/lib/pulse/upgrade/rollback.json 2>&1 | jq -r .state 2>&1)" '^degraded$'
    check "$1/$3：pulse upgrade status 提示修復套件" \
        "$(hx "$3" /opt/pulse/pulse upgrade status 2>&1 | grep -c '需要修復套件（PLS-W8418）')" '^1$'
    check "$1/$3：pulse status 以非 0 的結束代碼表示需要處理" \
        "$(hx "$3" sh -c '/opt/pulse/pulse status >/tmp/status.out 2>&1; echo $?; grep -c PLS-W8418 /tmp/status.out' | tr '\n' ' ')" '^[1-9][0-9]* [1-9]'
    check "$1/$3：hello 宣告 degraded、Kairos 標示需要修復套件" \
        "$(wait_rollback_declared "$3" '^degraded true$')" '^degraded true$'
}

# 情境 m（ADR-0003 的 INV-7、決策點 D1；REMOTE.md 第 4.2.3、17.10.4 節）：發佈金鑰輪替。新的版本 test.10 以
# 第二把金鑰 release2 簽章；主機目前的版本（與它的封存、本機紀錄、版本庫裡的簽章）是第一把 release 簽的。
#   m1（對照）：release 那一行還在、valid-before 已過（自然到期）→ deb／rpm 仍以本機封存經套件管理員回滾
#   m2：release 那一行被刪除（撤銷）→ 封存（deb）與提議的 rollback_bundle（rpm：先移除封存與本機紀錄）都
#       不得再使用 → 以 pulse.prev 回復＝降級回滾，推送自動暫停，主機標示需要修復套件
# tar 沒有套件層級的回滾，兩次都是一般的回滾。結束時把信任錨改回、修復套件並重新封存
scenario_key_revoked() {
    need $T10
    cur=$(hx deb /opt/pulse/pulse --version | awk '{print $2}')
    cur_re=$(printf '%s' "$cur" | sed 's/\./\\./g')
    say "情境 m：發佈金鑰輪替後 $cur → $T10（崩潰）——舊金鑰自然到期仍以封存回滾；舊金鑰撤銷 → 降級回滾"
    cancel_paused "key-revoked 之前取消暫停的推送"
    for kh in deb rpm; do
        check "m/$kh：開始前有 $cur 的封存" "$(archive_entry "$kh")" "^$cur_re [a-z]+ [0-9]+\$"
    done
    # m1：自然到期
    set_anchor expired
    id=$(rollout "systest key expired" $T10)
    wait_rollout "$id" 1200 || fail "m1：推送沒有在 1200 秒內結束"
    collect_logs m1
    for kh in $HOSTS; do
        verify m1 "$id" "$kh" rolled_back PLS-W8407 "$cur"
        not_degraded m1 "$id" "$kh"
    done
    for kh in deb rpm; do
        check "m1/$kh：金鑰到期仍以本機封存回滾" "$(helper_log "$kh" | grep -c '回滾來源：本機封存的 ')" '^1$'
        check "m1/$kh：經套件管理員回滾" "$(helper_log "$kh" | grep -o '經套件管理員裝回.*' | head -n 1)" \
            "經套件管理員裝回 $cur（(dpkg -i|rpm -U --oldpackage --replacepkgs)）"
        check "m1/$kh：封存沒有被標記為撤銷" \
            "$(hx "$kh" cat $HW/archive/index.json 2>/dev/null | jq -r '.entries[0].revoked_at // "-"' 2>&1)" '^-$'
    done
    check "m1：推送沒有因為降級回滾而暫停" "$(rollout_pause "$id")" ' -$'
    # m2：撤銷。rpm 先移除封存與本機紀錄：回滾檔只剩提議的 rollback_bundle 這個來源
    cancel_paused "key-revoked：m1 結束"
    hx rpm sh -c "rm -rf $HW/archive /var/lib/pulse/upgrade/releases /var/lib/pulse/upgrade/rollback.json"
    set_anchor revoked
    id=$(rollout "systest key revoked" $T10)
    wait_rollout "$id" 1200 || fail "m2：推送沒有在 1200 秒內結束"
    collect_logs m2
    verify m2 "$id" tar rolled_back PLS-W8407 "$cur"
    not_degraded m2 "$id" tar
    degraded_checks m2 "$id" deb "$cur" 'rollback_signer_revoked,package_db_inconsistent'
    degraded_checks m2 "$id" rpm "$cur" '(no_rollback_package|rollback_not_verified),package_db_inconsistent'
    check "m2/deb：輔助程式不使用簽署金鑰已撤銷的封存" \
        "$(helper_log deb | grep -c '本機封存不使用（rollback_signer_revoked）')" '^1$'
    check "m2/deb：封存索引記下撤銷的時間" \
        "$(hx deb cat $HW/archive/index.json 2>/dev/null | jq -r '.entries[0].revoked_at | type' 2>&1)" '^(string|number)$'
    check "m2/rpm：Pulse 不採用以撤銷的金鑰簽的 rollback_bundle" \
        "$(hx rpm sh -c 'cat /opt/pulse/logs/pulse-*.log | grep -c "本機紀錄與提議的 rollback_bundle 都沒有通過驗證" || true')" '^[1-9]'
    t10_pkg=$(printf '%s' "$T10" | sed 's/-/[~-]/; s/\./\\./g')
    for kh in deb rpm; do
        check "m2/$kh：沒有執行套件管理員回滾" "$(helper_log "$kh" | grep -c '經套件管理員裝回')" '^0$'
        check "m2/$kh：套件資料庫停在失敗的版本（不一致）" "$(pkg_version "$kh")" "^$t10_pkg-1\$"
    done
    check "m2：推送因為降級回滾而自動暫停" "$(rollout_pause "$id")" '^paused rollback_degraded$'
    # 復原：信任錨改回、修復套件並重新封存（之後的情境照常）
    cancel_paused "key-revoked 結束"
    set_anchor keep
    for kh in deb rpm; do
        repair_package m2 "$kh" "$cur"
    done
}

# arm_unpack_failure <主機>：輔助程式換上新版（紀錄到 replaced）之後，把套件中的一個檔案設為不可變更
# （chattr +i）：之後套件管理員裝回舊版時在解包途中失敗（模擬解包中斷：磁碟或檔案系統在回滾當下出問題）。
# 升級本身不受影響（那時還沒有設）
arm_unpack_failure() {
    deadline=$(($(date +%s) + 600))
    while [ "$(date +%s)" -lt "$deadline" ]; do
        if hx "$1" sh -c "grep -qs '\"step\": \"replaced\"' $HW/*/state.json" 2>/dev/null; then
            if hx "$1" chattr +i /opt/pulse/README.md; then
                info "$1：輔助程式在 replaced，已把 /opt/pulse/README.md 設為不可變更"
                echo "$1" >>"$WORK/armed"
                return 0
            fi
            return 1
        fi
        sleep 1
    done
    info "$1：等不到輔助程式的 replaced"
    return 1
}

# 情境 n（ADR-0003 第 5.4 節最後一列；REMOTE.md 第 4.5.2 節「套件回滾失敗先以 pulse.prev 緊急復原」）：
# 健康確認失敗後，套件管理員裝回舊版的命令本身失敗。
#   deb：解包中斷——換上新版之後把套件中的一個檔案設為不可變更，dpkg -i 舊版在解包時失敗並還原自己做到一半的部分
#   rpm：失敗的版本帶著壞掉的維護指令碼——test.10 的 %preun 在有記號檔時以 1 結束，rpm -U --oldpackage 已經
#        裝上舊版的檔案、卻無法移除失敗的版本（交易只完成一部分，套件資料庫有兩筆）
# 兩者都先以 pulse.prev 緊急復原（服務回到舊版）、回報降級回滾（package_rollback_failed）、推送自動暫停、主機
# 標示需要修復套件；輔助程式不自動修復套件資料庫。tar 是一般的回滾
scenario_pkg_rollback_fails() {
    need $T10
    cur=$(hx deb /opt/pulse/pulse --version | awk '{print $2}')
    cur_re=$(printf '%s' "$cur" | sed 's/\./\\./g')
    cur_pkg=$(printf '%s' "$cur" | sed 's/-/[~-]/; s/\./\\./g')
    t10_pkg=$(printf '%s' "$T10" | sed 's/-/[~-]/; s/\./\\./g')
    say "情境 n：套件管理員的回滾失敗 $cur → $T10（崩潰）——deb：解包中斷；rpm：失敗版本的 %preun 失敗 → 以 pulse.prev 緊急復原（降級回滾）"
    cancel_paused "pkg-rollback-fails 之前取消暫停的推送"
    for nh in deb rpm; do
        check "n/$nh：開始前有 $cur 的封存" "$(archive_entry "$nh")" "^$cur_re [a-z]+ [0-9]+\$"
    done
    rm -f "$WORK/armed"
    hx rpm touch /etc/kp-systest-prerm-fail
    id=$(rollout "systest pkg rollback fails" $T10)
    arm_unpack_failure deb || true
    if grep -qx deb "$WORK/armed" 2>/dev/null; then
        pass "n/deb：已在換上新版之後設下解包的障礙"
    else
        fail "n/deb：沒有在換上新版之後設下解包的障礙"
    fi
    wait_rollout "$id" 1200 || fail "n：推送沒有在 1200 秒內結束"
    collect_logs n
    verify n "$id" tar rolled_back PLS-W8407 "$cur"
    not_degraded n "$id" tar
    for nh in deb rpm; do
        degraded_checks n "$id" "$nh" "$cur" 'package_rollback_failed,package_db_inconsistent'
        check "n/$nh：先嘗試經套件管理員裝回" "$(helper_log "$nh" | grep -c "經套件管理員裝回 $cur")" '^1$'
        check "n/$nh：套件回滾失敗後以 pulse.prev 緊急復原" \
            "$(helper_log "$nh" | grep -c '套件回滾失敗（.*）：改以 pulse.prev 緊急復原（降級回滾）')" '^1$'
    done
    # deb：dpkg 還原了自己做到一半的部分，套件資料庫仍是失敗的版本
    check "n/deb：套件資料庫停在失敗的版本" "$(pkg_versions deb)" "^$t10_pkg-1 "
    # rpm：舊版已裝上、失敗的版本移除不了：兩筆都在
    check "n/rpm：套件資料庫有舊版與失敗的版本兩筆（交易只完成一部分）" "$(pkg_versions rpm)" \
        "^$cur_pkg-1 $t10_pkg-1 \$|^$t10_pkg-1 $cur_pkg-1 \$"
    check "n/rpm：detail 照實附上套件資料庫的狀態" "$(host_state "$id" rpm)" 'package_db_shows_'
    check "n：推送因為降級回滾而自動暫停" "$(rollout_pause "$id")" '^paused rollback_degraded$'
    # 修復：移除障礙後重新安裝目前版本的套件
    cancel_paused "pkg-rollback-fails 結束"
    hx deb chattr -i /opt/pulse/README.md
    hx rpm rm -f /etc/kp-systest-prerm-fail
    for nh in deb rpm; do
        repair_package n "$nh" "$cur"
    done
}

# ax <命令…>：在 Ansible 控制端容器的 /work 執行
ax() { docker exec -w /work "$P-ansible" "$@"; }

# ansible_run <標籤> <playbook> <allowed_signers>：對兩台受管主機執行 playbook，輸出在 $LOGS/ansible-<標籤>.log，
# stdout 為結束代碼。安裝字串在控制端只有擁有者可讀的變數檔裡（對應文件建議的 Ansible Vault），不放在命令列
ansible_run() {
    arc=0
    ax env ANSIBLE_ROLES_PATH=/work/ansible/roles ANSIBLE_NOCOLOR=1 ANSIBLE_FORCE_COLOR=0 \
        ansible-playbook -i /work/inventory.ini "/work/ansible/$2" \
        -e pulse_dist_dir=/work/dist -e "pulse_allowed_signers=/work/$3" -e @/work/vars.yml \
        >"$LOGS/ansible-$1.log" 2>&1 || arc=$?
    echo "$arc"
}

# 情境 o（安全稽核 N12；Pulse 的 INSTALL.md 第 A.4 節）：packaging/ansible 的 role，以**這一輪建置的真正套件**
# 與測試環境的 Kairos，由控制端容器經 SSH（一般帳號 ops＋become）對兩台全新的受管主機（Debian 12：deb、
# Rocky Linux 9：rpm；systemd 為 PID 1、有 sshd）執行：
#   o1 發佈公鑰不對 → 在控制端驗證簽章的步驟停止，受管主機沒有任何改變
#   o2 不用 become → 在檢查暫存目錄的步驟停止並移除目錄，沒有安裝
#   o3 整個 role：驗證簽章 → 暫存目錄（root、0700，連線帳號進不去，用完移除）→ 核對雜湊 → apt／dnf 安裝 →
#      pulse enroll → 啟動服務並連上 Kairos
#   o4 再執行一次：不重新註冊，服務照常
# SSH 金鑰臨時產生，結束時連同控制端與受管主機的容器一起刪除。不影響其他情境的三台主機
scenario_ansible() {
    need "$V0"
    say "情境 o：Ansible role 經 SSH（一般帳號＋become）安裝真正的 Pulse $V0 套件、pulse enroll、連上 Kairos"
    v0_re=$(printf '%s' "$V0" | sed 's/\./\\./g')
    AW=$WORK/ansible
    rm -rf "$AW"
    mkdir -p "$AW"
    chmod 700 "$AW"
    for c in ansible adeb arpm; do docker rm -f "$P-$c" >/dev/null 2>&1 || true; done
    docker build -q -t "$P-ansible" -f "$HERE/images/ansible.Dockerfile" "$HERE/images" >/dev/null
    docker build -q -t "$P-debian12-managed" --build-arg BASE="$P-debian12" -f "$HERE/images/managed.Dockerfile" "$HERE/images" >/dev/null
    docker build -q -t "$P-rocky9-managed" --build-arg BASE="$P-rocky9" -f "$HERE/images/managed.Dockerfile" "$HERE/images" >/dev/null
    docker run -d --name "$P-ansible" --hostname ansible-control --network "$NET" "$P-ansible" >/dev/null
    docker run -d --name "$P-adeb" --hostname pulse-adeb --privileged --cgroupns=private --network "$NET" "$P-debian12-managed" >/dev/null
    docker run -d --name "$P-arpm" --hostname pulse-arpm --privileged --cgroupns=private --network "$NET" "$P-rocky9-managed" >/dev/null
    # 臨時的 SSH 金鑰：公鑰放進受管主機 ops 的 authorized_keys，私鑰只複製到控制端容器
    ssh-keygen -q -t ed25519 -N '' -C "kp systemd test ansible" -f "$AW/id_ed25519"
    : >"$AW/known_hosts"
    for oh in adeb arpm; do
        wait_boot "$oh"
        # Debian 的映像不帶主機金鑰（不把金鑰留在映像裡）：開機後產生
        hx "$oh" sh -c 'ssh-keygen -A >/dev/null 2>&1; systemctl restart ssh.service 2>/dev/null || systemctl restart sshd.service'
        docker cp "$AW/id_ed25519.pub" "$P-$oh:/home/ops/.ssh/authorized_keys"
        hx "$oh" sh -c 'chown ops:ops /home/ops/.ssh/authorized_keys && chmod 600 /home/ops/.ssh/authorized_keys'
        # 主機金鑰經 docker exec（可信的管道）取得，控制端照常檢查主機金鑰
        printf '%s %s\n' "$P-$oh" "$(hx "$oh" cut -d' ' -f1,2 /etc/ssh/ssh_host_ed25519_key.pub)" >>"$AW/known_hosts"
        # 非 root 帳號的 sudo（PAM 的帳號檢查）在測試映像裡可用（images/managed.Dockerfile 的說明）
        check "o/$oh：受管主機上一般帳號的 sudo 可用" \
            "$(hx "$oh" setpriv --reuid ops --regid ops --init-groups sudo -n id -u 2>&1 | tail -n 1)" '^0$'
    done
    SUBNET=$(docker network inspect "$NET" -f "{{(index .IPAM.Config 0).Subnet}}")
    ois=$(kx enroll-token create --name "systest-ansible-$(date +%s)" --group default --max-uses 5 --approval auto \
        --allow-cidr "$SUBNET" --json | jq -r .data.install_string)
    case "$ois" in pulse-enroll:v1:*) ;; *) die "無法建立註冊 token：$ois" ;; esac
    (umask 077 && jq -n --arg s "$ois" '{pulse_install_string: $s}' >"$AW/vars.yml")
    {
        echo "[pulse_agents]"
        for oh in adeb arpm; do
            echo "$oh ansible_host=$P-$oh ansible_user=ops ansible_ssh_private_key_file=/work/id_ed25519 ansible_ssh_common_args='-o UserKnownHostsFile=/work/known_hosts -o StrictHostKeyChecking=yes -o IdentitiesOnly=yes'"
        done
    } >"$AW/inventory.ini"
    # 沒有被信任的發佈公鑰（o1）
    printf 'release@kairos-pulse namespaces="kairos-pulse-release" %s\n' "$(cut -d' ' -f1,2 "$KEYS/evil-a.pub")" >"$AW/wrong_signers"
    cp "$KEYS/allowed_signers" "$AW/allowed_signers"
    cp -r "$REPO/packaging/ansible" "$AW/ansible"
    # 同一份 playbook，只把 become 關掉（o2）
    sed 's/become: true/become: false/' "$AW/ansible/site-pulse.yml" >"$AW/ansible/site-nobecome.yml"
    grep -q 'become: false' "$AW/ansible/site-nobecome.yml" || die "無法產生不用 become 的 playbook"
    mkdir -p "$AW/dist"
    cp "$DIST/$V0"/SHA256SUMS "$DIST/$V0"/SHA256SUMS.sig "$DIST/$V0"/pulse-* "$AW/dist/"
    docker exec "$P-ansible" mkdir -p /work
    docker cp "$AW/." "$P-ansible:/work/"
    docker exec "$P-ansible" sh -c 'chown -R root:root /work && chmod 600 /work/id_ed25519 /work/vars.yml'
    info "ansible：$(ax ansible --version 2>&1 | head -n 1)"
    check "o：控制端經 SSH 以一般帳號連得上兩台受管主機" \
        "$(ax ansible -i /work/inventory.ini pulse_agents -m ansible.builtin.command -a 'id -un' 2>&1 | grep -c '^ops$')" '^2$'
    not_installed() {
        for nh in adeb arpm; do
            hx "$nh" sh -c 'ls -d /opt/pulse /var/tmp/pulse-install.* 2>/dev/null | wc -l'
        done | tr '\n' ' '
    }

    # o1：發佈公鑰不對
    rc=$(ansible_run o1-wrong-key site-pulse.yml wrong_signers)
    check "o1：發佈公鑰不對時 playbook 失敗" "$rc" '^[1-9]'
    check "o1：停在控制端驗證簽章的步驟" "$(grep -c '^fatal: \[adeb -> localhost\]' "$LOGS/ansible-o1-wrong-key.log")" '^1$'
    check "o1：沒有執行之後的步驟" "$(grep -c -E '^TASK \[pulse : (建立只有 root|複製套件|安裝 deb|安裝 rpm|pulse enroll)' "$LOGS/ansible-o1-wrong-key.log")" '^0$'
    check "o1：受管主機沒有安裝、沒有留下暫存目錄" "$(not_installed)" '^0 0 $'

    # o2：不用 become
    rc=$(ansible_run o2-no-become site-nobecome.yml allowed_signers)
    check "o2：不用 become 時 playbook 失敗" "$rc" '^[1-9]'
    check "o2：兩台都停在檢查暫存目錄的步驟" \
        "$(grep -c '這個 role 必須以 root 執行（become: true）' "$LOGS/ansible-o2-no-become.log")" '^2$'
    check "o2：沒有複製也沒有安裝套件" "$(grep -c -E '^TASK \[pulse : (複製套件|安裝 deb|安裝 rpm|pulse enroll)' "$LOGS/ansible-o2-no-become.log")" '^0$'
    check "o2：受管主機沒有安裝、暫存目錄已移除" "$(not_installed)" '^0 0 $'

    # o3：整個 role。執行期間在每台受管主機上等暫存目錄出現，記下它的擁有者與權限，並以連線帳號嘗試進入
    for oh in adeb arpm; do
        hx "$oh" sh -c '
i=0
while [ $i -lt 3000 ]; do
    for d in /var/tmp/pulse-install.*; do
        if [ -d "$d" ]; then
            stat -c "%U:%G %a" "$d"
            if setpriv --reuid ops --regid ops --init-groups ls "$d" >/dev/null 2>&1; then echo listable; else echo denied; fi
            exit 0
        fi
    done
    sleep 0.2
    i=$((i + 1))
done
echo never' >"$AW/tmpdir-$oh.txt" 2>&1 &
    done
    rc=$(ansible_run o3-install site-pulse.yml allowed_signers)
    wait
    check "o3：playbook 成功" "$rc $(grep -c 'failed=0' "$LOGS/ansible-o3-install.log")" '^0 2$'
    check "o3：安裝字串沒有出現在 Ansible 的輸出" "$(grep -c 'pulse-enroll:v1:' "$LOGS/ansible-o3-install.log")" '^0$'
    for oh in adeb arpm; do
        case "$oh" in adeb) om=deb ;; *) om=rpm ;; esac
        check "o3/$oh：暫存目錄是 root 擁有的 0700 目錄，連線帳號進不去" "$(tr '\n' ' ' <"$AW/tmpdir-$oh.txt")" '^root:root 700 denied $'
        check "o3/$oh：暫存目錄用完已移除" "$(hx "$oh" sh -c 'ls -d /var/tmp/pulse-install.* 2>/dev/null | wc -l')" '^0$'
        # shellcheck disable=SC2016 # ${Version} 是 dpkg-query 的格式，不是 shell 變數
        if [ "$om" = deb ]; then
            opv=$(hx "$oh" dpkg-query -W -f='${Version} ${db:Status-Abbrev}' pulse 2>&1)
        else
            opv=$(hx "$oh" rpm -q --qf '%{VERSION}-%{RELEASE}' pulse 2>&1)
        fi
        check "o3/$oh：套件已安裝" "$opv" "^$v0_re-1( ii ?)?\$"
        check "o3/$oh：/opt/pulse/pulse --version" "$(hx "$oh" /opt/pulse/pulse --version 2>&1 | awk '{print $2}')" "^$v0_re\$"
        check "o3/$oh：pulse enroll 取得身分（服務帳號擁有、其他人讀不到）" \
            "$(hx "$oh" stat -c '%U %a' /opt/pulse/identity/client.key 2>&1)" '^pulse [46]00$'
        check "o3/$oh：服務已啟用並啟動" \
            "$(hx "$oh" sh -c 'systemctl is-enabled pulse.service; systemctl is-active pulse.service' 2>&1 | tr '\n' ' ')" '^enabled active $'
        oa=$(hx "$oh" cat /opt/pulse/agent.id | tr -d '\r\n ')
        eval "AGENT_$oh=$oa"
        check "o3/$oh：連上 Kairos（hello 的安裝方式與版本）" "$(wait_install "$oh" "^$om [a-z_]+ (true|false) $v0_re\$")" \
            "^$om [a-z_]+ (true|false) $v0_re\$"
        api GET "/api/v1/hosts/$oa" >/dev/null
        check "o3/$oh：Kairos 的主機清單有這台、連線中" "$(body | jq -r '"\(.data.hostname) \(.data.online // .data.connected // .data.status)"' 2>&1)" \
            "^pulse-$oh (true|online|connected)"
    done

    # o4：再執行一次
    rc=$(ansible_run o4-again site-pulse.yml allowed_signers)
    check "o4：再執行一次仍成功" "$rc $(grep -c 'failed=0' "$LOGS/ansible-o4-again.log")" '^0 2$'
    for oh in adeb arpm; do
        check "o4/$oh：沒有重新註冊（agent.id 不變）、服務照常" \
            "$(hx "$oh" sh -c 'cat /opt/pulse/agent.id; echo; systemctl is-active pulse.service' 2>&1 | tr -d '\r' | tr '\n' ' ')" \
            "^$(agent_of "$oh") active \$"
    done
    for oh in adeb arpm; do
        {
            echo "## journalctl -u pulse"
            hx "$oh" journalctl -u pulse --no-pager -o short-iso 2>&1 | tail -n 60 || true
            echo "## journalctl（ssh、sudo）"
            hx "$oh" journalctl --no-pager -o short-iso 2>&1 | grep -E 'sshd|sudo' | tail -n 40 || true
        } >"$LOGS/ansible-$oh.log"
    done
    # 臨時金鑰與安裝字串：連同控制端、受管主機的容器一起刪除
    if [ -z "${KEEP:-}" ]; then
        for c in ansible adeb arpm; do docker rm -f "$P-$c" >/dev/null 2>&1 || true; done
    fi
    rm -rf "$AW"
    check "o：臨時的 SSH 金鑰與安裝字串已刪除" "$(ls -d "$AW" 2>/dev/null | wc -l)" '^0$'
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

say "匯入版本庫：$V0、$T1、$T2、$T3、$T4、$T5、$T6、$T7、$T8（臨時金鑰 release 簽章）、$T10（release2 簽章）"
for v in "$V0" $T1 $T2 $T3 $T4 $T5 $T6 $T7 $T8 $T10; do
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
    archive-tamper) scenario_archive_tamper ;;
    reboot) scenario_reboot ;;
    reboot-rollback) scenario_reboot_rollback ;;
    signature) scenario_signature ;;
    tamper) scenario_tamper ;;
    degraded) scenario_degraded ;;
    tamper-resume) scenario_tamper_resume ;;
    crash-replace) scenario_crash_replace ;;
    crash-rollback) scenario_crash_rollback ;;
    unsafe-path) scenario_unsafe_path ;;
    late-resume) scenario_late_resume ;;
    key-revoked) scenario_key_revoked ;;
    pkg-rollback-fails) scenario_pkg_rollback_fails ;;
    ansible) scenario_ansible ;;
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
