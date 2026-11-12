# ADR-0002 待決 U4（推送的時段名稱）研究：grok，2026-11-12

> 唯讀研究，未執行建置或測試。

這是產品用語決策研究，全程唯讀。我先盤點 repo 裡兩套「維護時段」的實際用名，同時查證業界用語。盤點已看到兩邊都叫「維護時段」，但英文識別字不同。接著對齊 UI／API／資料庫欄位，並開始查業界用語。規格與 Pulse 實作用語已有落差。接著查業界官方文件，並補齊剩餘識別字。盤點與業界查證已齊。接下來把決策研究寫進計畫，供你核定用語。**首選：告警側維持「維護時段」；升級推送側改稱「升級時段」。英文 API／協定／資料庫鍵全部不動。**

備選：推送側改稱「推送時段」。不要兩邊都維持「維護時段」，也不要把已發佈的告警側改名。

---

## 1. 現況盤點

兩套功能的英文識別字本來就分開；碰撞只發生在中文顯示名稱。

### 功能 1：告警的維護時段（0.6.0 已發佈）

一次性、計畫性作業期間停止通知。事件照常記錄，標 `silenced` 與 `in_maintenance`。不抑制系統事件。週期性時段尚未實作（`ALERTS.md` 第 10 節）。

| 層級 | 名稱 | 位置 |
|---|---|---|
| 介面標題 | 靜音與維護時段 | `kairos/web/assets/js/core/strings-alerts.js` 518 |
| 導覽 | 靜音與維護 | 同檔 10 |
| 按鈕 | 排定維護時段 | 同檔 520、559 |
| 類型 | 維護／維護時段（計畫性作業） | 同檔 537、567 |
| 事件旗標 | 維護中 | 同檔 64；`alerts-common.js` 46 |
| 權限顯示 | 靜音與維護時段檢視／管理 | `strings-admin.js` 35–36（`strings-alerts.js` 22–23 少了「時段」二字） |
| 錯誤文案 | 找不到這個靜音或維護時段；請改用維護時段 | `strings-alerts.js` 36、50 |
| API | `POST /api/v1/silences`，`kind: "maintenance"` | `API.md` 1309–1313；`alerts/model.rs` 486–497 |
| 事件欄位 | `in_maintenance` | `API.md` 1284；`sqlite.rs` 435 |
| 權限鍵 | `silences:read`／`write` | `API.md` 983–984；`rbac.rs` 80–83 |
| 錯誤碼 | `silence_not_found`、`silence_too_long` | `API.md` 1261、1264 |
| 稽核 | `silence.create`（兩種 kind 共用） | `api/alerts.rs` 1520 |
| CLI | `kairos silences --maintenance`；輸出「維護時段已建立…」 | `alerts_cli.rs` 55–68、722–829；`README.md` 688 |
| 通知 | 維護時段「{name}」已結束… | `alerts/notify/render.rs` 244–251 |
| 資料表 | `alert_silences.kind`、`alert_events.in_maintenance` | `sqlite.rs` 457–477 |
| 設定鍵 | 無專用鍵；時區用 `[alerts] timezone` | |

對外識別字已發佈、不該為這次決策改：`kind=maintenance`、`in_maintenance`、`silences`、`--maintenance`。

### 功能 2：升級推送的時段（0.10.0 Unreleased）

推送上每週允許開始替換的時段。時段外不提議新主機；已開始替換的照常完成。實作上有三層英文概念，使用者只看到一層：

1. `schedule.windows[]`：主控台編的每週時段（何時提議）
2. `apply_window`：每則 `upgrade_offer` 的 `not_before`／`not_after`（Pulse 何時替換；0.9 已遵守）
3. 匯流排 `upgrade_window`：進入 `applying` 後自動抑制離線告警（與 1、2 都不是同一件事）

| 層級 | 名稱 | 位置 |
|---|---|---|
| 表單區塊 | 排程與維護時段 | `strings-remote.js` 526；`pulse-rollout-new.js` 360、745 |
| 小標 | 每週的維護時段 | `strings-remote.js` 534 |
| 清單下一步 | 等待維護時段 {t} | 同檔 425–426；`pulse-common.js` 150–151 |
| 詳情 | 目前在維護時段外…；維護時段：{list} | `strings-remote.js` 653–658 |
| API 物件 | `schedule.{starts_at,ends_at,windows[],timezone}` | `API.md` 1499；`schedule.rs` 1–11 |
| 下一步代碼 | `next_action.code = "waiting_window"` | `API.md` 1510；`rollout.rs` 1117–1149 |
| 主機原因 | `reason: "window_closed"` | `engine.rs` 1154 |
| 409 | `schedule_ended`（文案寫「排程的結束時間」，沒寫維護時段） | `engine.rs` 522、644 |
| 協定 | `apply_window` | `PROTOCOL.md` 453；0.9 已發佈 |
| 資料庫 | `rollouts.schedule`（JSON 文字） | `store/sqlite/remote.rs` 126–133 |
| 設定鍵 | **沒有**專用鍵 | |
| Kairos CLI | **沒有** `kairos rollouts` | |
| Pulse 狀態 | 「等待升級時段 {t}」 | `pulse/src/status_cmd/eval.rs` 1641 |
| Pulse 日誌 | 「等待升級時段」「進入升級時段」「升級時段已過」 | `upgrade/mod.rs` 773、1690、1734 |
| 規格漂移 | `REMOTE.md` 952 寫 `pulse status` 顯示「等待維護時段」；實作已是「等待升級時段」 | |

### 改中文顯示名稱要不要動對外識別字？

**不必。** 功能 2 已進對外契約的英文識別字是 `schedule`、`windows`、`waiting_window`、`window_closed`、`schedule_ended`、預覽的 `window`、協定 `apply_window`。這些 0.10 發佈後再改才是破壞性變更。現在只改主控台字串與文件的中文說明。

不確定：設計包 `copywriting.md` §2.2 在 repo 外，本次未讀。

---

## 2. 語意、混淆、連動

| | 告警維護時段 | 推送時段 |
|---|---|---|
| 作用 | 停止通知 | 允許開始替換 Pulse |
| 範圍 | 標籤／網段／主機 + 規則 | 該次推送的對象 |
| 週期 | 一次性（週期性列為未做） | 每週重複，最多 16 段 |
| 已開始的工作 | 期間持續靜音 | 已開始替換的主機不受時段結束影響 |

典型混淆：

1. 在 S10 排了週六深夜的維護時段，以為那時才會升級——其實只是靜音。
2. 在 S26 填了時段，以為升級期間告警會自動靜音——其實不會。
3. 告警側日後做出「週期性維護時段」後，兩邊 UI 都會是「每週勾選星期 + 開始／結束」，同名幾乎無法分辨。
4. `pulse status` 已經寫「等待升級時段」，主控台寫「等待維護時段」。

**沒有共用設定，也沒有「設了 A 就等於 B」。** 唯一相關行為是升級中的離線告警抑制（`ALERTS.md` 5.5）：主機回報 `applying` 後，匯流排 `upgrade_window` 在 `health_timeout_secs + 10 分鐘` 內不開新的離線事件。這不是一筆 `alert_silences`，也不等於 `schedule.windows`。

---

## 3. 業界用語

**「maintenance window／維護時段」在兩個產品類別都是標準詞，意思相反。** Kairos 兩個都做，所以同名特別危險。

查證過、允許變更／更新的時段：

- ConfigMgr：官方繁中就是「[維護時段](https://learn.microsoft.com/zh-tw/intune/configmgr/core/clients/manage/collections/use-maintenance-windows)」；站台伺服器另叫「[服務時段](https://learn.microsoft.com/zh-tw/intune/configmgr/core/servers/manage/service-windows)」
- AWS Systems Manager：官方繁中「[維護時段](https://docs.aws.amazon.com/zh_tw/systems-manager/latest/userguide/maintenance-windows.html)」
- Azure Update Manager：zh-TW 混用「[維護期間](https://learn.microsoft.com/zh-tw/azure/update-manager/scheduled-patching)／維護時段／維護視窗」
- GKE：[Maintenance windows](https://docs.cloud.google.com/kubernetes-engine/docs/how-to/maintenance-windows-and-exclusions)＝允許自動升級的時間
- Windows／Intune：「[活動時段](https://learn.microsoft.com/zh-tw/windows/deployment/update/update-policies)」是禁止重啟的忙碌時段，語意相反

查證過、靜音告警的時段：

- Datadog：[Downtimes](https://docs.datadoghq.com/monitors/downtimes/)（文件把週期性的叫 maintenance windows）
- PagerDuty：直接叫 [Maintenance Windows](https://docs.pagerduty.com/incident-management/services/maintenance-windows)（停用整合、不開新事件）
- Zabbix：[Maintenance](https://www.zabbix.com/documentation/current/en/manual/maintenance)，官方中文「维护时段」
- Grafana／Alertmanager：Silence vs Mute timings；中文「[静默](https://grafana.org.cn/docs/grafana/latest/alerting/configure-notifications/create-silence/)／[靜音計時](https://docs.aws.amazon.com/zh_tw/grafana/latest/userguide/v10-alerting-manage-muting.html)」

憑印象、未逐頁核對：Red Hat Satellite 打 patch 也常用 maintenance window；ITIL 的 change window 指變更窗口。台灣有 SCCM 背景的人聽到「維護時段」第一反應是「這段時間才准部署」；監控背景則是 Zabbix 的抑制通知。

---

## 4. 候選比較

| 候選 | 消混淆 | 台灣習慣 | 破壞性 | 未來若擴到設定下發 | 對英文 |
|---|---|---|---|---|---|
| A. 兩邊都維持「維護時段」 | 差，週期性告警時段做完更差 | 兩邊各自都「對」，合在一起就錯 | 無 | 推送側反而更貼 SCCM／AWS | `windows` 常被譯成這個 |
| **B. (2)→升級時段（首選）** | 好 | 語意直白；與 Pulse 日誌一致 | 只動未發佈文案 | 若設定下發共用同一套 windows 會不夠貼 | 對得上 `apply_window` 與「升級推送」 |
| C. (2)→推送時段（備選） | 好 | 「推送」是本產品用語 | 同 B | 設定叫「集中設定」，一樣蓋不住 | 對 rollout，對不上 `apply_window` |
| D. (2)→部署／變更時段 | 中 | 變更時段偏 ITIL，台灣較少這麼叫 | 同 B | 最好（change window） | `windows` 可當 change window |
| E. (1)改名、(2)留維護時段 | 好 | (2) 對 SCCM 最準 | **高**：0.6 已出貨 | (2) 很貼 | (1) 的 kind 仍是 `maintenance` |
| F. 兩邊都改 | 最好但過量 | — | 高 | 可一次做成靜音時段＋變更時段 | 不該現在動 API |

不要為尚未存在的「集中設定時段」現在就選較泛的「變更時段」。設定若真的要時段，比較像主機層政策，屆時再引入共用名稱比較乾淨。

---

## 5. 建議與改名範圍

採 B 的理由：

1. 兩個功能不是同一件事；告警側路線圖還有「週期性維護時段」，形狀會變得跟推送時段一樣。
2. Pulse 0.9 已經用「升級時段」稱呼 `apply_window`；`schedule.windows` 的作用就是把 `apply_window` 裁到該段。對使用者是同一件事的兩層。
3. 0.10 還沒發佈，只改顯示名稱與文件，沒有相容性破壞。
4. ADR 0002 U4 與 Codex 審查已建議同一方向；業界證據仍支持。

備選 C：若設計包堅持「升級時段」只出現在 Pulse 本機狀態。代價是主控台與 `pulse status` 仍然用詞不同。

不建議 E：為了讓未發佈功能對上 SCCM 用詞，去改已經在值班流程裡的 S10。

若採首選，要改（不含 API 鍵）：

- 字串：`strings-remote.js` 411、425、426、509、526、527、534、578、653、654、658
- e2e：`e2e-remote.mjs` 252、275–279
- 文件：`CHANGELOG.md` 64、`API.md` 17.3 的中文、`REMOTE.md` 4.7.5／13.1／10.3、`kairos/README.md` 670、`web/README.md` S26、`packaging/INSTALL.md` 574、設計包 `copywriting.md` §2.2
- 建議一併改註解，避免之後搜「維護時段」混到兩邊：`schedule.rs`、`rollout.rs`、`engine.rs`

不要改：`schedule`／`windows`／`waiting_window`／`window_closed`／`apply_window`／`rollouts.schedule`／`--maintenance`／`upgrade_window`／告警側所有「維護時段」字串。

改顯示名稱對 0.9 客戶、API 整合、Pulse 0.9 皆無影響。0.10 發佈後再改中文不是 API 破壞，但手冊與截圖會再洗一次，所以要現在定案。
