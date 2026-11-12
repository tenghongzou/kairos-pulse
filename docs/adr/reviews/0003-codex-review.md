# ADR-0003 的外部審查（codex，2026-11-12）

> 審查對象：commit 4eb7086 的版本。唯讀審查，未執行建置或測試。審查意見尚未併入 ADR。

## 1. 總評

建議：**修改後核定**。ADR-0003 找到的 `from_version`、接續時重新信任 `request.json`、`method` 由低權限端決定，以及套件回滾失敗沒有 `pulse.prev` 退路，均確實存在；核心方向「特權元件自行綁定替換前版本」也是正確的。不過目前的 root `state.json` 修法沒有涵蓋狀態寫入失敗、狀態損毀與替換／狀態更新之間的斷電窗口，11 條不變條件中也有無法照字面實作的條文。尤其「顯式移除金鑰後仍執行封存套件」與「最壞訊息約 62～63 KiB」的論證不足，不宜原文核定。

## 2. 重大問題

### 2.1 root `state.json` 修法不足以安全接續

- **位置**：ADR-0003 §5.1、§9.1 0-1、INV-1／INV-6。
- **問題**：把 `from_version`、`method` 等存入 root 狀態是必要的，但現行狀態機的寫入失敗會被忽略，讀取失敗也被當成「沒有狀態」重新開始。更危險的是，替換完成到寫入 `Step::Replaced` 之間若當機，磁碟上的狀態仍是 `Verified`；接續時會再次 `prepare()`／`replace()`，可能以新版覆寫原本的 `pulse.prev`。
- **證據**：
  - `save()` 只記 `WARN`，失敗不停止流程：`pulse/src/remote/upgrade/helper.rs:362-369`。
  - `state.json` 不存在、無法解析或 schema 不符都變成 `None`：`helper.rs:324-329`。
  - `Step::Verified` 沒有專門復原，直接重新預檢與替換：`helper.rs:372-401`。
  - `pulse.prev` 在每次 `replace()` 一開始即覆寫：`helper.rs:688-697`。
  - 狀態先存為 `Verified`，真正替換後才存為 `Replaced`：`helper.rs:702-714`、`716-753`。
  - `write_atomic` 沒有在 rename 後 fsync 目錄，不能宣稱斷電耐久：`pulse/src/remote/mod.rs:40-68`。
- **建議修改**：
  1. `state.json` 首次持久化失敗必須 fail closed，絕不能開始替換。
  2. 明定並測試 `prepared → replacing → replaced` 的交易式狀態；每一步要可判斷磁碟實況並冪等接續。
  3. 狀態損毀或缺失但已看到 `pulse.prev`／套件版本改變時，不得當成全新請求。
  4. root 狀態至少綁定 `upgrade_id`、原版本與 commit、目標版本與 commit、安裝方法、已驗證 artifact／rollback package 的 SHA-256、是否已建立 `pulse.prev`、替換前後套件資料庫狀態。
  5. 接續交易應由 root 的 active-transaction 指標找回，不應先以可改寫的 `request.json.upgrade_id` 決定要讀哪個 root 工作目錄。

### 2.2 「已綁定目標只檢查完整性」沒有區分自然過期與顯式撤銷

- **位置**：ADR-0003 §5.3、INV-7、P2。
- **問題**：對 `valid-before` 自然到期仍允許回滾，安全取捨大致成立；但「金鑰被顯式移除」通常代表外洩、誤簽或簽署者不再可信，不能與自然過期視為同一事件。回滾套件不只恢復同一份 `pulse` 執行檔，還會以 root 再執行維護指令碼並重寫套件其他內容，因此退回 `pulse.prev` 與重裝封存套件並非安全等價。
- **證據**：
  - ADR 自己承認 deb/rpm 回滾會執行舊版維護指令碼：`docs/adr/0003-rollback-bundle-trust.md:69-70`、`:246`。
  - INV-2 只綁執行檔與整包雜湊，沒有證明維護指令碼重新執行在不同時間與系統狀態下無害：`0003:219`。
  - 套件回滾確實呼叫 `dpkg`／`rpm`：`helper.rs:871-899`。
  - 現行規格明定移除舊金鑰後拒絕舊金鑰簽署的發佈：`crates/kairos-proto/REMOTE.md:407-409`。
  - 金鑰外洩流程也要求移除並以新金鑰重簽：`RELEASING.md:118-121`。
- **建議修改**：拆成兩種政策：
  - `valid-before` 自然到期：完整性通過即可回滾。
  - 金鑰顯式移除／列入撤銷清單：Linux 封存套件不得自動重跑維護指令碼，改退 `pulse.prev` 並標記套件資料庫不一致；若要強制使用，需本機管理者明確核准。
  - Windows 前一槽可以採完整性優先，但仍要記錄「以已撤銷簽署者建立」的高嚴重度狀態。
  
  若堅持 P2(c)，ADR 必須把這列為明確接受的安全風險，而不能稱兩條路徑「安全性沒有差異」。

### 2.3 INV-11 按字面無法實作，且與正常升級流程矛盾

- **位置**：ADR-0003 INV-11。
- **問題**：「低權限元件提供的任何欄位只能讓特權元件少做，不能讓它多做或做不同的事」過度寬泛。正常流程本來就由 `request.json` 的目標與 artifact 觸發 root 安裝不同版本；`health_timeout_secs`、`allow_downgrade` 等欄位也會改變 root 行為。若照字面實作，遠端升級本身即違反 INV-11。
- **證據**：
  - `Request` 包含 target、method、artifact、健康期限與降版允許：`pulse/src/remote/upgrade/files.rs:39-60`。
  - 輔助程式依 target 驗證及安裝：`helper.rs:513-553`、`716-729`。
  - 健康期限也取自請求：`helper.rs:671-675`。
- **建議修改**：改成「低權限輸入不得直接決定**信任結論、既有交易的接續身分、回滾目標或特權執行路徑**；所有會增加權限效果的輸入必須由特權元件依本機政策與已驗證發佈資料重新推導」。並逐欄建立來源／重新驗證／接續時是否可變的表格。

### 2.4 單行大小的 62～63 KiB 論證不正確

- **位置**：ADR-0003 §6.3。
- **問題**：估算只把 `manifest.content` 的 JSON 跳脫放大計入，卻沒有對兩份 `sha256sums`、兩份 signature、URL 等字串採一致的最壞 JSON 編碼。現有驗證只限制原始 UTF-8 bytes，不限制序列化後 bytes；因此「所有欄位取上限仍低於 65536」不能成立。單純省略 `rollback_bundle` 也未必足夠，因為沒有 bundle 的基礎訊息本身也可能因跳脫超限。
- **證據**：
  - 上限為 65536 bytes：`crates/kairos-proto/src/lib.rs:113-114`。
  - `sha256sums` 16 KiB、signature 4 KiB、manifest 8 KiB、URL 2 KiB：`crates/kairos-proto/src/mgmt/mod.rs:661-670`。
  - 驗證只檢查原字串長度：`mgmt/mod.rs:813-839`、`848-892`。
  - Pulse 確實以 65536 建立 `LinesCodec`，超過即連線錯誤：`pulse/src/network.rs:447-450`、`540-545`。
  - Kairos 目前只呼叫欄位驗證，沒有檢查序列化 wire length：`kairos/src/remote/engine.rs:1347-1353`。
- **建議修改**：
  1. 刪除 62～63 KiB 的結論，改成「未實際序列化前無法證明」。
  2. 以完整 `ServerMsg` 實際序列化後的 byte length 為唯一判斷，並預留換行／封裝餘裕。
  3. 先嘗試含 bundle；超限則省略 bundle；仍超限則不得送出，主機應得到明確的資格／內部錯誤，而不是斷線。
  4. 測試必須包含需要大量 JSON 跳脫的合法與惡意輸入，而不只是一般 `SHA256SUMS` 範例。

### 2.5 內容綁定不能支持「Kairos 只能決定附不附」

- **位置**：ADR-0003 §0 #3、T1、方案 A2。
- **問題**：比較套件內 `/opt/pulse/pulse` 與 `pulse.prev` 只能固定代理程式執行檔；Kairos 仍可在多個合法簽署、相同版本且執行檔相同的重打包套件中，影響維護指令碼、unit 或其他檔案。ADR 有提到這些內容靠簽章，但這表示 Kairos 的能力不只是「附不附」，而是可選擇哪一個官方簽署的封裝。
- **證據**：
  - 回滾驗證刻意不檢查 commit：`helper.rs:570-578`。
  - `check_artifact` 只有 expectation 提供 commit 時才檢查：`crates/kairos-proto/src/release/mod.rs:687-698`、`:734-750`。
  - Hello 的 `agent_version` 只有版本，沒有 commit；程式註解明示：`pulse/src/main.rs:659`、`pulse/src/network.rs:1529`。
  - A2 自己承認執行檔以外仍靠簽章：`docs/adr/0003-rollback-bundle-trust.md:151-156`。
- **建議修改**：弱化 T1 與結論文字，明列「bundle 會把信任範圍由本機曾安裝的發佈擴大到任何同版本、同平台、官方簽署且執行檔相同的封裝」。若不能可靠取得現行套件的完整 artifact hash／commit，應把這項殘餘風險列入並禁止同版本重新出包；長期可在安裝時保存 artifact hash 作為完整包綁定。

### 2.6 套件回滾失敗後只換執行檔，不能一律回報普通 `rolled_back`

- **位置**：ADR-0003 §9.1 0-3、INV-5。
- **問題**：`dpkg`／`rpm` 失敗可能已部分執行維護指令碼或更新部分檔案。此時只恢復 `pulse.prev` 雖比完全停止好，但不能保證 unit、套件內容與資料庫一致，也不能將它等同完整回滾成功。
- **證據**：
  - 現況確實在 package command 失敗時直接 `rollback_failed`：`helper.rs:898-933`。
  - `pulse.prev` 路徑只替換執行檔；unit 還原只針對 tar 的 `unit_replaced`：`helper.rs:900-928`。
- **建議修改**：可以嘗試 `pulse.prev` 作為緊急復原，但結果至少要是 `rolled_back_degraded` 或 `rolled_back` 加明確的機器可讀 degraded flag，並觸發告警與人工修復要求；測試需涵蓋 package transaction 部分完成的情況。

## 3. 次要問題與不一致

1. **ADR-0001 的掃描退路仍違反 INV-1／INV-10**  
   ADR-0001 在指標損毀時選「版本最高且 confirmed」的槽：`docs/adr/0001-windows-ota-upgrade.md:216-220`。ADR-0003 已正確指出應依最近確認時間而不是版本：`0003:328-334`。在 ADR-0001 修訂前，不能宣稱 Q1～Q6 已經完整整合。

2. **ADR-0001 與 ADR-0003 對 Linux 紀錄的信任等級不一致**  
   ADR-0001 把 Windows `release\` 與 Linux `upgrade/releases/` 稱為同一種已驗證本機紀錄：`0001:334-338`；後者其實由 `pulse` 帳號可寫，只是提示。ADR-0003 的修正方向正確，但 README 也明載尚未併入：`docs/adr/README.md:21-23`。

3. **REMOTE.md 需要修訂的章節列得不完整**  
   ADR 首頁只列 2.4.1、4.4.2、4.5.3、5.1、17.3、17.5：`0003:8-10`。若採 INV-7 與套件失敗退路，還必須修改：
   - 金鑰移除使舊發佈全部失效：`REMOTE.md:407-409`。
   - 回滾失敗後「不再嘗試任何動作」：`REMOTE.md:484-488`。
   - 自動回滾與遠端降版的定義：`REMOTE.md:585-591`。
   - 「任何驗證失敗都 failed」與回滾檔驗證失敗改走 fallback 的適用範圍：`REMOTE.md:397-405`。

4. **REMOTE.md 的無回滾檔行為確實與程式不一致**  
   規格說不回復：`REMOTE.md:490-492`；程式會換回 `pulse.prev`：`helper.rs:900-928`。ADR 對這點的判斷正確。

5. **「立即修正、不待核定」的治理範圍過大**  
   `from_version` 信任缺口適合直接安全修補；但「套件失敗後改回 `pulse.prev` 並改終止狀態」是對外行為及告警語意變更，不宜僅以 README 的「不待核定」帶過：`README.md:21-24`、`0003:373-388`。

6. **ADR-0002 沒有實質技術衝突**  
   ADR-0002 明確限於 Web 主控台、不含 Pulse 端：`docs/adr/0002-rollout-console-design.md:3-8`。它對降版的 L3 文案不要求重新驗證密碼：`0002:19-25`；這不影響 ADR-0003 的回滾信任判斷，但未來若新增「遠端切前一槽」，需同步定義確認與稽核 UI。

7. **協定新增欄位本身向後相容，但「不需要 feature」需加條件**  
   `UpgradeOffer` 確實沒有 `deny_unknown_fields`：`mgmt/mod.rs:777-801`；共通規則也明載忽略未知欄位：`crates/kairos-proto/src/messages.rs:1-8`。因此選用欄位可被舊 Pulse 忽略，但前提是序列化後仍不超過舊端同樣的單行上限。

8. **INV-3 與 INV-8 的例外關係不清楚**  
   INV-3 要求所有特權執行的非 binary 內容必須由本機信任錨驗證：`0003:220`；INV-8 又說本機管理者安裝的版本信任根不是簽章：`0003:225`。應明定 INV-8 是 INV-3 的本機管理者例外，以及種子套件的維護指令碼如何處理。

9. **INV-5 的「更保守」沒有可操作定義**  
   Windows 的 `current.json.bak → 掃描 → 重建 seed` 不一定單調更保守；掃描可能選到管理者剛退掉或不該再次啟動的槽。應定義候選資格與排序，而非只用抽象詞。

## 4. 已核對且確認正確的關鍵事實主張

- 輔助程式以 `req.from_version` 驗證回滾 artifact：`pulse/src/remote/upgrade/helper.rs:555-580`。
- `Request::validate` 沒有把 `from_version` 綁到 helper 自身版本：`pulse/src/remote/upgrade/files.rs:88-115`。
- 目標版本政策使用 helper 的 `current_version`：`helper.rs:534-548`；正式值來自 `CARGO_PKG_VERSION`：`helper.rs:165-180`。
- 相同版本被視為 reinstall，不受降版開關限制：`crates/kairos-proto/src/release/mod.rs:788-807`。
- `request.json` 每次執行都重新讀取，接續時現況仍持有新的 `req`：`helper.rs:302-337`、`:372-396`。
- `method` 直接決定解包、安裝及回滾路徑：`helper.rs:513-522`、`:607-620`、`:716-729`、`:871-899`。
- 實際已有獨立的安裝方式偵測邏輯可重用：`pulse/src/remote/install.rs:104-132`。
- 套件管理員回滾失敗目前直接成為 `rollback_failed`：`helper.rs:930-946`。
- 回滾確認使用 `req.from_version`：`helper.rs:949-973`。
- 本機 release 紀錄由服務帳號寫入，helper 會重新驗證：`pulse/src/remote/upgrade/mod.rs:1642-1681`、`helper.rs:555-587`。
- 現況只在本機有目前版本紀錄時下載回滾檔：`upgrade/mod.rs:1241-1269`、`:1305-1340`。
- Kairos 現在只對 deb/rpm 尋找目前版本的 rollback artifact，沒有附該版本的簽章 bundle：`kairos/src/remote/engine.rs:1261-1275`、`:1302-1335`。
- `UpgradeOffer` 未使用 `deny_unknown_fields`，未知欄位按規則忽略：`mgmt/mod.rs:777-801`、`messages.rs:1-8`。
- 線路單行上限是 65536 bytes，Pulse 超限會當作 codec／連線錯誤：`kairos-proto/src/lib.rs:113-114`、`pulse/src/network.rs:447-450`、`:540-545`。
- Kairos 目前只做欄位驗證，沒有總序列化長度檢查：`kairos/src/remote/engine.rs:1347-1353`、`mgmt/mod.rs:848-892`。
- 既有版本匯入會回 `release_exists`；CLI 對相同 `SHA256SUMS` 只走補 artifact 流程：`kairos/src/remote/ops.rs:897-907`、`kairos/src/releases_cli.rs:504-544`。
- `pulse-install.sh` 的確驗證簽章與 artifact hash，但目前沒有保存紀錄：`pulse/packaging/install.sh:147-171`、`:173-180`。
- 現行 REMOTE 規格對 `pulse` 帳號受降版政策限制的宣稱不完全成立：`REMOTE.md:610-620`。

## 5. 不正確或無法在程式碼中找到依據的主張

1. **「最壞約 62～63 KiB，仍在上限內」不正確**  
   未完整計算 JSON 跳脫與完整訊息封裝，不能由現有常數推出。

2. **「INV-2 後 Kairos 只能決定要不要附資料」過度陳述**  
   Kairos 仍可能影響同版本官方封裝中的維護指令碼、unit 與其他檔案；只是不再能改變最終代理程式 binary。

3. **「封存套件與 `pulse.prev` 安全性沒有差異」不正確**  
   封存套件會重跑 root 維護指令碼並重寫其他套件內容，`pulse.prev` 不會。

4. **「root state.json 即足以解決接續信任」沒有程式依據**  
   現行 `save()` 失敗不阻擋、state 損毀會重新開始，且有替換後尚未存 `Replaced` 的窗口。

5. **「T3 非特權使用者能力為無」證據不足**  
   目錄權限與 `/opt/pulse` 所有權可支持一般使用者不能直接改檔，但 ADR 沒有完整核對 root 工作目錄既存時的擁有者／mode。`create_dir_all_mode` 對已存在的目錄直接接受，不重新檢查：`pulse/src/remote/mod.rs:71-91`。應改成「未發現直接路徑」，而不是絕對的「無」。

6. **Pulse 0.9.0 tag 與 HEAD 完全相同**  
   在本次限制「只讀檔與搜尋、不執行 git 比對」下，我無法獨立重核 tag 差異；只能確認目前工作樹存在缺陷。ADR 應保留其 git 查證紀錄，但本報告不把它列為已獨立確認。

7. **0.8.0 是可實際利用的降版目標**  
   ADR 自己也標示未確認；目前沒有足夠依據證明其檔名、簽章資料及套件一定能通過 0.9.0 helper。

## 6. 遺漏的方案、風險或驗證項目

- 建立 root 端交易紀錄的正式 schema、遷移規則、checksum、目錄 fsync 與損毀處理，而不只是加四個欄位。
- 對每個 crash point 做故障注入：首次 state save 前後、`pulse.prev` 建立後、package manager 執行中、package 成功但 `Replaced` 尚未寫入、回滾成功但 `RollingBack` 尚未寫入。
- 檢查 root 工作目錄及所有路徑元件的 owner、mode、mount／symlink 條件；不能只依賴 systemd 首次建立。
- 防範 `pulse` 帳號以大量 UUID／`rollback_failed` 交易耗盡 `/var/lib/pulse-upgrade`。
- 保存「目前實際安裝 artifact」的完整 SHA-256 與 commit；否則第一次 bundle 回滾只能做到 binary 綁定。
- 明定同版本重新出包政策，最好禁止相同版本對應不同 `SHA256SUMS`／封裝內容。
- 金鑰撤銷需有獨立於 `valid-before` 的語意；必要時保存撤銷原因與時間。
- 套件回滾失敗後的 degraded 狀態、告警、修復指令與套件資料庫 reconcile 流程。
- `rollback_bundle` 過長時的主機資格／觀測性：省略原因應可在主控台看到，而不只記 Kairos `WARN`。
- 實際序列化測試應涵蓋舊 Pulse 型別、完整 `ServerMsg`、最大跳脫字串、含／不含 bundle 的兩段式 fallback。
- Windows 手動切回 `previous` 建議也走 trial＋健康確認；否則 Q6 只做完整性檢查，仍可能把主機留在無法啟動的舊槽。
- INV-10 必須有原子、不可歧義的確認紀錄；`confirmed: true` 與 current pointer 分開寫入時要定義順序與復原。

## 7. 對「待使用者決定」各項的意見

| 項目 | 意見 |
|---|---|
| **P1 發佈方式** | **不同意只併 0.10.0**。建議 0.9.1 與 0.10.0 都修；若 0.10.0 能立即普及，至少仍應發布安全公告。舊版升到修正版的那一次仍由舊 helper 執行，ADR 對此判斷正確，但這不是延後修補舊支線的理由。 |
| **P2 過期／移除後能否使用** | **不同意統一採 (c)**。自然過期採 (c)；顯式移除採 Linux 的 (b) 或更嚴格撤銷政策。Windows 前一槽可採 (c)，但需高嚴重度記錄。 |
| **P3 封存幾版** | **同意 Linux 先保留目前版本一份、Windows active＋previous**。前提是升級交易期間不可提前清除回滾目標，且清理必須原子、可恢復。 |
| **P4 回滾下限** | **同意不另設**。正確落實 INV-1 後，自動回滾只能回替換前版本；版本下限應留給遠端降版。 |
| **P5 安裝時寫種子** | **同意**。但階段 1 的服務帳號紀錄只能稱提示；真正可靠的種子必須保存完整 artifact、hash、來源與確認狀態於 root 專屬位置。 |
| **P6 本機紀錄優先** | **同意，但要改稱提示來源優先順序**。兩者都必須由 root 以相同規則重驗；root archive 應永遠排第一。 |
| **P7 階段 2 時程** | **不同意把 Linux 封存綁死於 Windows 時程**。可以共用 versioned record schema 與測試案例，但 Windows 實機驗證可能拖延，不應因此延後 Linux 的可靠回滾。 |
| **P8 版本庫重簽** | **同意，但建議保留多份簽章而非覆寫唯一簽章**。必須驗證 `SHA256SUMS` 完全相同、記錄舊新 signer、時間、操作者與稽核事件，並保留歷史以供鑑識。 |
| **P9 hello 宣告 rollback** | **同意**。選用欄位對舊 Kairos 可向後相容，且能改善資格預覽；值應區分 `archived`、`binary_only`、`none`、`degraded`，避免只用布林值。 |

### ADR-0001 Q1～Q6

| 問題 | 意見 |
|---|---|
| **Q1** | **部分同意**：啟動器不內建 SSHSIG 驗證是合理的；自然過期不阻擋。顯式撤銷仍需由可更新的 helper 記錄並影響後續升級／管理提示。 |
| **Q2** | **同意，但補條件**：種子槽必須是本機管理者建立、曾成功健康執行、metadata 位於特權目錄且 hash 相符；不能只因「目前存在」就合法。 |
| **Q3** | **同意統一內容格式、不統一信任位置**。artifact SHA-256 應是主要不可變識別，版本與 commit 是索引與可讀資訊；另需 schema 版本、驗證政策快照及撤銷狀態。 |
| **Q4** | **同意只對 deb/rpm 送**。槽與 tar 不需要 bundle。 |
| **Q5** | **同意第一版不提供**。日後若提供，必須是獨立、可稽核的遠端降版動作，不能藏在一般 offer 或健康狀態中。 |
| **Q6** | **大致同意**。保留到下一次成功提交合理；本機管理者不受遠端降版政策限制，但手動切槽仍建議走 trial／健康確認，並原子更新 active／previous，避免損毀後掃描又選回被退掉的槽。 |