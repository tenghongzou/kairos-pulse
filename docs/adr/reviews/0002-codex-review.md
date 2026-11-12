# ADR-0002 的外部審查（codex，2026-11-12）

> 審查對象：commit 4eb7086 的版本。唯讀審查，未執行建置或測試。審查意見尚未併入 ADR。

## 1. 總評

建議：**修改後核定**。四步精靈、固定送審對象、核准者不得為建立者或送出者、修改即失效重審，方向上確實能降低大規模升級誤操作；其中多數核心狀態轉移已與現有後端一致。  
但目前提案允許核准 L2 只做前端比對，這會讓 API 直接繞過第二人的實質確認；此外，關閉雙人覆核的治理模型、待核准逾時語意、版本庫檔案遺失的實際行為都尚未閉合。ADR 也低估了後端、安全測試及設計包同步成本，所謂 0.10 最小組合並不是安全完整的垂直切片。  
本次只讀取與搜尋檔案；沒有執行 cargo、建置或測試。

## 2. 重大問題

### 2.1 核准 L2 若只有前端比對，可由 API 完全繞過

- **位置**：ADR-0002 第 2b 節、U1，特別是「後端完成前，主控台先在用戶端比對」及「時程不足時先以用戶端比對出貨」；`docs/adr/0002-rollout-console-design.md:129-137,487`
- **問題**：這不是單純的 UI 漸進增強，而是安全邊界缺失。任何有 `rollouts:approve` 的登入工作階段都可直接送空本文核准，不必輸入「核准 N 台」，因此無法宣稱核准者確實看過固定台數與風險。
- **證據**：
  - 現行 approve/reject API 只允許 `reason`，沒有 `confirm`：`kairos/src/api/remote.rs:1322-1329`
  - 引擎核准只檢查狀態與身分，沒有確認字串：`kairos/src/remote/engine.rs:628-656`
  - 相較之下，送出階段已在伺服器端計算並驗證精確確認字串：`kairos/src/remote/engine.rs:539-548`
- **建議修改**：
  - B3 必須是 U1(b) 的發佈前置條件，刪除「先以用戶端比對出貨」。
  - `approve_confirm_text` 必須由伺服器依已固定的 `rollout_hosts` 在同一交易中計算；approve 在同一交易重新計算並比對，避免 TOCTOU。
  - API 測試要直接以空字串、舊台數、前後空白、降版錯誤字串驗證 400 `confirmation_required`。

### 2.2 `required → none` 的方案不能等同 L3，也不足以保障權責分離

- **位置**：ADR-0002 第 2f 節、決策 #3、U5；`docs/adr/0002-rollout-console-design.md:168-178,342,491`
- **問題**：ADR 以「能改 `kairos.toml` 的系統權限門檻不低於 L3」為理由，允許單一主機管理者關閉雙人覆核。這是權限強度的比較，不是權責分離；一個人權限再高，仍可獨自關閉政策並在重新啟動後送出大型推送。啟動後補記稽核只能偵測，不能阻止繞過。
- **證據**：
  - 現有規格明定關閉雙人覆核是 L3：`crates/kairos-proto/REMOTE.md:659-660`
  - 設定只接受 `none|required`、預設 `none`，且須重新啟動：`kairos/src/config/remote.rs:3,38-45,47-59`
  - 送出時只看啟動時載入的設定；`none` 會直接進入 `scheduled/running`：`kairos/src/remote/engine.rs:513-514,582-593`
  - 現行健康資訊只顯示目前值，沒有前值或設定變更稽核：`kairos/src/api/health.rs:776-784`
- **建議修改**：
  - 不應宣稱設定檔修改「等同 L3」。若仍採 U5(a)，ADR 必須明定這是接受的治理缺口，並要求部署端以設定管理、PR 雙簽或 PAM 流程補足。
  - B2 應定義前值保存位置、首次啟動、DB 還原、資料遺失、設定來自環境變數時的語意；目前沒有 schema／持久化設計。
  - `required → none` 至少應產生高可見度安全事件，而不只是資訊事件；事件應在遠端推送功能開始接受請求前寫入。
  - 已存在的 `pending_approval` 保持待核准是合理的；現行 approve 只看狀態，所以設定改為 `none` 後仍可核准：`kairos/src/remote/engine.rs:628-644`。

### 2.3 版本庫檔案遺失目前不會在資格預覽變成 `no_artifact`，且現有 S25 重新匯入會跳過它

- **位置**：ADR-0002 第 5b 節及未確認事項 #2、#3；`docs/adr/0002-rollout-console-design.md:254-263,500-505`
- **問題**：ADR 把這件事主要視為顯示缺口，但目前後端仍把資料庫中 `stored=true`、磁碟上已遺失的檔案視為可提議；使用者可能在送審摘要看到主機「可升級」，核准後才在下載階段失敗。此外，ADR 所稱「現有行為」可重新匯入補檔並不正確：S25 看到 `stored` 就跳過上傳。
- **證據**：
  - `GET /releases` 的 artifact JSON 沒有 `problem`：`kairos/src/remote/ops.rs:1164-1177,1197-1225`
  - 實際檔案檢查只在健康 API 做 `stat`：`kairos/src/api/health.rs:803-829,867-879`
  - 資格分類只看 DB 的 `stored`，沒有檢查檔案是否存在：`kairos/src/remote/rollout.rs:590-599,657-667`
  - 提議建構同樣使用 DB artifact；檔案路徑直到分段下載才交出：`kairos/src/remote/engine.rs:1253-1257,1336-1344,1364-1399`
  - S25 對 `stored || registered` 的檔案直接顯示「已有」並跳過上傳：`kairos/web/assets/js/screens/pulse-releases.js:413-425`
- **建議修改**：
  - B7 不只是畫面資料來源，而應是安全修正的前置項。
  - hosted artifact 至少在預覽、submit 與 build_offer 重新檢查存在性及大小；缺失時分類為 `no_artifact`，或阻擋整份推送送審。
  - S25 只有在 `problem == null` 時才能跳過；`missing`／`size_mismatch` 必須執行 PUT 補檔。
  - 加入「預覽後、核准前檔案被刪除」的 TOCTOU 測試。

### 2.4 逾時自動退回缺少完整狀態語意與競態定義

- **位置**：ADR-0002 第 2e 節、B5、U2；`docs/adr/0002-rollout-console-design.md:156-166,401,488`
- **問題**：提案只寫「到期退回草稿」，未明定是否刪除固定主機、清除哪些欄位、何時釋放 overlap、與同時核准／修改／取消的競態、調整 timeout 是否回溯影響既有推送。若處理不完整，可能保留已過期的固定對象、錯誤佔用主機或留下一半更新的狀態。
- **現有可參考行為**：
  - 人工退回會在同一交易刪除固定主機、清除送出者與核准欄位，再回到 draft：`kairos/src/remote/engine.rs:693-704`
  - 修改待核准推送也會刪除主機並清除送審／核准欄位：`kairos/src/remote/engine.rs:458-480`
  - 待核准主機會造成其他推送 `rollout_overlap`：`kairos/src/remote/tests.rs:2157-2163`
- **建議修改**：
  - 明定逾時轉移等同系統執行的 reject：以 `submitted_at + timeout` 為基準，在單一 DB 交易檢查仍為 `pending_approval`、刪除 hosts、清除 submitted/approved、釋放 overlap。
  - 稽核應保存原 `submitted_by`、`submitted_at`、逾時設定值，另用穩定的原因碼 `approval_expired`；不要只保存會隨語系變動的中文句子。
  - 明定 timeout 改小是否立即使舊案過期，以及系統時間向前／向後跳動的處理。
  - 補上核准、退回、取消、逾時同時發生時「只有一個狀態轉移成功」的測試。

### 2.5 實作計畫的「最小組合」不是安全完整切片，估工也有算術與範圍問題

- **位置**：ADR-0002 第 7.2 節、U8；`docs/adr/0002-rollout-console-design.md:417-430,494`
- **問題**：
  - 所謂最小組合允許 P4 只有前端，會留下 2.1 所述的核准繞過。
  - P2 自稱包含樂觀鎖，但依賴 B6；不能把後端拿掉仍稱完整草稿流程。
  - 以表內數字計算，P0 1.5 天＋P1 2.5 天＋P2 7 天＋P4 前端 3 天，已是 14 人天；若包含 P2 必要的 B6 後端則至少 15 人天，並非約 2.5 人週。
  - P6 僅 2 人天，卻要完成 14 組 Playwright/axe、6 組 RBAC、後端單元測試及多份文件同步，風險明顯偏高。
- **建議修改**：
  - 以垂直切片重排：先完成 B3、身分差異、待核准檢視、送審摘要及安全測試，再出貨 L2 核准。
  - 草稿編輯必須連同 B6 及衝突測試一起交付。
  - 把設計包定案、review、返工、資料庫遷移及測試除錯獨立列工，不要只用 27.5 人天除以 5 得出 5.5 人週。
  - U8 不應採目前描述的方案 (b)。

## 3. 次要問題與不一致

- **建立者與送出者的邊界沒有說清楚**：現行實作同時禁止兩者核准：`kairos/src/remote/engine.rs:632-640`。因此若甲建立、乙修改並送出，必須由第三人核准；不是文件多處所暗示的「另一位管理員」即可。建議把這個三人邊界列為明確決策與測試案例。

- **`GET /api/v1/settings` 的未確認事項已可確認**：它會透過 `show_effective` 展開整個 `AppConfig`，不是只回傳可修改的九個鍵；`remote.*` 已包含在結果中，只是 `console_editable=false`：`kairos/src/settings.rs:202-217,293-330`、`kairos/src/api/enroll.rs:768-788`。因此 B1 不需新增「回傳 remote.*」，只需 S21 分組與呈現。

- **樂觀鎖目前確實不存在**：`RolloutSpec::parse` 不接受 `expected_updated_at`：`kairos/src/remote/rollout.rs:438-452`；PUT 也直接呼叫 update：`kairos/src/api/remote.rs:1252-1260`。新增時應決定欄位放在本文還是使用 HTTP `If-Match`；後者較符合既有資源更新語意。

- **REMOTE.md 自身有矛盾**：
  - 概念規格只寫建立者不能核准：`crates/kairos-proto/REMOTE.md:659,883,903`
  - 實作附註改為建立者或送出者：`crates/kairos-proto/REMOTE.md:1085,1125`
  - 同一段又說 `rollout_approval=none` 時 approve/reject 為 invalid_state，但同時說既有 pending 仍須核准或退回：`crates/kairos-proto/REMOTE.md:1085`。實作實際上是只看 rollout state，較符合後一句。

- **關閉雙人覆核的決策與 REMOTE.md 衝突**：REMOTE.md 把它列為 L3：`crates/kairos-proto/REMOTE.md:660`；ADR-0002 改為不另設 L3。核定後必須正式修訂規格，不應只修改設計包文案。

- **README 所稱 ADR-0002 與另外兩份「獨立」不準確**：`docs/adr/README.md:24`。ADR-0001 會新增 `slot` 安裝方式並改變「無法回滾」判斷與主控台提示：`docs/adr/0001-windows-ota-upgrade.md:291,317-330`；ADR-0003 會改變 deb/rpm 回滾檔資格與版本庫需求：`docs/adr/0003-rollback-bundle-trust.md:275,403-409`。S25、資格預覽與送審摘要都會受影響。

- **`KRS-E4406` 不宜綁在本 ADR 核定**：它屬 PostgreSQL 另一開發線，repo 內找不到實作依據；ADR 自己也承認無法確認：`docs/adr/0002-rollout-console-design.md:316-328,507`。應拆成獨立決策或等待該線合併後核對。

- **`pending_approval_actionable` 應由後端計算**：前端以 actor 字串過濾雖可改善泡泡，但容易與身分格式或未來 service account 規則分歧。後端已掌握建立者、送出者與 principal，應回傳每位使用者可操作的數量。

- **逾時設定的命名符合現有 snake_case 規則，但缺常數與驗證位置**：repo 對範圍通常以常數與 `validate()` 實作，例如 `MAX_ROLLOUT_RETENTION_DAYS`：`kairos/src/config/remote.rs:15-16,84-87`。新設定應沿用同一模式。

## 4. 已核對且確認正確的關鍵事實主張

- 建立推送目前確實是單一長表單，檔案共 780 行；草稿只在最後送出時 POST，之後立即 submit：`kairos/web/assets/js/screens/pulse-rollout-new.js:627-670`
- 預覽使用 600 ms debounce：`kairos/web/assets/js/screens/pulse-rollout-new.js:583-600`
- PUT 允許 `draft`、`scheduled`、`pending_approval`；後兩者修改會回 draft、刪除固定主機並清除送審／核准欄位：`kairos/src/remote/engine.rs:447-480`
- 建立者與送出者都不能核准：`kairos/src/remote/engine.rs:628-640`
- approve/reject 必須是帳號登入工作階段，API token 不可使用：`kairos/src/api/remote.rs:1310-1319`
- 人工退回會刪除固定對象、保存原因，重新送出會清除原因：`kairos/src/remote/engine.rs:693-710,577-581`
- `ends_at` 已過時，送出及核准都會被 `schedule_ended` 阻擋：`kairos/src/remote/engine.rs:520-523,642-645`
- 待核准主機確實會阻擋重疊推送：`kairos/src/remote/tests.rs:2157-2163`
- `rollout_approval` 預設為 `none`，只接受 `none|required`，需重啟才生效：`kairos/src/config/remote.rs:3,43-59,89-96`
- SideNav 目前把所有 pending 都算進泡泡，包括自己不能核准的項目：`kairos/web/assets/js/screens/pulse-boot.js:32`、`kairos/src/api/remote.rs:1198-1219`
- 目前送出者／建立者看不到 approve，但仍看得到 reject：`kairos/web/assets/js/screens/pulse-rollout.js:225-249`
- 批次最多 10 批：`kairos/src/remote/rollout.rs:135-138,481-492`
- 時段最多 16 個，空 days 代表每天，開始等於結束會拒絕：`kairos/src/remote/schedule.rs:16-17,308-327`
- 現況時區只接受固定位移，不接受 IANA 名稱：`kairos/src/remote/schedule.rs:64-81,154-161`
- 版本庫檔案遺失健康檢查只做 metadata／大小檢查，不重算雜湊：`kairos/src/remote/releases.rs:248-255`
- S25 的 release API 目前沒有逐檔 `problem`：`kairos/src/remote/ops.rs:1164-1177`
- 全域搜尋確實已有主機、告警事件、升級推送、設定檔四種 provider：`kairos/web/assets/js/ui/search.js:18-38`、`kairos/web/assets/js/screens/alerts-boot.js:69`、`kairos/web/assets/js/screens/pulse-boot.js:66-80`
- 搜尋提示仍只描述主機：`kairos/web/assets/js/core/strings.js:84-85,510-511`
- `KRS-W8417` 已在 Rust 定義但仍標為暫定，`error-codes.js` 沒有條目：`kairos/src/remote/mod.rs:78-79`

## 5. 不正確或無法找到依據的主張

- **「`GET /api/v1/settings` 是否回傳 `remote.*` 尚未確認」**：已確認會回傳；B1 的後端新增需求不成立。證據見第 3 節。
- **「S25 重新匯入同一版本可直接補上遺失檔案（現有行為）」**：不完整。雖然 UI 能在 `release_exists` 後載入既有版本，但 `stored=true` 時會直接跳過該檔，即使實體檔已遺失：`kairos/web/assets/js/screens/pulse-releases.js:421-425,494-507`
- **「檔案遺失的主機可能歸入 `no_artifact`」**：目前不會；資格分類只看 DB `stored`：`kairos/src/remote/rollout.rs:590-599,666-667`
- **「關閉設定檔中的雙人覆核門檻不低於 L3，因此不需要 L3」**：這是治理假設，不是能由程式碼證明的事實，而且把高權限與多人分權混為一談。
- **「最小組合約 2.5 人週」**：與表內人天加總不符，且未計必要的 B3／B6。
- **`KRS-E4406` 的使用範圍、結束代碼與號碼空缺**：目前 repo 內沒有足夠依據查證。
- **設計包是否已有通用步驟列元件、copywriting 編號是否空缺**：設計包位於 workspace 外，本次依 repo 唯讀範圍未查證；不能據此核定其號碼或元件規格。
- **IANA 時區的 DST 重複／不存在時間語意**：現有程式只支援固定位移，repo 內沒有可核對的實作。

## 6. 遺漏的方案、風險或驗證項目

- 送審摘要應有伺服器產生的 revision／digest；核准應對該 revision 操作，避免 UI 摘要與固定資料不一致。
- 明定「甲建立、乙送出」是否刻意要求丙核准；這會直接影響只有兩位管理員的部署。
- 沒有任何可核准者時的偵測，例如只有一位管理員、另一位帳號停用、權限在待核准期間被撤銷。
- 同時 approve、reject、update、cancel、timeout 的競態與冪等性測試。
- 推送送審後 release 被刪除、檔案遺失或大小改變的 TOCTOU。
- 逾時設定變更、系統時間跳動、重新啟動後補跑逾時的行為。
- 設定變更稽核的前值要放哪裡、DB 還原或首次升級時如何避免誤報。
- 關閉雙人覆核事件是否需要外部通知；若只留在同一台可能已受控制的 Kairos，稽核價值有限。
- 待核准通知只靠泡泡可能導致七天後自動退回；大型組織通常至少需要可選通知管道。
- ADR-0003 導入 `rollback_bundle` 或本機封存後，S25 的「N 台回滾需要」與 `no_rollback` 文案必須重新定義。
- ADR-0001 的 `slot` 主機不需要 rollback artifact；資格摘要、篩選及說明不能沿用 deb/rpm 文案。
- IANA 時區必須先定義 tzdb 來源與版本；瀏覽器 `Intl` 與伺服器可接受名稱可能不同。
- 大量主機下送審摘要的效能與可驗證性：顯示前 100 台可以，但必須提供完整總數、分類及可匯出清單。
- 稽核應測試 hash chain、actor=`system`、原始送審資訊保存，而不只測有無事件。

## 7. 對「待使用者決定」各項的意見

- **U1 核准確認等級**：同意 **(b)**，但 B3 伺服器驗證是硬性前置；不同意前端-only 出貨。
- **U2 待核准逾時**：原則同意 **(b)**。需先補齊第 2.4 節的交易、競態及設定變更語意。
- **U3 通知**：不同意只把系統規則無條件延後。至少應在 0.10 提供泡泡、複製連結及可選的既有通知事件；否則逾時功能很容易只造成反覆退回。
- **U4 名稱**：同意 **(b)「升級時段」**，能避免和告警靜音的「維護時段」混淆。
- **U5 主控台修改**：可以暫採 **(a)**，但不同意「設定檔權限等同 L3」的理由。必須明列治理缺口、持久化前值、稽核／通知及部署端雙人變更流程。
- **U6 精靈步數**：同意 **(a) 四步**；版本與對象共同決定資格，合併合理。
- **U7 捨棄草稿**：同意 **(b)**。保留 cancelled 記錄而不真刪除，較符合稽核需求。
- **U8 0.10 範圍**：不同意目前的 **(b)**。應改成安全垂直切片；核准 L2 必須帶 B3，草稿修改必須帶 B6。完整方案需重新估工。
- **U9 送出者是否看到退回**：同意 **(b)**；送出者使用「修改」或「取消」較清楚，後端保留 reject 能力亦可。
- **U10 代碼**：`KRS-W8417` 同意沿用；`KRS-E4406` 不建議在本 ADR 核定，應等 PostgreSQL 開發線有可核對實作後另行決定。