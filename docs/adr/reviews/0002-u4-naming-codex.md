# ADR-0002 待決 U4（推送的時段名稱）研究：codex，2026-11-12

> 唯讀研究，未執行建置或測試。

結論先講：我建議把升級推送功能的中文名稱改成「升級時段」，保留告警功能既有的「維護時段」。這能直接消除介面混淆，而且不需要更動任何 API、協定、設定鍵或資料庫欄位，因此不構成機器介面的破壞性變更。

業界並沒有把 Maintenance Window 專門保留給「允許變更」或「靜音告警」其中一種用途；兩邊都有產品使用。這反而意味著 Kairos 必須靠產品內部的限定詞區分，而「升級時段」是目前最準確、成本最低的限定詞。

## 1. Repo 現況盤點

### (1) 告警的維護時段

| 層級 | 現有名稱／識別字 | 證據 |
|---|---|---|
| 導覽與權限 | 「靜音與維護」、「靜音與維護檢視／管理」 | [strings-alerts.js:10–23](/F:/sync-project/kp-s11-adr/kairos/web/assets/js/core/strings-alerts.js:10) |
| 頁面標題 | 「靜音與維護時段」 | [strings-alerts.js:517–530](/F:/sync-project/kp-s11-adr/kairos/web/assets/js/core/strings-alerts.js:517) |
| 建立動作 | 「臨時靜音」、「排定維護時段」 | [strings-alerts.js:519–520](/F:/sync-project/kp-s11-adr/kairos/web/assets/js/core/strings-alerts.js:519) |
| 類型名稱 | 「維護」、「維護時段（計畫性作業）」；英文值 `maintenance` | [strings-alerts.js:536–568](/F:/sync-project/kp-s11-adr/kairos/web/assets/js/core/strings-alerts.js:536)、[alerts-silences.js:255–272](/F:/sync-project/kp-s11-adr/kairos/web/assets/js/screens/alerts-silences.js:255) |
| 狀態標示 | 「維護中」；事件欄位 `in_maintenance` | [strings-alerts.js:63–64](/F:/sync-project/kp-s11-adr/kairos/web/assets/js/core/strings-alerts.js:63)、[API.md:1284](/F:/sync-project/kp-s11-adr/kairos/API.md:1284) |
| 功能說明 | 「計畫性停機前先建立維護時段，可以避免…誤報」；一次性、最長 366 天 | [strings-alerts.js:551–578](/F:/sync-project/kp-s11-adr/kairos/web/assets/js/core/strings-alerts.js:551)、[ALERTS.md:161–172](/F:/sync-project/kp-s11-adr/kairos/ALERTS.md:161) |
| API 資源 | `/api/v1/silences`、`kind: "maintenance"`、`starts_at`、`ends_at` | [API.md:1309–1313](/F:/sync-project/kp-s11-adr/kairos/API.md:1309) |
| API 事件旗標 | `silenced`、`in_maintenance` | [API.md:1284](/F:/sync-project/kp-s11-adr/kairos/API.md:1284) |
| API 錯誤 | `silence_not_found`、`silence_too_long`；均無 KRS 產品代碼 | [API.md:1256–1265](/F:/sync-project/kp-s11-adr/kairos/API.md:1256) |
| 稽核／通知事件 | `silence.create`、`silence.update`、`silence.end`；結束通知 `silence_end` | [api/alerts.rs:1516–1530](/F:/sync-project/kp-s11-adr/kairos/src/api/alerts.rs:1516)、[api/alerts.rs:1580](/F:/sync-project/kp-s11-adr/kairos/src/api/alerts.rs:1580)、[alerts/eval.rs:313–389](/F:/sync-project/kp-s11-adr/kairos/src/alerts/eval.rs:313) |
| 資料庫 | `alert_silences`，欄位 `kind`、`starts_at`、`ends_at` 等；值為 `maintenance` | [sqlite.rs:457–478](/F:/sync-project/kp-s11-adr/kairos/src/store/sqlite.rs:457)、[alerts/model.rs:483–512](/F:/sync-project/kp-s11-adr/kairos/src/alerts/model.rs:483) |
| 事件資料庫 | `alert_events.silenced`、`alert_events.in_maintenance` | [sqlite.rs:401–435](/F:/sync-project/kp-s11-adr/kairos/src/store/sqlite.rs:401) |
| CLI 名稱 | `kairos silences`；`--maintenance`；輸出「維護時段」或「臨時靜音」 | [alerts_cli.rs:54–69](/F:/sync-project/kp-s11-adr/kairos/src/alerts_cli.rs:54)、[alerts_cli.rs:826–836](/F:/sync-project/kp-s11-adr/kairos/src/alerts_cli.rs:826) |
| CLI 空狀態／錯誤建議 | 「目前沒有靜音或維護時段」；「改用維護時段（--maintenance）」 | [alerts_cli.rs:645–648](/F:/sync-project/kp-s11-adr/kairos/src/alerts_cli.rs:645)、[alerts_cli.rs:768–772](/F:/sync-project/kp-s11-adr/kairos/src/alerts_cli.rs:768) |
| 設定鍵 | 沒有「維護時段」的 `kairos.toml` 設定鍵；規則存在資料庫，由 API／CLI 管理。`alerts.timezone` 只是共用時區 | [ALERTS.md:53–64](/F:/sync-project/kp-s11-adr/kairos/ALERTS.md:53) |

所以 (1) 的「維護時段」不是純顯示文字：已發佈的機器介面包含 `kind: maintenance`、資料表 `alert_silences`、CLI `--maintenance` 與事件欄位 `in_maintenance`。如果要反過來全面改英文識別字，會是明確的破壞性變更；單改中文顯示則不會破壞 API，但會造成既有使用者的術語遷移成本。

### (2) 升級推送時段

| 層級 | 現有名稱／識別字 | 證據 |
|---|---|---|
| 功能摘要 | 「限定在每週的維護時段內開始替換」 | [strings-remote.js:411](/F:/sync-project/kp-s11-adr/kairos/web/assets/js/core/strings-remote.js:411) |
| 表單區塊 | 「排程與維護時段」 | [strings-remote.js:526–527](/F:/sync-project/kp-s11-adr/kairos/web/assets/js/core/strings-remote.js:526) |
| 時段標題 | 「每週的維護時段」 | [strings-remote.js:534](/F:/sync-project/kp-s11-adr/kairos/web/assets/js/core/strings-remote.js:534) |
| 狀態文字 | 「等待維護時段」、「維護時段外」 | [strings-remote.js:425–426](/F:/sync-project/kp-s11-adr/kairos/web/assets/js/core/strings-remote.js:425) |
| 預覽／詳情 | 「目前在維護時段外」、「只在維護時段內開始替換主機」、「維護時段：…」 | [strings-remote.js:509–534](/F:/sync-project/kp-s11-adr/kairos/web/assets/js/core/strings-remote.js:509)、[strings-remote.js:578](/F:/sync-project/kp-s11-adr/kairos/web/assets/js/core/strings-remote.js:578)、[strings-remote.js:653–658](/F:/sync-project/kp-s11-adr/kairos/web/assets/js/core/strings-remote.js:653) |
| API 本文 | `schedule: { starts_at, ends_at, windows[], timezone }` | [API.md:1492–1500](/F:/sync-project/kp-s11-adr/kairos/API.md:1492) |
| API 預覽 | `schedule`、`window.open`、`window.until`／`next`、`default_timezone` | [API.md:1508–1509](/F:/sync-project/kp-s11-adr/kairos/API.md:1508) |
| API 狀態 | `scheduled`、`next_action.code: waiting_window` | [API.md:1510](/F:/sync-project/kp-s11-adr/kairos/API.md:1510) |
| API 錯誤／原因 | 409 `schedule_ended`；主機原因 `window_closed`、`rollout_ended` | [API.md:1514–1515](/F:/sync-project/kp-s11-adr/kairos/API.md:1514)、[API.md:1556–1559](/F:/sync-project/kp-s11-adr/kairos/API.md:1556)、[remote/rollout.rs:158–164](/F:/sync-project/kp-s11-adr/kairos/src/remote/rollout.rs:158) |
| Pulse 協定 | `upgrade_offer.apply_window.not_before/not_after` | [REMOTE.md:133–148](/F:/sync-project/kp-s11-adr/crates/kairos-proto/REMOTE.md:133)、[mgmt/mod.rs:749–797](/F:/sync-project/kp-s11-adr/crates/kairos-proto/src/mgmt/mod.rs:749) |
| 資料庫 | `rollouts.schedule TEXT NOT NULL`，內容為 JSON；沒有名為 `maintenance_window` 的欄位 | [store/sqlite/remote.rs:126–135](/F:/sync-project/kp-s11-adr/kairos/src/store/sqlite/remote.rs:126)、[REMOTE.md:770–786](/F:/sync-project/kp-s11-adr/crates/kairos-proto/REMOTE.md:770) |
| 設定鍵 | 沒有專屬 `maintenance_window`／`upgrade_window` 設定鍵。時段在每筆 rollout 的 API／資料庫 `schedule` 內；省略 `timezone` 時借用 `alerts.timezone` | [REMOTE.md:1078–1084](/F:/sync-project/kp-s11-adr/crates/kairos-proto/REMOTE.md:1078)、[remote/mod.rs:242–322](/F:/sync-project/kp-s11-adr/kairos/src/remote/mod.rs:242) |
| CLI | repo 中沒有管理 rollout 排程的 CLI；目前是 Web 主控台與 API 功能。`kairos releases` 是版本庫 CLI，不是推送時段 CLI | [CHANGELOG.md:256](/F:/sync-project/kp-s11-adr/CHANGELOG.md:256) |
| 文件名稱 | 「排程與維護時段」、「推送的維護時段」 | [REMOTE.md:1076–1084](/F:/sync-project/kp-s11-adr/crates/kairos-proto/REMOTE.md:1076)、[API.md:1488–1499](/F:/sync-project/kp-s11-adr/kairos/API.md:1488)、[CHANGELOG.md:64](/F:/sync-project/kp-s11-adr/CHANGELOG.md:64) |

關鍵判斷：

- `schedule`、`windows` 已是 0.10 API 欄位。
- `scheduled`、`waiting_window`、`window_closed`、`rollout_ended` 已是 API 狀態／原因碼。
- `apply_window` 是 Kairos–Pulse 協定欄位，而且 Pulse 0.9.0 已經支援，不能為了中文改名而動它：[REMOTE.md:1080–1084](/F:/sync-project/kp-s11-adr/crates/kairos-proto/REMOTE.md:1080)。
- 但是「升級時段」是中文產品名稱，不必與英文欄位逐字同名。`schedule.windows` 和 `apply_window` 都能自然承載「升級時段」。
- 因此本次只需改顯示字串與說明文件；不應更動任何對外識別字。

## 2. 實際語意與連動

### 兩者的差異

告警維護時段：

- 對象是告警規則與主機範圍。
- 目的在停止通知、重送與升級通知。
- 事件仍照常建立並記錄，標示 `silenced`；維護類另標 `in_maintenance`。
- 不抑制系統事件。
- 一次性，最長 366 天。
- 不會安排、啟動或限制 Pulse 升級。[ALERTS.md:161–172](/F:/sync-project/kp-s11-adr/kairos/ALERTS.md:161)

升級時段：

- 對象是某一筆升級推送內的主機。
- 目的在限制「開始替換 Pulse」的時間。
- 時段外不提出新的升級；下載與驗證可提早完成。
- 已進入替換的主機不會因時段結束而中止。
- 未開始的主機在下一個時段重新提議；`ends_at` 到期則成為 `rollout_ended`。[API.md:1499](/F:/sync-project/kp-s11-adr/kairos/API.md:1499)

容易發生的誤解包括：

- 使用者先在「告警 → 維護時段」排定凌晨 01:00，誤以為升級推送會自動等到 01:00。
- 在推送畫面設定「維護時段」，誤以為同一時段的 CPU、記憶體、憑證等告警都會被靜音。
- 誤以為兩個頁面是在編輯同一個共用物件，或認為要先建立告警維護時段才能建立升級推送。
- 誤以為時段結束會停止正在替換的 Pulse；實際上只禁止新的主機開始。
- API 使用者可能把 `schedule.windows` 誤解成引用 `alert_silences` 的既有資料，而實際上它是 rollout 內嵌 JSON。

### 現有連動

兩套「時段」本身完全沒有連動：

- 推送引擎不查詢 `alert_silences`。
- 告警維護時段不會啟動或延後 rollout。
- `schedule.windows` 不會建立 `alert_silences` 記錄，也不會把主機標成 `in_maintenance`。

但目前已有一項獨立的自動保護：

- Pulse 回報進入 `applying` 後，遠端管理服務發布內部 `upgrade_window` 事件。
- 在 `applying` 到終止狀態或安全期限之間，告警引擎不建立新的 `host_offline`、`host_long_offline`，主機也不計入網段／大規模離線統計。
- 其他告警不受影響；已經開著的離線事件也不會被關閉。
- 這不是「靜音既有事件」，而是暫時不建立新的離線事件；期限過後仍離線就正常開事件。
- 會寫 `details.upgrade.suppressed_until` 與時間軸動作 `upgrade_suppressed`。[ALERTS.md:174–186](/F:/sync-project/kp-s11-adr/kairos/ALERTS.md:174)、[remote/mod.rs:742–825](/F:/sync-project/kp-s11-adr/kairos/src/remote/mod.rs:742)、[alerts/eval.rs:925–958](/F:/sync-project/kp-s11-adr/kairos/src/alerts/eval.rs:925)

這項行為反而更需要把兩個名稱分開：推送確實會自動抑制一小部分離線告警，但它不等於啟用了告警的「維護時段」。

## 3. 業界用語調查

### 已由官方文件查證：變更、更新與修補

| 產品 | 官方英文用語 | 已查證的繁中／中文用語 | 判讀 |
|---|---|---|---|
| Windows Update／Intune | Update rings、Active hours、Scheduled install time、Automatic maintenance | 台灣 Microsoft 使用「更新通道」、「使用時間」、「排定的安裝時間」、「自動維護」 | 沒有只靠 Maintenance Window 一詞；會依行為區分更新排程、使用時間與維護。[Microsoft Intune](https://learn.microsoft.com/zh-tw/intune/device-updates/windows/ref-update-ring-settings) |
| Windows 11 新政策 | Maintenance windows | 「維護時段」 | Microsoft 2026 文件明確把更新允許時段稱為維護時段。[Microsoft Windows Update](https://learn.microsoft.com/zh-tw/windows/deployment/update/wufb-compliancedeadlines) |
| WSUS | Scheduled installation、deadline、synchronization schedule | 本次未找到可直接引用的現行繁中頁 | WSUS 傳統上多用排定安裝時間／期限，而不是把每個更新排程物件都叫 Maintenance Window。[Microsoft WSUS](https://learn.microsoft.com/en-us/windows-server/administration/windows-server-update-services/plan/plan-your-wsus-deployment) |
| AWS Systems Manager | Maintenance Windows；Patch Manager | AWS 繁中為「維護時段」 | 明確用來排定修補、驅動程式更新、軟體安裝等可能中斷服務的動作。[AWS Maintenance Windows](https://docs.aws.amazon.com/systems-manager/latest/userguide/maintenance-windows.html)、[AWS 繁中排程說明](https://docs.aws.amazon.com/zh_tw/systems-manager/latest/userguide/maintenance-windows-schedule-options.html) |
| Azure Update Manager | Maintenance configuration、maintenance window | 「維護設定」、「維護時段」 | 更新在客戶定義的維護時段內執行；進行中的更新可能跨越時段完成，與 Kairos 很相近。[Azure Update Manager](https://learn.microsoft.com/zh-tw/azure/update-manager/overview)、[前置與後置事件](https://learn.microsoft.com/zh-tw/azure/update-manager/pre-post-scripts-overview) |
| Google VM Manager | Patch deployment schedule、duration／maintenance window | 未查到官方繁中在地化用語 | 設定頻率與 duration，確保修補工作不會在指定 maintenance window 外開始，語意和 Kairos 幾乎相同。[Google VM Manager](https://docs.cloud.google.com/compute/vm-manager/docs/patch) |
| GKE | Maintenance window、maintenance exclusion | 未查到官方繁中在地化用語 | Kubernetes 相關託管產品用 maintenance window 控制叢集自動維護／升級；另有 exclusion。[GKE 維護時段文件](https://docs.cloud.google.com/kubernetes-engine/docs/how-to/maintenance-windows-and-exclusions) |
| Red Hat Satellite | maintenance window；remote execution | 本次未查到官方繁中 | 官方資料確實建議在 maintenance window 中以 Satellite remote execution 批次處理主機；但我沒有查到足夠證據證明目前 Satellite 有一個與 AWS 同型的第一級「Maintenance Window」排程物件，因此只能列為有限查證。[Red Hat 文件](https://docs.redhat.com/es/documentation/subscription_central/1-latest/epub/getting_started_with_simple_content_access/assembly-about-simplecontent_simplecontentaccess_guide) |

### 已由官方文件查證：告警靜音

| 產品 | 用語 | 實際行為 |
|---|---|---|
| Datadog | Downtime；Schedule Downtime；mute | Downtime 靜音告警與通知，但監控狀態仍會轉換；週期性 downtime 可用於 recurring maintenance windows。[Datadog Downtimes](https://docs.datadoghq.com/monitors/downtimes/) |
| PagerDuty | Maintenance Window | 服務在 Maintenance Window 中不建立新 incident，也不通知值班人員。[PagerDuty](https://support.pagerduty.com/main/lang-ja/docs/why-incidents-fail-to-trigger) |
| Zabbix | Maintenance、Maintenance period | 維護用於在預定期間抑制問題；可選擇是否收集資料。官方現行中文頁是簡體中文，不是台灣在地化。[Zabbix 維護](https://www.zabbix.com/documentation/current/zh/manual/maintenance) |
| Prometheus Alertmanager | Silence、`mute_time_intervals`、`active_time_intervals` | route 在 mute interval 中不送通知，但其他處理照常；另有一次性 Silence。[Alertmanager 設定](https://prometheus.io/docs/alerting/latest/configuration/) |
| Grafana Alerting | Silence、Mute timing、Active time interval | Silence 通常為固定起訖的一次性靜音；Mute timing 是週期性通知靜音，不停止規則評估。[Grafana](https://grafana.com/docs/grafana/latest/alerting/configure-notifications/mute-timings/) |

### 調查結論

- Maintenance Window／「維護時段」在業界兩邊都有人用：
  - AWS、Azure、GKE、Google Patch：允許更新／維護開始。
  - PagerDuty、Zabbix：抑制事件或通知。
- 因此不能僅靠「符合業界用語」決定哪一邊獨占「維護時段」。
- 更明確的產品通常使用限定詞或不同名詞，例如 Patch deployment、Scheduled install、Downtime、Silence、Mute timing。
- 台灣 IT 人員能理解「維護時段」，但在同一套產品內同時出現兩個不相連的「維護時段」，限定詞比照搬英文慣例更重要。
- 我沒有查到 Datadog、PagerDuty、Grafana、Prometheus 或 Red Hat Satellite 的可靠官方繁體中文（台灣）介面用語；沒有把簡中翻譯當成台灣在地化證據。

## 4. 候選名稱比較

| 方案 | 消除混淆 | 業界／台灣習慣 | 既有功能影響 | 未來擴及設定下發 | 與英文識別字 |
|---|---|---|---|---|---|
| 兩者都維持「維護時段」 | 否 | 單獨看都合理，但同產品內衝突 | 無 | 廣義名稱可涵蓋 | `windows` 可對應，但使用者無法知道是哪一組 |
| (2) 改「升級時段」 | 是 | 「升級」是台灣系統管理常用語，直接指出受控動作 | (1) 完全不動；(2) 尚未發佈，成本最低 | 若未來同一時段真的控制設定下發，名稱會偏窄 | 與 `apply_window`、upgrade rollout 語意吻合；不要求逐字同名 |
| (2) 改「推送時段」 | 大致是 | 「推送」符合產品既有「升級推送」 | 無已發佈相容問題 | 可涵蓋其他下發 | 容易誤以為限制下載／傳輸；實際下載可提早，限制的是開始替換，因此語意不夠準 |
| (2) 改「部署時段」 | 是 | 台灣 IT 可理解，但「部署 Pulse」可能被理解為初次安裝或應用部署 | 無已發佈相容問題 | 比「升級」更廣 | 與 rollout 接近，但與目前 UI 的「升級推送」不完全一致 |
| (2) 改「變更時段」 | 是 | 接近 ITSM change window，專業但較抽象 | 無已發佈相容問題 | 最適合未來統一管理升級、設定、其他變更 | `schedule.windows` 很通用；但目前 `apply_window` 只套用升級，名稱超前於實作 |
| (1) 改「告警靜音時段」，(2) 保留「維護時段」 | 是 | Silence／Mute 的業界依據強 | (1) 是已發佈用語，會造成 UI、文件、CLI 教材與操作習慣變更；若連 `maintenance` 一起改則是破壞性 API 變更 | (2) 廣義可擴充 | (1) 既有 `kind: maintenance`、`in_maintenance` 會和中文新名稱產生落差 |
| 兩者都改名，例如「告警靜音時段」＋「升級時段」 | 是，最徹底 | 每個名稱都明確 | 不必要地擾動已發佈的 (1) | 可按需求命名 | 需要接受 (1) 中文和既有英文 API 不一致，或付出 API 遷移成本 |

補充兩個語意細節：

- 「推送時段」不夠精準：Pulse 可以在時段前下載、驗證、暫存；被限制的是「開始替換」。
- 「部署時段／變更時段」雖較有延展性，但名稱應描述現在存在的能力，而不是尚未承諾的未來架構。若未來建立真正共用的 change gate，屆時可以新增一個上位概念，不必現在先把單純升級抽象化。

## 5. 建議

### 首選：「升級時段」

建議用語：

- 區塊：「排程與升級時段」
- 欄位群組：「每週升級時段」
- 狀態：「等待升級時段」
- 空窗狀態：「目前不在升級時段內」
- 摘要：「升級時段：週一～週五 01:00～05:00」
- 說明保留精確語意：「升級時段是允許主機開始替換 Pulse 的時間。下載與驗證可提早進行；已開始替換的主機會照常完成。」

理由：

1. 直接對應目前唯一受控的動作：Pulse 升級。
2. 不會讓人以為與告警維護時段共用設定。
3. 比「推送時段」更準確，因為不是限制下載。
4. 保留已發佈的告警術語與 API。
5. 不需要改任何英文識別字。
6. ADR 與外部審查已有相同結論：[ADR:204–208](/F:/sync-project/kp-s11-adr/docs/adr/0002-rollout-console-design.md:204)、[review:163](/F:/sync-project/kp-s11-adr/docs/adr/reviews/0002-codex-review.md:163)。

### 備選：「變更時段」

只在 0.10 發佈前已明確決定以下架構時採用：

- 同一個時段物件很快會同時控制升級、集中設定下發與其他遠端變更；
- API 也準備維持通用的 `schedule.windows`；
- UI 能清楚列出「適用的變更類型」。

否則目前用「變更時段」會過度抽象，讓使用者以為 Kairos 已有一般遠端變更／指令執行能力，而 REMOTE.md 明確說它不是任意遠端執行通道：[REMOTE.md:17](/F:/sync-project/kp-s11-adr/crates/kairos-proto/REMOTE.md:17)。

### 需要修改的位置

若採首選，建議修改：

- 主控台字串：
  - [strings-remote.js:411](/F:/sync-project/kp-s11-adr/kairos/web/assets/js/core/strings-remote.js:411)
  - [strings-remote.js:425–426](/F:/sync-project/kp-s11-adr/kairos/web/assets/js/core/strings-remote.js:425)
  - [strings-remote.js:509–534](/F:/sync-project/kp-s11-adr/kairos/web/assets/js/core/strings-remote.js:509)
  - [strings-remote.js:578](/F:/sync-project/kp-s11-adr/kairos/web/assets/js/core/strings-remote.js:578)
  - [strings-remote.js:653–658](/F:/sync-project/kp-s11-adr/kairos/web/assets/js/core/strings-remote.js:653)
- 使用者文件：
  - [kairos/README.md:670](/F:/sync-project/kp-s11-adr/kairos/README.md:670)
  - [kairos/web/README.md:263](/F:/sync-project/kp-s11-adr/kairos/web/README.md:263)
  - [REMOTE.md:535–579](/F:/sync-project/kp-s11-adr/crates/kairos-proto/REMOTE.md:535)
  - [REMOTE.md:952](/F:/sync-project/kp-s11-adr/crates/kairos-proto/REMOTE.md:952)
  - [REMOTE.md:1076–1084](/F:/sync-project/kp-s11-adr/crates/kairos-proto/REMOTE.md:1076)
  - [API.md:1488–1510](/F:/sync-project/kp-s11-adr/kairos/API.md:1488)
  - [CHANGELOG.md:64](/F:/sync-project/kp-s11-adr/CHANGELOG.md:64)
- 工程註解／測試期望文字：
  - [schedule.rs:1–8](/F:/sync-project/kp-s11-adr/kairos/src/remote/schedule.rs:1)
  - [rollout.rs:1–2](/F:/sync-project/kp-s11-adr/kairos/src/remote/rollout.rs:1)
  - [pulse-rollout-new.js:4–11](/F:/sync-project/kp-s11-adr/kairos/web/assets/js/screens/pulse-rollout-new.js:4)
  - [e2e-remote.mjs:252](/F:/sync-project/kp-s11-adr/kairos/web/tests/e2e-remote.mjs:252)
  - [e2e-remote.mjs:275–279](/F:/sync-project/kp-s11-adr/kairos/web/tests/e2e-remote.mjs:275)
- 決策紀錄：
  - 將 [ADR U4](/F:/sync-project/kp-s11-adr/docs/adr/0002-rollout-console-design.md:490) 從待決改為已決。
  - 外部 review 建議保留為歷史紀錄，不必改寫；可在 ADR 加上採納註記。

不應修改：

- API：`schedule`、`windows`、`starts_at`、`ends_at`、`timezone`、`window`。
- API 狀態／原因：`scheduled`、`waiting_window`、`window_closed`、`rollout_ended`、`schedule_ended`。
- Kairos–Pulse 協定：`apply_window.not_before/not_after`。
- 資料庫：`rollouts.schedule`。
- 告警 API／資料庫：`kind: maintenance`、`alert_silences`、`in_maintenance`。
- CLI：告警的 `--maintenance`。
- 內部 `upgrade_window`：它表示升級期間的離線告警抑制，不是升級排程的顯示名稱。

相容性判定：若只做上述中文與文件改名，API、協定、設定檔、資料庫與舊版 Pulse 全部相容；影響僅限 0.10 尚未發佈的 UI 文案、文件與對應測試快照。這正是應在 0.10.0 發佈前完成的低風險定名窗口。