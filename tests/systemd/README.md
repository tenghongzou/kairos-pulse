# systemd 系統層測試：Pulse 遠端升級（deb／rpm／tar）

`crates/kairos-proto/REMOTE.md` 第 12.3 節的測試：以**真的 systemd**（容器內 PID 1）、真的套件管理員與真的
Kairos，驗證 Pulse 的遠端升級、自動回滾、重新開機接續與簽章錯誤。`tests/e2e` 以假的 systemd 與套件管理員
驗證兩端各自的邏輯；這裡驗證的是 `pulse-upgrade.path`／`pulse-upgrade.service`、`dpkg -i`／`rpm -U`、postinst、
unit 沙箱與重新開機這些只有在實際系統上才會出問題的部分。

**不在 CI 必跑的範圍**：需要 `--privileged` 的 docker、約 3 GB 的映像與建置快取，完整執行約 40～60 分鐘
（第一次另加建置時間；2026-11-13 在 kp-build 沿用建置快取時：建置 9 個發佈約 24 分鐘、10 個情境約 18 分鐘）。在發佈前或修改遠端升級、打包、unit 時，於建置機（kp-build）手動執行。

## 執行

在有 docker 的 Linux 主機（kp-build：Ubuntu 24.04、cgroup v2）上、repo 的 **git clone** 內（打包腳本需要 git
歷史；`kp-remote.sh` 同步的目錄沒有 `.git`，請以 `git clone`／`git bundle` 取得）：

```bash
sh tests/systemd/run.sh                       # 建置發佈物（已建置過的沿用）並執行全部情境
SKIP_BUILD=1 sh tests/systemd/run.sh          # 不重新建置
SCENARIOS="upgrade rollback" sh tests/systemd/run.sh
KEEP=1 sh tests/systemd/run.sh                # 結束時保留容器（除錯）；之後 CLEAN_ONLY=1 sh tests/systemd/run.sh
```

需要 `docker`、`git`、`curl`、`jq`、`ssh-keygen`（OpenSSH 8.1 以上）、`sha256sum`。工作目錄預設 `~/kp-systest`
（`WORK=`；不可在 git repo 內）：`dist/`（發佈物）、`keys/`（臨時金鑰）、`logs/`（每個情境每台主機的日誌）、
`results.txt`（PASS／FAIL 清單）。結束代碼 0 全部通過、1 有失敗。結束時刪除容器與網路；映像
（`kp-systest-*`）預設保留以加速下次執行，`CLEAN_IMAGES=1` 一併刪除。建置快取在 docker volume
`kp-systemd-test-build`（不再需要時 `docker volume rm kp-systemd-test-build`）與共用的 `sync-cargo`。

## 發佈物（`build.sh`）

以 HEAD 打包（`pulse/packaging/build-release.sh`、`kairos/packaging/build-release.sh`，只建 x86_64 musl），版本號的
修改只提交在工作目錄的暫時 clone 中，不影響 repo：

| 目錄 | 內容 |
|---|---|
| `kairos` | Kairos（tar.gz） |
| `<V0>`（例如 `0.10.0`） | Pulse HEAD（`pulse/Cargo.toml` 的版本）：tar.gz、deb、rpm、`pulse-install.sh`、發佈清單 |
| `<N>-test.1`、`<N>-test.3` | 同一份程式碼，只改版本號。`<N>` 是 `<V0>` 的下一個 PATCH（`0.10.0` → `0.10.1`；`build.sh` 與 `run.sh` 各自由 `pulse/Cargo.toml` 算出），所以測試用的版本一定比 HEAD 新，不會被當成降版 |
| `<N>-test.2`、`<N>-test.4`、`<N>-test.7` | 另加「沒有參數啟動（服務）時立刻以結束代碼 1 結束」：通過輔助程式的預檢（`--version`、`check-config`），啟動後一直崩潰 |
| `<N>-test.5`、`<N>-test.6` | 同 test.1，另以 `TEST_FEATURES=fault-injection` 開啟 Pulse 的編譯期 feature（REMOTE.md 第 17.9.9 節）：**測試用建置**，升級輔助程式可以在指定的當機點停住。`pulse --version` 結尾有 `[fault-injection]` |
| `evil-a` | test.1 的檔案，`SHA256SUMS` 以沒有人信任的金鑰簽章 |
| `<N>-test.9` | test.1 的檔案改名為 test.9，以只有 Kairos 信任的金鑰簽章 |

簽章一律用測試時以 `ssh-keygen -t ed25519` 產生的臨時金鑰（`packaging/sign-release.sh`，namespace
`kairos-pulse-release`）；Pulse 的信任錨（`/etc/kairos-pulse/allowed_signers`）只有 `release`，Kairos 的
`[remote] allowed_signers` 有 `release` 與 `evil-b`。**不使用正式的離線發佈金鑰。**

**故障注入只在 test.5、test.6**：其餘版本（含安裝在主機上的 `<V0>`，情境 b0～f 用到的全部版本）都是與正式發佈
相同方式建置的執行檔。打包腳本對每一個版本執行 `packaging/check-release-binary.sh`——正式的建置含故障注入的
標記就中止打包，測試用建置反過來要求標記存在；`build.sh` 建置後確認兩種檢查都有執行過、以 `tar.gz` 內的執行檔
再確認一次，並確認 `packaging/release.sh` 在設定了 `TEST_FEATURES` 時拒絕執行。

## 環境（`run.sh`）

- docker 網路 `kp-systest-net`；Kairos 容器（`debian:12`，名稱 `kairos.test`）：`kairos pki init`、mTLS、線上簽發、
  註冊端點 `open`、`[remote]`；主控台 API 只發佈到主機的 `127.0.0.1:19443`。
- 三台 systemd 主機（`--privileged --cgroupns=private`，`/sbin/init` 為 PID 1，`STOPSIGNAL SIGRTMIN+3`
  讓 `docker restart` 等於正常的重新開機）：

  | 主機 | 映像 | 安裝方式 |
  |---|---|---|
  | `deb` | Debian 12 | `pulse-install.sh --dir`（驗證簽章 → `apt-get install` → 信任錨 → `pulse enroll` → 啟動） |
  | `rpm` | Rocky Linux 9 | 同上（`dnf install`） |
  | `tar` | Ubuntu 24.04 | INSTALL.md 第 3、4、5.0、7、10.1 節（手動安裝 tar.gz、`pulse enroll`、兩個輔助程式 unit） |

  一行安裝腳本只處理 deb／rpm，tar 安裝依 INSTALL 手動執行。
- **容器偵測的繞過（只在測試映像）**：Pulse 在容器中回報 `install.upgrade = "unsupported"`（REMOTE.md 第 4.4.5
  節：`/.dockerenv` 或 `/proc/1/cgroup` 含容器字樣）。測試主機模擬實體主機，映像中的 `kp-undocker.service`
  每次開機在 Pulse 啟動前刪除 `/.dockerenv`；`--cgroupns=private` 下 `/proc/1/cgroup` 為 `0::/`。
- 推送：單一批次、`max_failure_ratio: 1`（失敗也不暫停，讓三台都跑完）、`health_timeout_secs: 120`
  （`HEALTH_TIMEOUT=`）。推送引擎在 Kairos 啟動後 1 分鐘才開始提議（第 4.7.4 節第 5 項）。

## 情境

依序執行，後一個情境沿用前一個的版本：

| 情境 | 推送 | 預期 |
|---|---|---|
| `rollback-first`（b0） | `<V0>` → test.2（崩潰） | 第一次遠端升級：本機沒有 `<V0>` 的已驗證發佈，回滾檔不可信（REMOTE.md 第 17.3 節第 1 項）→ 三台以 `pulse.prev` 換回執行檔、`rolled_back`（`PLS-W8407`）；deb／rpm 的套件資料庫仍是 test.2，`detail` 有 `binary_restored_package_db_shows_0.9.1-test.2` |
| `upgrade`（a） | `<V0>` → test.1 | 套件資料庫是 test.2（比目標新：rpm 需要 `--oldpackage`）。三台 `succeeded`（`PLS-I8416`）；`--version`、`dpkg-query`／`rpm -q`、Kairos 的 `agent_version` 為 test.1；tar 的 `pulse.prev` 為 0.9.0；推送 `completed` |
| `rollback`（b） | test.1 → test.2（崩潰） | 三台 `rolled_back`（`PLS-W8407`，`crash_restart`）、回到 test.1 且服務正常；deb／rpm 以 `rollback_artifact`（`dpkg -i`／`rpm -U --oldpackage --replacepkgs`）回滾，套件資料庫也是 test.1 |
| `reboot`（c） | test.1 → test.3 | 輔助程式在健康確認（`state.json` 的 `replaced`）時重新開機；開機後 `pulse-upgrade.path` 再次觸發、接續健康確認 → `succeeded` |
| `reboot-rollback`（c2） | test.3 → test.4（崩潰） | 回滾確認（`rolling_back`）時重新開機；開機後接續 → `rolled_back`、回到 test.3 |
| `signature`（d） | — ／ test.9 | d1：Kairos 拒絕匯入 `evil-a`（400 `release_signature_invalid`）；d2：Kairos 匯入 test.9，Pulse 下載前驗證簽章失敗 → `failed`（`PLS-E8401`）、版本不變、推送自動暫停 |
| `tamper`（e） | （不經 Kairos）test.4＋謊報的 `from_version` | 0.10.0 的安全性修正（REMOTE.md 第 5.1 節）：在 deb、rpm 主機上**以服務帳號**自己寫 `request.json`——目標是有官方簽章的 test.4（崩潰，健康確認一定失敗），`from_version` 謊報為較舊的 `<V0>` 並附上 `<V0>` 有官方簽章的套件當回滾檔。輔助程式以自己的版本為準：`failed`（`PLS-E8402`，`from_version_mismatch: request says <V0>, installed version is <目前的版本>`），執行檔與套件資料庫都沒有改變、沒有執行套件管理員、`request.json` 已刪除、服務照常執行。修正前同樣的請求會讓 root 以 `dpkg -i`／`rpm -U --oldpackage` 把主機降到 `<V0>` |
| `tamper-resume`（f） | 目前的版本 → test.4（崩潰） | 進行中交易的指標與交易紀錄（REMOTE.md 第 17.8、17.9 節）：回滾確認（`rolling_back`）期間**以服務帳號**把 `request.json` 的 `upgrade_id` 改成另一個 UUID、`from_version` 改成謊報的 `0.0.1`，然後重新開機。輔助程式仍接續原本的交易：三台 `rolled_back`（`PLS-W8407`）、`detail` 加註 `request_upgrade_id_ignored` 與 `request_from_version_ignored`，沒有為假的 `upgrade_id` 建立工作目錄，`/var/lib/pulse-upgrade/active.json` 已清除，`pulse.prev` 仍是替換前的版本。另外檢查竄改當下 root 的檔案：`active.json`（schema `pulse.upgrade-active/v1`）與它指向的 `state.json`（schema `pulse.upgrade-helper/v2`、步驟 `rolling_back`）的 `checksum` 以 `sha256sum` 重新計算相符、兩者的 `upgrade_id` 與 `binding_sha256` 相同、紀錄有替換前的版本、新舊執行檔與發佈檔的雜湊、`prev_saved` 與回滾的意圖；工作目錄 0700、兩個檔案 0600、都屬於 root，服務帳號讀不到 `/var/lib/pulse-upgrade`。交易結束後紀錄為 `done`、工作目錄只留 `state.json` 與 `upgrade.log`、`/var/lib/pulse-upgrade` 只保留最近一筆交易 |
| `crash-replace`（g） | （test.3 → test.5）→ test.6 | 當機點（REMOTE.md 第 17.9.4 節第 11 列）。主機上的輔助程式不是測試用建置時，先以一次正常的推送升級到 test.5。在每台主機放好掛鉤 `/var/lib/pulse-upgrade/fault-inject`（內容 `binary_replaced`），推送 test.6：輔助程式在**執行檔已替換（deb／rpm：套件已安裝完成）、還沒記下 `replaced`** 時停住，腳本確認當時的紀錄是 `replacing`（checksum 相符）、執行檔已是 test.6、`pulse.prev` 是 test.5，然後 `docker kill`（SIGKILL PID 1）並重新啟動容器。開機後輔助程式「接續上次中斷的替換」：不再保留 `pulse.prev`（「不再覆寫」）、健康確認 → 三台 `succeeded`（`PLS-I8416`），`pulse.prev` 仍是 test.5，指標已清除 |
| `crash-rollback`（h） | test.6 → test.7（崩潰） | 當機點（第 17 列）：掛鉤 `rollback_restored`。新版崩潰 → 輔助程式記下回滾的意圖、還原（tar：換回 `pulse.prev`；deb／rpm：以回滾套件經套件管理員裝回），在**已還原、還沒記下 `rolling_back`** 時停住；腳本確認紀錄是 `rollback_intent`（`cause: health`，`via` 為 `binary`／`package`）、執行檔已是 test.6，然後 `docker kill` 並重新啟動。開機後「接續上次中斷的回滾：重新還原」（冪等重做）→ 三台 `rolled_back`（`PLS-W8407`，`detail` 為「<原因>; resumed」），deb／rpm 的套件資料庫也是 test.6 |

每個情境結束後的日誌：`logs/<情境>-<主機>.log`（`pulse upgrade status`、`journalctl -u pulse-upgrade`、
輔助程式的 `upgrade.log`、Pulse 日誌中與升級有關的行）、`logs/<情境>-kairos.log`。

情境 g、h 的 `docker kill` 等同「所有行程立即終止」，不是真正的斷電：已寫入頁面快取的資料不會遺失。其餘的
當機點（REMOTE.md 第 17.9.4 節共 22 列）由單元測試的故障注入矩陣涵蓋（`pulse/src/remote/upgrade/helper/fault_tests.rs`）；
要在實機重現其他的當機點，把掛鉤的內容換成那個 checkpoint 的名稱即可（只有 test.5、test.6 的輔助程式會讀）。

## 沒有涵蓋

- 真正的斷電（頁面快取遺失、fsync 之前的寫入消失）：情境 g、h 只終止行程。

- 套件資料庫被鎖住時的重試（第 12.3 節「另測」）：需要在替換的那一刻持有 dpkg／rpm 的鎖，時序不易穩定重現，
  以 `remote::upgrade::helper::tests` 的假套件管理員涵蓋。
- tar 安裝替換已修改／未修改的 `pulse.service`：測試的新舊版 unit 相同（同一個 commit），輔助程式不替換 unit；
  判斷邏輯由單元測試涵蓋。
- aarch64、Debian 11／RHEL 8（OpenSSH 8.0 沒有 `ssh-keygen -Y verify`）、Windows（0.10.0）。
