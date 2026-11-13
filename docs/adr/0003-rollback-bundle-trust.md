# ADR-0003：遠端升級的回滾檔（`rollback_bundle`）如何被信任

| 項目 | 內容 |
|---|---|
| 狀態 | 已核定（2026-11-13，使用者核定；依 2026-11-12 外部審查修訂後的版本。待決事項除另有註明外，皆採文內建議）；修訂紀錄見第 14 節 |
| 日期 | 2026-11-12 |
| 決策者 | 使用者（2026-11-13 核定；P1、P13 先前已個別決定，見第 11 節） |
| 處理的待決 | [`REMOTE.md`](../../crates/kairos-proto/REMOTE.md) 第 17.5 節第 1 項（回滾檔的信任）、第 3 項（套件資料庫與執行檔不一致）；第 17.3 節第 1 項（與規格的差異） |
| 取代／修訂 | 核定後修訂 `REMOTE.md` 的章節見第 6.5 節 |
| 相關 | [ADR-0001](0001-windows-ota-upgrade.md)（Windows 的 OTA 式版本槽，第 4.11 節向本 ADR 提出 Q1～Q6）；[ADR-0002](0002-rollout-console-design.md)（主控台）；外部審查 [`reviews/0003-codex-review.md`](reviews/0003-codex-review.md)、[`reviews/0001-codex-review.md`](reviews/0001-codex-review.md)；`RELEASING.md` 第 3 節 |

**標記方式**：**〔程式確認〕**＝讀程式碼確認（附檔案與行號）；**〔文件〕**＝只見於規格、CHANGELOG 或測試說明；**〔推論〕**＝依程式與文件推導，沒有實測。本 ADR 的撰寫與修訂只讀程式、文件與 git 紀錄，沒有建置、執行或實測。

**程式位置**：沒有特別註明時，路徑與行號指本工作樹（`kp-s11-adr`，commit 4eb7086 時點，與 `pulse-v0.9.0` 的輔助程式相同）；標為「0.10.0 整合分支」者指 `kp-s10-int` 的 commit f08a3da 之後。

---

## 0. 結論先行

1. **輔助程式信任 `request.json` 的缺陷已在 0.10.0 整合分支修正一大部分**（63cd981、a11d449、f08a3da），但**仍缺 root 端完整的交易紀錄**：接續時仍以 `request.json` 的 `upgrade_id` 找工作目錄、`rename` 後沒有 fsync 目錄、交易紀錄沒有綁定發佈檔雜湊與套件資料庫狀態、沒有逐一當機點的故障注入。依使用者決定，**只在 0.10.0 修正、不出 0.9.1、不發安全公告**（第 10、11 節）。
2. **採用 17.5 原建議的 `rollback_bundle`**：`upgrade_offer` 新增選用欄位，附上回滾檔所屬發佈的 `SHA256SUMS`／`.sig`。Pulse 端的本機紀錄與 bundle 都只是「提示」，由 root 輔助程式在**替換前**以本機信任錨完整驗證。協定不升版；只要**實際序列化後**仍在單行上限內，就不需要新的 feature（第 6 節）。
3. **內容綁定**：回滾套件中的 `/opt/pulse/pulse` 必須與替換前實際安裝的執行檔（`pulse.prev`）相同，套件的 commit 必須等於執行中版本的 commit。這固定了「回到哪一個代理程式」，但**沒有**固定套件的其他內容：bundle 把信任範圍從「本機曾安裝過的那一份發佈」擴大到「同版本、同平台、官方簽署、執行檔相同的任何封裝」。這是明列的殘餘風險，以「同一版本只能有一份發佈」的發佈政策收斂（第 5.5 節）。
4. **驗證失敗一律退回 `pulse.prev`，不放寬驗證**。套件管理員回滾失敗時，也以 `pulse.prev` 做緊急復原，但結果是**可機器判讀的降級狀態並觸發告警**。這屬於對外行為變更，**要等本 ADR 核定後才實作**。
5. **金鑰到期與金鑰撤銷分開處理**（決策點 D1）：`valid-before` 自然到期時，只要完整性通過就可以回滾；金鑰被**顯式移除**（撤銷）時，Linux 不得自動重跑以該金鑰簽署之封存套件的維護指令碼，改退 `pulse.prev` 並標記套件資料庫不一致。Windows 的前一槽仍以完整性優先（沒有別的版本可退），但要記錄高嚴重度狀態。
6. **第二階段**：輔助程式在升級成功後，把套件封存在 root 專屬目錄，以發佈檔 SHA-256 為主鍵綁定。**Linux 的第二階段不綁 Windows 時程**，只共用紀錄格式與測試案例。
7. **不採用**「由 Kairos 簽署回滾目標」；**不單獨採用**版本下限。

信任規則寫成與平台無關的不變條件 INV-1～INV-11（第 5.2 節），Windows 的「前一槽」直接套用。Windows 目前沒有權限隔離，所以這些條件在 Windows 只是縱深防禦（第 7.3 節）。

---

## 1. 脈絡

### 1.1 術語

| 術語 | 說明 |
|---|---|
| 回滾檔（`rollback_artifact`） | deb／rpm 主機「目前版本」的同格式套件，健康確認失敗時由套件管理員裝回，讓套件資料庫與實際檔案一致 |
| `rollback_bundle` | 本 ADR 提議的新欄位：回滾檔所屬那次發佈的 `SHA256SUMS` 全文與 `SHA256SUMS.sig` 簽章 |
| 本機紀錄 | Pulse 每次暫存遠端升級時，把**目標版本**的 `SHA256SUMS`／`.sig` 存到 `/var/lib/pulse/upgrade/releases/<版本>/`。服務帳號可寫，只是**提示** |
| 信任錨（`allowed_signers`） | 本機由 root 擁有的公鑰清單，所有簽章驗證的根；Kairos 不能遠端修改 |
| 輔助程式（helper） | `pulse-upgrade.service` 以 root 執行的 `/opt/pulse/pulse upgrade apply`，負責重新驗證、替換、健康確認與回滾；它就是**目前安裝的那一版**執行檔 |
| `pulse.prev` | 輔助程式在替換前以 root 把 `/opt/pulse/pulse` 複製出來的備份 |
| 交易紀錄 | 輔助程式在 root 專屬目錄 `/var/lib/pulse-upgrade/<id>/state.json` 中記下的這次升級的步驟與綁定資料 |
| 自然到期 | `allowed_signers` 中某把金鑰的 `valid-before` 已過，但那一行仍在檔案中 |
| 撤銷（顯式移除） | 某把金鑰的那一行已從 `allowed_signers` 刪除 |
| 降版政策 | 遠端降版必須「推送允許」且「本機 `upgrade.allow_downgrade = true`」兩者都成立（`REMOTE.md` 第 4.8 節） |
| 種子（seed） | 由本機管理者安裝時就建立的回滾來源，不是遠端升級產生的 |
| 取得時驗證 | 替換**之前**、決定要不要採用某個回滾來源時做的驗證（現行做法） |
| 降級回滾 | 回滾後服務恢復，但套件內容或資料庫與實際檔案不一致的結果（第 5.4 節） |

### 1.2 現況的資料流

| 步驟 | 誰 | 做什麼 | 出處 |
|---|---|---|---|
| 組提議 | Kairos | 對 deb／rpm 主機，從版本庫找主機目前版本（hello 的 `agent_version`，只有版本號、沒有 commit）同格式、同架構的檔案當 `rollback_artifact`；目前版本是外部 URL 版本庫時，只在目標也是外部 URL 時才附。**不附**回滾檔所屬發佈的簽章資料 | `kairos/src/remote/engine.rs:1261-1275`、`1317-1319`、`1327-1328`；`pulse/src/network.rs:1529` 〔程式確認〕 |
| 判斷可信 | Pulse（`pulse` 帳號） | `rollback_is_trusted` 讀 `releases/<自身版本>/`，以當下的信任錨與時間驗證，確認回滾檔列在其中、`version = current_version`，**不檢查 `commit`**。不可信就不下載、照常升級 | `pulse/src/remote/upgrade/mod.rs:1241-1254`、`1305-1340` 〔程式確認〕 |
| 暫存 | Pulse | 可信時把本機紀錄複製成 `rollback.SHA256SUMS(.sig)`；把**目標**的簽章資料存成 `releases/<to_version>/`；只保留兩份 | `mod.rs:1647-1681`、`1712-1724` 〔程式確認〕 |
| 交付 | Pulse | 寫 `request.json`（0644，服務帳號擁有） | `mod.rs:1755-1776` 〔程式確認〕 |
| 複製與重新驗證 | 輔助程式（root） | 複製到 `/var/lib/pulse-upgrade/<upgrade_id>/`；目標的版本政策用自己的版本。0.9.0／本工作樹中，回滾檔的版本期望值用 `req.from_version`；**0.10.0 整合分支改用輔助程式自己的版本**（`trust_request`） | `helper.rs:418-452`、`534-587`（第 572 行）；0.10.0：63cd981 〔程式確認〕 |
| 替換 | 輔助程式 | 0.10.0：先寫 `state.json`（`verified`）→ 保留 `pulse.prev` → 記下 `prev_saved` → 複製回滾檔 → 替換；兩次寫入失敗就不替換 | 0.10.0：f08a3da 〔程式確認〕 |
| 回滾 | 輔助程式 | deb／rpm 有 `rollback.pkg` 時用 `dpkg -i`／`rpm -U --oldpackage --replacepkgs`，否則用 `pulse.prev`。**套件回滾失敗時直接 `rollback_failed`**（0.10.0 仍未改） | `helper.rs:866-933` 〔程式確認〕 |
| 重開機接續 | 輔助程式 | 0.10.0：`from_version`、`method`、`target`、`rollback_artifact` 取自 `state.json`；**工作目錄仍以 `request.json` 的 `upgrade_id` 決定** | 0.10.0 整合分支 `helper.rs:346`；a11d449 〔程式確認〕 |

### 1.3 三種安裝方式的差異

| | tar | deb | rpm |
|---|---|---|---|
| 回滾方式 | `pulse.prev`（＋unit 備份） | 有可信回滾檔：`dpkg -i`；否則 `pulse.prev` | 有可信回滾檔：`rpm -U --oldpackage --replacepkgs`；否則 `pulse.prev` |
| 需要回滾檔 | 否 | 是（為了套件資料庫一致） | 是 |
| 回滾的信任根 | 「它就是替換前在執行的位元組」 | 發佈簽章；退路同 tar | 同 deb |
| 回滾時以 root 執行的其他程式 | 無 | 舊版套件的維護指令碼，以及失敗那一版的 `prerm`〔推論：dpkg 一般行為〕 | 舊版 scriptlet |
| 退回 `pulse.prev` 的副作用 | 無 | `dpkg -l` 顯示失敗的版本；unit 等其他套件檔案仍是新版 | `rpm -q` 顯示失敗的版本 |

### 1.4 已知限制

1. **第一次遠端升級沒有套件回滾**：手動安裝（含會驗證簽章的 `pulse-install.sh`，`pulse/packaging/install.sh:147-171`）都不寫本機紀錄。系統層測試 b0 已實測（`tests/systemd/README.md:73`）。〔程式確認＋實測紀錄〕
2. **主控台的「可自動回滾」與主機實際能力不一致**：Kairos 只看版本庫，不知道 Pulse 端有沒有本機紀錄。〔推論〕
3. **規格與實作不一致**：`REMOTE.md` 第 4.5.3 節寫「沒有回滾檔時 `failed`、不回復」，實作一律有 `pulse.prev` 退路（0.10.0 整合分支的 `REMOTE.md:490-492` 仍未改）。〔程式確認〕
4. **金鑰輪替後失去套件回滾**：本機紀錄在每次使用時以當下的信任錨與時間驗證。`RELEASING.md:121` 要求金鑰外洩時以新金鑰重簽仍在支援期內的發佈，但 Kairos 版本庫無法替換既有版本的 `.sig`（`kairos/src/remote/ops.rs:899-907`、`kairos/src/releases_cli.rs:504-544`）。〔程式確認〕
5. **規格把「移除金鑰」定義為「拒絕該金鑰簽的所有發佈」**（`REMOTE.md:407-409`），沒有區分到期與撤銷，也沒有說明它對已安裝、已封存版本的意義。〔文件〕

### 1.5 交叉核對與查證摘要

初版的兩份分析（資安、工程）與外部審查的主張，逐項查證結果見第 14 節修訂紀錄與第 10 節。會影響決策的事實：

| # | 主張 | 查證結果 |
|---|---|---|
| 1 | 輔助程式以 `request.json` 的 `from_version` 驗證回滾檔 | 成立（`helper.rs:572`；`files.rs:89-115` 不檢查）。`pulse-v0.9.0` 與 4eb7086 的 `helper.rs`、`files.rs` 沒有差異（`git diff pulse-v0.9.0 4eb7086 --stat`，本 ADR 撰寫時執行；外部審查在其限制下未獨立重核）。0.10.0 已修（63cd981）〔程式確認〕 |
| 2 | 狀態寫入失敗被忽略、損毀當成全新請求、替換後到記下 `Replaced` 之間的窗口會覆寫 `pulse.prev` | 成立（`helper.rs:324-329`、`362-369`、`688-714`）。0.10.0 已修（f08a3da）〔程式確認〕 |
| 3 | `write_atomic` 在 `rename` 後不 fsync 目錄 | 成立，0.10.0 仍是如此（`pulse/src/remote/mod.rs` 的 `write_atomic`：檔案 `sync_all` 後 `rename`，沒有開目錄 fsync）〔程式確認〕 |
| 4 | `create_dir_all_mode` 遇到已存在的目錄直接接受，不檢查擁有者與權限 | 成立（`pulse/src/remote/mod.rs:71-91`，0.10.0 相同）〔程式確認〕 |
| 5 | `UpgradeOffer` 沒有 `deny_unknown_fields` | 成立（`crates/kairos-proto/src/mgmt/mod.rs:777-801`、`messages.rs:4`）〔程式確認〕 |
| 6 | 單行上限 65536 bytes，Kairos 不檢查序列化後的總長 | 成立（`crates/kairos-proto/src/lib.rs:114`、`engine.rs:1350`、`pulse/src/network.rs:447-448`、`543`）。初版的「最壞 62～63 KiB」**不成立**，已刪除（第 6.3 節）〔程式確認〕 |
| 7 | `upgrade_status.state` 只接受固定的值 | 成立（`mgmt/mod.rs:1007`）：新增 `rolled_back_degraded` 這類狀態，舊 Kairos 會當成 `bad_request`，所以降級回滾用旗標表示（第 5.4 節）〔程式確認〕 |
| 8 | Pulse 知道自己的 commit | 成立：`env!("PULSE_BUILD_COMMIT")`（`pulse/src/cli.rs:11`、`pulse/build.rs`）。hello 的 `agent_version` 不含 commit（`pulse/src/main.rs:659` 的註解）〔程式確認〕 |
| 9 | 同一個 Kairos 不能為同一版本匯入不同的 `SHA256SUMS` | 成立（`ops.rs:899-907`、`releases_cli.rs:528-536`）；發佈流程「tag 不移動、出問題發佈新 PATCH」（`RELEASING.md:42`）〔程式確認／文件〕 |

---

## 2. 威脅模型摘要

| 攻擊者 | 現況（0.9.0）能做到什麼 | 決策後（0.10.0 修正＋第一、二階段） |
|---|---|---|
| T1 被入侵的 Kairos／主控台帳號 | 不能讓 root 安裝未簽章的內容；回滾檔被綁在主機自身版本。可以不附回滾檔，或故意讓健康確認失敗觸發大量回滾〔程式確認／推論〕 | 不能改變回滾後執行的代理程式（INV-2）。**仍能**在「同版本、同平台、官方簽署、執行檔與 commit 相同」的封裝之間選擇，影響維護指令碼、unit 等其他內容（殘餘風險 R8），以發佈政策收斂（第 5.5 節）。仍能不附 bundle、影響可用性 |
| T2 網路中間人 | 只能讓下載失敗 → 退回 `pulse.prev` | 相同 |
| T3 主機上的非特權使用者 | **未發現直接路徑**：`/var/lib/pulse/upgrade` 為 0750、`/opt/pulse` 屬 root、`/var/lib/pulse-upgrade` 由 systemd 以 0700 建立〔推論〕。下列假設未逐一核對：既存目錄的擁有者與權限（`create_dir_all_mode` 不重新檢查）、systemd 對既存 `StateDirectory` 是否強制 `StateDirectoryMode=0700`、tar 安裝 `/opt/pulse` 的擁有者與權限（依 INSTALL 手動設定）、各路徑元件沒有被換成符號連結、掛載選項 | 第 9.1 節加入路徑元件的擁有者與權限檢查後重新評估 |
| **T4 被入侵的 `pulse` 服務帳號** | 可以讓 root 裝回任一有官方簽章的舊版，繞過 `allow_downgrade = false`；可以在接續時改寫 `request.json`；可以偽造 `status.json`〔程式確認＋推論〕 | 0.10.0 擋下 `from_version`、`method` 與接續時的欄位改寫。**仍可**在重開機前換掉 `request.json` 的 `upgrade_id`，讓輔助程式把它當成新交易：覆寫 `pulse.prev`，並在結束時刪除前一個工作目錄（第 10 節 G1，推論）。仍能影響健康確認（R1） |
| T5 外部 URL 檔案伺服器被入侵 | 只能讓下載失敗 | 相同；第二階段起回滾不依賴它 |
| T6 持有公開的舊版官方套件與簽章 | 與 T4 結合即可降版 | 0.10.0 起無法利用 |
| T7 發佈金鑰外洩 | 可以直接推送惡意新版，與回滾無關 | 撤銷（移除金鑰）後，Linux 不再自動重跑以該金鑰簽署的封存套件之維護指令碼（D1）；`pulse.prev` 路徑仍會恢復同一份執行檔 |
| T8 時鐘操控 | 全域問題 | 相同 |

---

## 3. 決策驅動因素

| # | 因素 | 說明 |
|---|---|---|
| D1 | 不擴大 Kairos 被入侵時的能力 | Kairos 沒有私鑰；主機只接受本機信任錨驗證過的官方發佈（`REMOTE.md` 第 1 節原則 2、第 5.1 節） |
| D2 | 特權分離真的成立 | `pulse` 帳號的任何輸出對 root 都是不可信輸入 |
| D3 | 回滾的可用性 | 不因 Kairos 停機、外部 URL、金鑰自然到期或時間而失效 |
| D4 | 套件資料庫一致 | 弱點掃描與合規以 `dpkg -l`／`rpm -q` 判斷版本 |
| D5 | 撤銷有意義 | 移除金鑰應該能阻止以該金鑰簽署的**新的** root 程式碼被執行 |
| D6 | 相容性 | 0.9.x／0.10 的 Kairos 與 Pulse 任意組合都能運作 |
| D7 | 跨平台一致 | 規則可直接套用到 ADR-0001 的 Windows 版本槽，共用測試案例 |
| D8 | 成本與元件大小 | 回滾路徑上的程式要少；Windows 的啟動器要小而穩定 |

---

## 4. 考慮的方案

共同前提：輔助程式不連網路，回滾當下能用的只有本機檔案；第 10 節的安全修正是任何方案的前提。

### 方案 0：維持現狀（本機紀錄＋`pulse.prev` 退路）

- 防護：T1、T2、T5；T4 要等 0.10.0 的修正。
- 可用性：第一次遠端升級沒有套件回滾；金鑰輪替後失去套件回滾。
- 成本：0。

### 方案 A：`rollback_bundle`（17.5 原建議）

Kairos 附上回滾檔所屬發佈的 `SHA256SUMS`／`.sig`；本機紀錄優先，沒有時用 bundle；兩者都由輔助程式以信任錨在替換前完整驗證，版本以自己的版本綁定。

- **防護範圍**：與方案 0 相同。但本機紀錄代表「這台主機驗證並暫存過這次發佈」，bundle 只代表「存在這樣一次官方發佈」。所以**信任範圍擴大**：從本機曾見過的那一份，擴大到同版本、同平台、官方簽署的任何封裝。A2 能縮小這個範圍，但無法消除（第 5.5 節）。
- **可用性**：直接解決第一次遠端升級（前提是版本庫有主機目前的版本）。金鑰輪替問題仍在。
- **相容性**：選用欄位，須遵守第 6.3 節的大小規則。
- **成本**：約 3.5～4 人日。

#### 方案 A2：內容綁定（A 的補強）

輔助程式在預檢時解開回滾套件，確認：（1）其中 `/opt/pulse/pulse` 的 SHA-256 等於替換前的 `/opt/pulse/pulse`；（2）套件檔名的 commit 等於執行中版本的 commit（`PULSE_BUILD_COMMIT`，依前綴比對）。任何一項不符，就不用回滾檔、退回 `pulse.prev`。
- 固定了代理程式執行檔與建置來源，擋下「同版本、不同 commit」的重新出包。
- **不固定**套件中的其他檔案（維護指令碼、unit）：同一 commit 的多份官方封裝仍可能不同，這部分只靠簽章與發佈政策。
- 成本約 1～1.5 人日。

### 方案 A'：回滾當下再驗一次簽章

- 安全收益小：回滾檔驗證後就在 root 0700 的目錄中。
- 可用性代價：健康確認最長 1 小時，期間金鑰到期就會在最需要的時候驗不過。
- **不作為一般規則採用**。D1 對「撤銷」的處理需要在回滾當下判斷信任錨是否仍列出該金鑰，但只檢查「是否被移除」，不重新判斷 `valid-before`（第 5.3 節）。

### 方案 B：root 專屬的本機封存＋雜湊綁定

輔助程式在升級**成功**後，把剛裝好、已驗證的套件連同 `SHA256SUMS`、`.sig`、驗證時的信任政策快照，封存到 `/var/lib/pulse-upgrade/archive/<artifact_sha256>/`（root 0700），並寫入索引（第 7.1 節 Q3 的格式）。回滾時優先使用封存，以發佈檔 SHA-256 為主鍵綁定。

- **防護範圍**：與 A＋A2 相同，而且回滾目標固定為「本機實際安裝過的那一份完整封裝」，消除 A 的信任範圍擴大。
- **可用性**：最好。直接 `apt install ./x.deb` 的主機沒有種子，第一次升級仍要靠 A。〔推論〕
- **磁碟**：每台主機多一個套件。〔推論〕
- **成本**：約 9～11 人日。
- **與 Windows 的關係**：ADR-0001 的前一槽本質上就是方案 B。

### 方案 C：回滾版本下限

INV-1 使回滾只能回到替換前的版本，這已是最嚴格的下限。發佈清單的下限可能讓合法的回滾失敗。**不單獨採用**；清單下限的概念只用於遠端降版，另案處理。

### 方案 D：由 Kairos 簽署回滾目標

被入侵的 Kairos 能簽任何東西，沒有增加防禦力；而且違反「Kairos 沒有私鑰」與「信任錨不能被遠端修改」兩個原則。約 8～12 人日，另加金鑰維運。**不採用**。

### 比較

| | 0 現狀 | A＋A2 | B（＋A 備援） | C | D |
|---|---|---|---|---|---|
| 擋 T4 降版（0.10.0 修正後） | ✓ | ✓（縱深） | ✓（縱深） | 部分 | ✗ |
| 第一次升級有套件回滾 | ✗ | ✓（版本庫有目前版本時） | ✓ | — | ✓ |
| Kairos 能影響封裝內容 | 否（只能用本機見過的） | **同版本同 commit 的官方封裝之間**（R8） | 否（封存優先時） | — | 是 |
| 不依賴 Kairos／外部 URL | — | ✗ | ✓ | — | ✗ |
| 金鑰自然到期後仍可用 | ✗ | ✗（需重簽，P8） | ✓ | — | — |
| 成本（人日） | 0 | 約 5～5.5 | 約 9～11 | 0.5～2 | 8～12＋ |

---

## 5. 決策（建議）

### 5.1 分階段

| 階段 | 內容 | 狀態／時程 |
|---|---|---|
| **安全修正** | `from_version`、`method`、接續時的欄位、狀態檔 fail closed、`pulse.prev` 不被覆寫 | **已在 0.10.0 整合分支修正**（63cd981、a11d449、f08a3da），不待核定 |
| **階段 0b：交易紀錄補強** | root active-transaction 指標、完整的交易紀錄 schema 與 checksum、目錄 fsync、路徑元件檢查、逐一當機點的故障注入（第 9.1 節） | 安全性質的補強，建議 0.10.0 發佈前完成；屬內部行為，不改對外狀態 |
| **階段 1** | 方案 A＋A2；降級回滾（第 5.4 節，對外行為變更）；`pulse-install.sh` 寫入種子紀錄；Kairos 的實際序列化長度規則；同版本重新出包的政策 | **核定後**；0.10.x 或下一個次版本 |
| **階段 2** | 方案 B；hello 的 `install.rollback`；`pulse upgrade archive-current` | **獨立於 Windows 時程**；與 ADR-0001 共用紀錄格式與測試案例 |
| 另案 | 版本庫保存多份簽章（P8）；清單下限只用於遠端降版 | 依需求 |

### 5.2 信任不變條件（與平台無關）

「特權元件」在 Linux 指 root 輔助程式，在 Windows 指 ADR-0001 的啟動器與輔助程式；「低權限元件」在 Linux 指以 `pulse` 帳號執行的 Pulse，在 Windows 指代理程式。Windows 目前兩者同為 LocalSystem，見第 7.3 節。

- **INV-1（回滾目標由特權元件自己決定）**：自動回滾的目標版本與 commit，等於特權元件自己記錄的「替換前實際執行的版本」。Linux 為輔助程式第一次執行時的 `CARGO_PKG_VERSION` 與 `PULSE_BUILD_COMMIT`，記在 root 交易紀錄；Windows 為指標檔中最後一個已提交的 generation 的 `active`。
- **INV-2（內容綁定）**：回滾後要執行的代理程式，必須與替換前實際執行的位元組相同（執行檔 SHA-256＝`pulse.prev`，或前一槽記錄的雜湊）。以完整封裝回滾時，封裝另以發佈檔 SHA-256 綁定：第一階段綁定 bundle 或本機紀錄中列出的雜湊，第二階段綁定本機封存時的雜湊。
- **INV-3（新的特權內容需要簽章，在取得時驗證）**：回滾時以特權執行、但不在執行檔綁定範圍內的內容（維護指令碼、unit 等），必須來自以本機信任錨**完整**驗證（含 `valid-after`／`valid-before`）通過的官方發佈。驗證由特權元件在取得時（替換前的 `prepare`，或封存時）執行，結果與發佈檔雜湊、簽署者指紋一起存在特權專屬位置。
- **INV-4（Kairos 不選擇、回滾不需要網路）**：Kairos 最多提供證明資料（回滾檔與 `rollback_bundle`），由特權元件自己驗證與綁定；回滾不需要網路。
- **INV-5（候選來源有明確的資格與順序，只往下退）**：回滾來源依第 5.4 節的固定順序評估；每個來源有列舉的資格條件。不符合就評估下一個，**不得回頭使用已判定不合格的來源，也不得放寬任何資格條件**。「更保守」的定義：以特權執行的新內容更少（重跑維護指令碼 → 只換回執行檔 → 不動），且只依賴更早、更直接由特權元件建立的紀錄。每次退回都在結果中以可機器判讀的方式標明原因。
- **INV-6（信任的紀錄只存在特權專屬位置，且可耐久地找回）**：低權限元件可寫的紀錄（`/var/lib/pulse/upgrade/releases/`、`request.json`）只是提示，使用前必須由特權元件重新驗證。特權元件的交易紀錄、封存索引與 Windows 指標檔只由特權元件寫入：寫入順序為檔案 fsync → `rename` → 目錄 fsync，路徑元件的擁有者與權限在使用前檢查。**進行中的交易由特權專屬的 active-transaction 指標找回**，不以低權限輸入（例如 `request.json` 的 `upgrade_id`）決定要接續哪一筆交易。
- **INV-7（到期與撤銷分開處理）**：受 INV-2 綁定的回滾目標，在回滾當下：
  - 簽署金鑰**仍列在**信任錨中、只是 `valid-before` 已過 → 完整性通過即可使用；
  - 簽署金鑰**已不在**信任錨中（撤銷），或信任錨無法讀取 → Linux 不得自動執行該封裝的維護指令碼，改用 `pulse.prev`，結果標記為降級回滾（`package_db_inconsistent`、`rollback_signer_revoked`）；Windows 前一槽仍以完整性優先，但記錄高嚴重度狀態（`previous_slot_signer_revoked`），並經 `upgrade_status` 與主控台呈現。
  
  撤銷的方式是**刪除那一行**，不是把 `valid-before` 設成過去的時間；`RELEASING.md` 與 `REMOTE.md` 第 4.2.3 節要寫明這一點。
- **INV-8（本機管理者安裝的版本：執行檔層級的例外）**：本機管理者安裝且正在執行的版本是合法的**執行檔層級**回滾來源（tar 的 `pulse.prev`、Windows 已提交的種子槽），信任根是本機管理者的安裝行為。INV-8 **只**豁免 INV-3 對「執行檔恢復」的簽章要求。任何會以特權重跑維護指令碼的套件層級回滾，一律適用 INV-3：只有在種子由特權元件於安裝時以本機信任錨驗證（例如 `pulse-install.sh` 驗證後由 root 寫入的種子封存）時才能使用；直接 `apt install ./x.deb` 而沒有種子的主機，只能做執行檔層級的回滾。
- **INV-9（自動回滾與遠端降版的界線）**：自動回滾不受降版政策限制，**正因為** INV-1 使它只能回到替換前的版本。回到其他任何版本的動作都是遠端降版，受第 4.8 節的兩個開關限制，且必須是明確、可稽核的白名單動作。本機管理者的手動回滾不受遠端降版政策限制，但仍須通過 INV-2，Windows 上也要走試用與健康確認（第 7.1 節 Q6）。
- **INV-10（只有原子提交過的版本是回滾目標）**：「已確認健康」只由一個原子寫入的提交紀錄表示：單調遞增的 commit generation 與該 generation 的 `active`／`previous`（Linux 為封存索引中的條目，Windows 為指標檔）。槽或封存目錄內的 `confirmed` 之類旗標只是快取，不作為依據。沒有通過健康確認的版本（失敗的試用、`rolled_back` 的目標）不得進入提交紀錄，也不得被封存。
- **INV-11（低權限輸入不得決定信任結論、交易身分、回滾目標或特權執行路徑）**：低權限元件提供的輸入，不得直接決定：（a）任何信任結論（簽章、雜湊、版本政策）；（b）要接續哪一筆既有交易；（c）回滾目標；（d）以哪一條特權路徑（tar／dpkg／rpm、套件層級或執行檔層級）執行。會改變特權效果的輸入，必須由特權元件依本機政策與已驗證的發佈資料重新推導，或限制在本機政策允許的範圍內。交易開始後，接續只依特權紀錄。逐欄規則見下表。

#### INV-11 逐欄表（`request.json` 與其他低權限輸入）

| 輸入 | 來源 | 特權元件如何處理 | 接續時可否改變 | 0.10.0 整合分支現況 |
|---|---|---|---|---|
| `schema` | `pulse` | 只檢查格式 | — | 已檢查 |
| `upgrade_id` | `pulse`（源自 Kairos） | 新交易：UUID 格式；**接續**：只由 root active-transaction 指標決定 | 否 | **未完成**：仍以它選擇工作目錄（`helper.rs:346`），見 G1 |
| `target.version`／`commit` | `pulse`（源自 Kairos 提議） | 必須列在已驗證的 `SHA256SUMS` 中，並與清單及發佈檔名一致；版本政策以輔助程式自己的版本判斷 | 否（取自交易紀錄） | `Replaced`／`RollingBack` 已取自 `state.json`；**`Verified` 接續時仍採用請求中的值**，見 G3 |
| `method` | `pulse` | 必須等於輔助程式自己偵測的結果 | 否 | 已完成（a11d449） |
| `artifact`（檔名） | `pulse` | 必須列在已驗證的 `SHA256SUMS` 中，雜湊相符；雜湊要記進交易紀錄 | 否 | 已驗證；**雜湊沒有記進 `state.json`** |
| `manifest` | `pulse` | 必須列在已驗證的 `SHA256SUMS` 中 | 否 | 已驗證 |
| `rollback_artifact` | `pulse` | 提示；依 INV-2、INV-3 驗證與綁定，雜湊記進交易紀錄 | 否 | 檔名已取自 `state.json`；**雜湊與 commit 未綁定** |
| `health_timeout_secs` | `pulse`（源自 Kairos） | 與本機設定取較大值，夾在 120～3600 秒；只影響期限，不影響信任結論 | 否（記在交易紀錄） | 已記在 `state.json` |
| `allow_downgrade` | `pulse`（源自 Kairos） | 必須與本機 `upgrade.allow_downgrade` **同時**為真才允許降版：只能讓結果更嚴，不能放寬本機政策 | 否 | 已如此 |
| `from_version` | `pulse` | 只用來比對，必須等於輔助程式自己的版本 | 否 | 已完成（63cd981） |
| `requested_at` | `pulse` | 只記錄 | — | — |
| 暫存的 `rollback.SHA256SUMS(.sig)` | `pulse`（本機紀錄或 bundle） | 提示；以信任錨完整驗證 | 否（驗證結果記進交易紀錄） | 已驗證 |
| `/var/lib/pulse/upgrade/releases/` | `pulse` | 提示 | — | 已如此 |
| `status.json`（健康訊號） | `pulse` | 無法避免的信任（健康確認本來就看 Pulse 自己的狀態）；以 root 可見的 systemd 屬性交叉檢查 | — | 殘餘風險 R1 |

### 5.3 決策點 D1：金鑰到期或撤銷後，已綁定的回滾目標還能不能用

| 情況 | Linux 封存套件／第一階段的回滾檔 | Linux `pulse.prev` | Windows 前一槽 |
|---|---|---|---|
| 金鑰仍在、`valid-before` 已過（自然到期） | 完整性（INV-2）通過即可使用 | 可用 | 完整性通過即可使用 |
| 金鑰已移除（撤銷）或信任錨無法讀取 | **不自動使用**；改用 `pulse.prev`，結果為降級回滾，`detail` 含 `rollback_signer_revoked`。本機管理者可以明確核准後手動執行套件回滾（第一版不提供命令，見 P12） | 可用 | 完整性優先，照常切回；記錄高嚴重度的 `previous_slot_signer_revoked`，經 `upgrade_status` 與主控台告警 |

**兩條 Linux 路徑的差異（不是安全等價）**：

| | 重裝封存套件 | 只換回 `pulse.prev` |
|---|---|---|
| 恢復的代理程式 | 同一份執行檔（INV-2） | 同一份執行檔 |
| 以 root 執行的其他程式 | 舊版的 `postinst`／scriptlet 與失敗版本的 `prerm`，在**不同的時間與系統狀態**下再執行一次 | 無 |
| 改寫的其他檔案 | 套件內的 unit、文件等全部回到舊版 | 無（仍是失敗版本的內容） |
| 套件資料庫 | 一致 | 不一致 |
| 撤銷的意義 | 會執行被撤銷簽署者簽的程式碼 | 不會再執行新的程式碼 |

**取捨與接受的風險**：
- 自然到期時使用封存套件，接受「以已到期金鑰簽署的維護指令碼再執行一次」。金鑰到期是正常的輪替，不代表不可信；封存時已完整驗證，而且整包雜湊綁定。
- 撤銷時改走 `pulse.prev`，接受套件資料庫不一致（D4），換取「撤銷後不再執行新的 root 程式碼」（D5）。
- Windows 前一槽在撤銷時仍切回，接受「執行以被撤銷簽署者建立的版本」。理由：切回只是恢復替換前**已在執行**的同一份位元組，而 Windows 沒有其他版本可退，拒絕只會讓主機停在失敗的試用版或 `rollback_failed`。而且代理程式與啟動器同為 LocalSystem，被撤銷的版本若有惡意，早已能改寫整個槽區（第 7.3 節）。高嚴重度紀錄讓管理者知道要盡快以新金鑰推送。
- 「撤銷」以信任錨中是否仍有該金鑰的指紋判斷：封存與交易紀錄要保存簽署者指紋。判斷由 Linux 輔助程式與 Windows 輔助程式（可隨 OTA 更新）執行，**Windows 啟動器不需要 SSHSIG 驗證**，只需要讀取特權元件寫下的結果做紀錄；切回的決定不依賴這個結果。

初版的「(c) 一律只檢查完整性」與「兩條路徑安全性沒有差異」的說法已撤回。

### 5.4 回滾來源的優先順序與降級結果（deb／rpm）

| 順序 | 來源 | 資格（全部成立才使用） | 失敗時 |
|---|---|---|---|
| 1 | 第二階段的 root 封存 | 版本與 commit＝INV-1 的記錄；整包 SHA-256 相符；套件內執行檔雜湊＝`pulse.prev`；簽署者未被撤銷（D1）；封存條目屬於已提交的 generation（INV-10） | 評估 2 |
| 2 | 本機紀錄（提示）驗證通過的回滾檔 | 替換前完整驗證通過（INV-3）；版本、commit、執行檔雜湊綁定（A2）；驗證結果、雜湊、簽署者指紋已記進交易紀錄 | 評估 3 |
| 3 | `rollback_bundle`（提示）驗證通過的回滾檔 | 同 2 | 評估 4 |
| 4 | `pulse.prev` | 存在，且雜湊等於交易紀錄中替換前記下的值 | `rollback_failed` |
| — | 第 1～3 項的套件管理員命令失敗 | — | 執行 4，結果為**降級回滾** |

**降級回滾的表示方式**（對外行為變更，要等核定）：
- `upgrade_status.state` 維持 `rolled_back`（舊 Kairos 只接受固定的狀態值，`mgmt/mod.rs:1007`），另外：
  - 新的警告代碼（由設計包分配，與一般的 `PLS-W8407` 區分）；
  - `detail` 以固定前綴 `degraded:` 開頭，接著列出機器可讀的原因，例如 `degraded:package_rollback_failed,package_db_inconsistent` 或 `degraded:rollback_signer_revoked,package_db_inconsistent`。
- Kairos：推送自動暫停（同 `rolled_back`）；主機標記「需要修復套件」；開告警，並寫稽核事件。
- Pulse：`pulse upgrade status` 顯示修復指引（重新安裝目前版本的套件），`pulse status` 以非 0 結束代碼表示需要處理。
- 測試必須涵蓋「套件交易部分完成」的情況（`prerm` 失敗、`postinst` 失敗、解包中斷）。
- 第一版只回報，不自動修復資料庫（不另外執行 `dpkg --configure -a` 之類的命令）。

### 5.5 Kairos 能影響什麼：bundle 的殘餘風險與發佈政策

- INV-2 與 A2 之後，Kairos 能影響的是：要不要附 bundle，以及在「同版本、同平台、同 commit、官方簽署、執行檔相同」的多份封裝之間選哪一份。被選中的封裝的維護指令碼與 unit 會以 root 執行或寫入（R8）。
- **發佈政策**（建議寫進 `RELEASING.md`）：同一個版本號只發佈**一份** `SHA256SUMS`。需要修改封裝時發佈新的 PATCH 版本，不重新出包。這和既有的「tag 不移動」一致（`RELEASING.md:42`），Kairos 也已拒絕同一版本匯入不同的 `SHA256SUMS`（第 1.5 節第 9 列）。重簽（P8）只為相同的 `SHA256SUMS` 增加簽章，不改變內容。
- 長期：第二階段在安裝或升級成功時保存完整的發佈檔雜湊，回滾綁定到本機實際安裝過的那一份，R8 隨之消失。`pulse-install.sh` 的種子也要記下發佈檔雜湊。

---

## 6. 協定與相容性

### 6.1 欄位

```jsonc
// upgrade_offer（Kairos → Pulse）新增選用欄位
"rollback_bundle": {
  "sha256sums": "<回滾檔所屬發佈的 SHA256SUMS 全文>",   // 1..=16384 bytes（MAX_SHA256SUMS_BYTES）
  "signature":  "<該發佈的 SHA256SUMS.sig 全文>"        // 1..=4096 bytes（MAX_SIGNATURE_BYTES）
}
```

- 只有在 `rollback_artifact` 存在時才允許；`validate_upgrade_offer` 檢查大小。
- 只對 `install.method` 為 `deb`／`rpm` 的主機附上；Pulse 在其他安裝方式下忽略。
- Kairos 版本庫已有 `sha256sums`／`signature`（`engine.rs:1327-1328`），不需要改 schema。採用 P8（多份簽章）時，選一份以主機信任錨可能接受的簽署者簽的簽章；Kairos 不知道主機的信任錨，第一版附上最新的一份。
- 不需要改 `request.json` 的 schema；Pulse 仍把採用的簽章資料寫成 `rollback.SHA256SUMS(.sig)`。

### 6.2 是否需要新的 feature

**條件式地不需要**：`UpgradeOffer` 沒有 `deny_unknown_fields`，舊 Pulse 會忽略新欄位。**前提是含新欄位的整則訊息在實際序列化後不超過舊端同樣的單行上限**（第 6.3 節）。超過上限時，舊端會把它當成連線錯誤，而不是忽略欄位。

### 6.3 單行大小的規則

初版「各欄位取上限時最壞約 62～63 KiB」的結論**已刪除**：它沒有把所有字串的 JSON 跳脫與完整訊息的封裝一致地算進去。現有的驗證只限制原始 bytes，沒有實際序列化之前，無法證明含或不含 bundle 的訊息都在上限內。

**規則**（Kairos，送出 `upgrade_offer` 前）：
1. 以要送出的**完整訊息**（實際的伺服器訊息封裝，含 `type`、`seq` 等）實際序列化，取 byte 長度；加上換行字元與預留的餘裕（建議 1 KiB）後，必須 ≤ `MAX_LINE_LENGTH`（65536）。
2. 含 bundle 時超過 → 省略 `rollback_bundle` 再序列化一次；
3. 仍超過 → **不送出**這則提議：Kairos 回 `upgrade_unavailable`，在推送中把這台主機標為明確的原因（例如 `offer_too_large`），開告警並寫稽核事件。主機不會因此斷線。
4. 第 2、3 步的原因要在主控台的主機與推送詳情中看得到，不能只記 Kairos 的 `WARN`。

**測試要求**：
- 以完整的伺服器訊息型別序列化，涵蓋需要大量 JSON 跳脫的合法輸入與惡意輸入（含引號、反斜線、控制字元的清單內容與 URL，以通過或繞過欄位驗證的邊界值構造），分別驗證三段式規則的每一個分支。
- 以凍結的 0.9 與 0.10 Pulse 型別解析：含 bundle 的訊息被忽略欄位；超過上限的訊息從未被送出。
- 比照 `crates/kairos-proto/src/metrics_spec_tests.rs:447` 的長度測試。

沒有 bundle 的現行訊息在實際內容下是否可能超過上限，**未確認**（匯入時的格式檢查應已限制跳脫的比例，但沒有實際驗證）；規則 3 同時涵蓋這種情況。

### 6.4 版本組合

「N」指含階段 1 的版本。

| Kairos ＼ Pulse | 0.9.x | 0.10.0（安全修正，無 bundle） | N |
|---|---|---|---|
| 0.9.x | 現況（有缺陷） | 收不到 bundle → 本機紀錄或 `pulse.prev`；`from_version` 等已修 | 同左，另有 A2 與降級回滾 |
| 0.10.0 | 同上列 0.9.x 欄 | 同上 | 同上 |
| N | 忽略 bundle（大小規則成立時）；仍有缺陷 | 忽略 bundle | 第一次遠端升級即可套件回滾 |

- 0.9.0 的主機升級到 0.10.0 的那一次仍由 0.9.0 的輔助程式執行，沒有保護（`pulse-upgrade.service:19`，CHANGELOG 0.10.0 已註明）。
- 第二階段的 `install.rollback` 是 hello `install` 物件的新欄位，舊 Kairos 忽略。

### 6.5 核定後需要修訂的 `REMOTE.md` 章節

| 章節 | 修訂內容 |
|---|---|
| 2.1（hello `install`） | 第二階段的 `install.rollback` 列舉值（P9） |
| 2.4.1（`upgrade_offer`） | `rollback_bundle` 欄位與大小規則 |
| 2.4.3（`upgrade_status`） | 降級回滾的代碼與 `detail` 前綴 |
| 2.6（相容矩陣） | 第 6.4 節 |
| 4.1.1、8.2（版本庫與 API） | 多份簽章、重簽 API（P8）；同版本只有一份 `SHA256SUMS` |
| 4.2.2（驗證步驟） | 「任何一步失敗都 `failed`」只適用於**目標**；回滾來源的驗證失敗改走第 5.4 節的退回，不讓升級失敗 |
| 4.2.3（金鑰輪替） | 區分自然到期與撤銷；撤銷以刪除那一行表示；撤銷對已封存版本與 Windows 前一槽的效果（INV-7） |
| 4.4.1、4.4.3 | root 交易紀錄、active-transaction 指標、目錄 fsync、接續規則 |
| 4.4.2（回滾所需的檔案） | 第 5.4 節的來源順序 |
| 4.5.2（回滾） | 降級回滾；「回滾失敗後不再嘗試任何動作」改為「套件回滾失敗先以 `pulse.prev` 緊急復原」 |
| 4.5.3（沒有回滾檔時） | 改為實際行為（`pulse.prev`） |
| 4.7.4（自動暫停） | 降級回滾也暫停推送；`offer_too_large` 的處理 |
| 4.8（降版政策） | 自動回滾、手動回滾與遠端降版的界線（INV-9） |
| 5.1、5.3（威脅、稽核） | INV-1～INV-11；新的稽核事件（重簽、降級回滾、撤銷簽署者的前一槽） |
| 6（Kairos 儲存） | 版本的多份簽章 |
| 11（錯誤代碼） | 降級回滾、`offer_too_large`、撤銷相關的代碼（由設計包分配） |
| 12.1～12.3（測試） | 第 9.4 節 |
| 17.3、17.5、17.8 | 標記處理結果；17.8 補上階段 0b |

---

## 7. 與 ADR-0001（Windows OTA 式版本槽）的關係

### 7.1 對 ADR-0001 第 4.11 節 Q1～Q6 的回答（已依外部審查修訂）

| # | 問題 | 結論 | 理由與條件 |
|---|---|---|---|
| Q1 | 回滾當下是否以目前的信任錨重驗簽章？ | **不重驗簽章；啟動器不需要內含 SSHSIG 驗證**。自然到期不影響切回。**撤銷**（金鑰已從信任錨移除）時仍以完整性優先切回，但要記錄高嚴重度的 `previous_slot_signer_revoked`，並影響後續升級與主控台提示。 | 第 5.3 節。撤銷的判斷由可隨 OTA 更新的**輔助程式**在每次升級的 `prepare` 與 `pulse upgrade status` 時，依槽紀錄中的簽署者指紋執行，結果寫進特權狀態；啟動器只負責轉述，不據此改變切回的決定。 |
| Q2 | 由 MSI／zip 安裝、沒有經過 `allowed_signers` 驗證的種子槽能否當回滾目標？ | **可以，但只做執行檔層級**（INV-8），並須同時符合：由啟動器從本機管理者安裝的執行檔建立；槽紀錄位於特權目錄、雜湊相符；**曾以試用＋健康確認成功並被提交**（INV-10）。只因「目前存在」不算合法。 | 種子切換也要走試用與健康確認（第 7.3 節第 3 點）。全新安裝的第一個種子沒有前一槽，不涉及回滾。 |
| Q3 | Linux 與 Windows 的「已驗證發佈的本機紀錄」是否統一格式？鍵用什麼？ | **統一內容格式，不統一信任位置**。**發佈檔 SHA-256 是主要的不可變識別**；版本與 commit 是索引與人讀資訊；另記綁定用的執行檔 SHA-256。 | 紀錄內容：`schema`（版本化）、`artifact_sha256`（主鍵）、`version`、`commit`、`artifact`（檔名）、`binary_sha256`、`source`（`ota`／`seed`／`install`）、驗證時的政策快照（簽署者指紋、principal、當時的 `valid-after`／`valid-before`、信任錨檔案的雜湊、驗證時間）、撤銷狀態（`revoked_at`、偵測時間）、提交的 generation；簽章資料（`SHA256SUMS`、`.sig`、清單）。位置依 INV-6：Windows 的槽目錄在特權目錄中；Linux 的 `/var/lib/pulse/upgrade/releases/` 只是提示，信任的紀錄是第二階段的 `/var/lib/pulse-upgrade/archive/`。 |
| Q4 | `rollback_bundle` 是否只對 deb／rpm 送？ | **是**。`tar`、`slot` 與舊式的 `msi`／`zip` 不送；Pulse 收到也忽略。 | 只有需要讓套件資料庫一致的安裝方式需要回滾檔。 |
| Q5 | 是否允許 Kairos 遠端要求切回前一槽？ | **第一版不提供**。日後若提供，它是**獨立、可稽核的遠端降版動作**：受第 4.8 節的兩個開關限制、是新的白名單動作、只能切到最後一個已提交的 `previous`，不能藏在 `upgrade_offer` 或健康狀態中；主控台的確認與稽核 UI 由 ADR-0002 一併定義。 | INV-9、INV-1、INV-10。 |
| Q6 | `previous` 保留多久？手動回滾是否受版本政策限制？ | **保留到下一次升級提交成功為止**；交易期間不得清除回滾目標，清理本身要原子、可恢復。**本機管理者的手動切槽不受遠端降版政策限制**，但仍須通過完整性檢查，並**走試用＋健康確認**，以一次寫入原子更新 `active`／`previous` 與 generation。 | 避免手動切到無法啟動的舊槽，也避免指標損毀後掃描又選回被退掉的槽。Linux 第二階段的封存同樣預設保留 1 份。 |

### 7.2 ADR-0001 必須遵守的條款

1. **INV-1**：自動切回的目標只能是最後一個已提交 generation 的 `active`；`request.json`、`health.json` 與代理程式寫的任何檔案都不影響目標。
2. **INV-2**：每次啟動任何槽前，比對槽紀錄中的雜湊（ADR-0001 的 V5）。槽日後若包含 DLL 或其他可執行內容，就要比對完整的檔案清單與每檔雜湊。
3. **INV-3**：OTA 建立的槽在建槽時以本機信任錨完整驗證，槽紀錄在封存前寫好。
4. **INV-4**：切回不需要網路，忽略 `rollback_artifact`／`rollback_bundle`。
5. **INV-5**：候選來源的順序與資格：`current.json`（可解析、generation 最大、`active` 槽完整）→ `current.json.bak`（同樣條件）→ 依提交紀錄中 **generation 最大**的已提交槽（排除有 `failed_at` 的槽，以及被手動切換取代的槽）→ 啟動器重建種子槽（走試用）。不得以「版本最高」排序。
6. **INV-6**：槽、指標、交易紀錄只由特權元件寫入，寫入順序含目錄的 flush；位置須通過 ACL 檢查。
7. **INV-7**：撤銷時切回照常，但要記錄高嚴重度狀態（Q1）。
8. **INV-10**：提交以 generation 原子表示；`slot.json` 的 `confirmed` 只是快取。
9. **槽 ID**：要包含發佈檔 SHA-256 的短碼或不重複的 ID（同一 version／commit 的重試、重簽或重建不得衝突）；版本與 commit 另存為顯示資訊。

### 7.3 與 ADR-0001 的衝突點與建議修改（不直接修改 ADR-0001）

1. **第 4.3 節的掃描退路**：「選版本最高、`confirmed: true` 的槽」違反 INV-1、INV-10。建議改用單調遞增的 commit generation（或追加式的提交紀錄），掃描時選最新的已提交或手動選定的 generation，排除 `failed_at` 槽；並定義每一個寫入順序、flush、當機點與恢復結果。外部審查認為這是 ADR-0001 核定前的必要修改，本 ADR 同意。
2. **Windows 沒有權限隔離**：代理程式、輔助程式與啟動器同為 LocalSystem。候選版本若被攻陷或有嚴重的檔案操作錯誤，可以改寫指標、槽紀錄、信任錨、前一槽，甚至啟動器。因此 INV-1、INV-6、INV-11 在 Windows 只能防**良性**的當機與受控寫入中斷，**不是安全邊界**。建議：（a）ADR-0001 明寫這是預覽限制；（b）進入「支援」前，代理程式改以受限 token、服務 SID 或獨立的低權限服務執行，只有特權元件能寫槽、指標與信任錨；（c）在那之前仍實作這些不變條件，降權時就不必重新設計。
3. **第 4.4 節第 3 步的種子切換不經健康確認**：建議種子切換也走試用與健康確認，失敗就切回原本的 `active`（Q2 的條件）。外部審查同樣列為必要修改。
4. **第 4.11 節「與 Linux 的 `releases/` 是同一種資料」**：內容相同，**信任等級不同**。建議改寫為「與 Linux 第二階段的 root 封存同一種資料（第 7.1 節 Q3 的格式）」。
5. **槽 ID 只用 `<版本>-<commit>`**：與 Q3「發佈檔 SHA-256 是主要識別」不一致，而且同一發佈的重試會撞上保留 7 天的失敗槽。建議槽 ID 加入發佈檔雜湊短碼，並定義重試、清除失敗槽與磁碟空間預檢的順序。
6. **第 4.6 節「試用槽保留 7 天」**：與 INV-10 不衝突，但失敗槽必須標記 `failed_at`，讓所有自動選擇明確排除它。

---

## 8. 後果

### 8.1 正面

- `REMOTE.md` 第 5.1 節「`pulse` 帳號被入侵……受降版政策限制」的宣稱在 0.10.0 大致成立，補完階段 0b 後完整成立。
- deb／rpm 主機第一次遠端升級即可套件回滾，消除 17.5 #3 的套件資料庫不一致。
- 撤銷有了明確的語意：移除金鑰後，Linux 不再自動執行該簽署者的新 root 程式碼。
- 降級回滾變得可見：不再把「只換回執行檔」誤報為完整的回滾。
- 不新增 feature（在大小規則下）、不改 Kairos 的資料庫 schema（P8 除外）、Kairos 仍不持有私鑰。

### 8.2 負面

- 第一階段多一次解開回滾套件。
- 第二階段每台 deb／rpm 主機多一個套件的磁碟用量，以及更多需要清理與診斷的狀態。
- 降級回滾增加新的代碼、主控台狀態與告警。
- 撤銷時 Linux 會出現套件資料庫不一致，需要人工修復。
- Kairos 要實作實際序列化的長度規則與 `offer_too_large`。

### 8.3 殘餘風險

| # | 風險 | 嚴重度 | 處理 |
|---|---|---|---|
| R1 | `pulse` 帳號被入侵時仍能影響健康確認 | 中（可用性） | 接受並明列；另案以 root 可見的 systemd 屬性強化 |
| R2 | 0.9.0 主機升級到 0.10.0 的那一次仍由舊輔助程式執行 | 低（0.9.0 沒有正式部署的使用者，P1） | CHANGELOG 已註明；在意的主機手動升級 |
| R3 | 金鑰自然到期後，第一階段的 bundle 與本機紀錄驗證不過 → `pulse.prev` | 低 | 第二階段封存；P8 重簽 |
| R4 | 時鐘操控 | 低～中，全域問題 | 既有的時鐘偏差提示 |
| R5 | 合法回滾時以 root 重跑舊版維護指令碼（含自然到期的情況） | 低 | 取得時驗證＋整包雜湊；撤銷時不重跑 |
| R6 | 沒有種子、版本庫又沒有目前版本的主機，第一次升級仍無套件回滾 | 低 | 主控台照舊標示；`pulse upgrade archive-current` |
| R7 | 發佈金鑰外洩 | 高，但與回滾無關 | 撤銷＋以新金鑰推送乾淨版本 |
| R8 | bundle 把信任範圍擴大到同版本、同 commit 的任何官方封裝；Kairos 可在其中選擇 | 低（需要同一版本存在多份官方封裝，發佈政策禁止） | 第 5.5 節發佈政策；第二階段綁定本機安裝的發佈檔雜湊 |
| R9 | Windows 沒有權限隔離，不變條件只防良性錯誤 | 中（Windows 預覽期間） | 第 7.3 節第 2 點 |
| R10 | 撤銷時 Windows 仍切回由被撤銷簽署者建立的前一槽 | 中 | 高嚴重度紀錄與告警；以新金鑰推送 |

---

## 9. 實作計畫

人日為推估，含單元測試，不含 kp-build 上的執行時間。

### 9.1 階段 0b：交易紀錄補強（約 4～5 人日；建議 0.10.0 發佈前）

| 項目 | 內容 | 人日 |
|---|---|---|
| 0b-1 | **active-transaction 指標**：`/var/lib/pulse-upgrade/active`（root 0600），在第一次寫交易紀錄前寫入（含 `upgrade_id`、交易紀錄的雜湊）。輔助程式啟動時**先讀指標**：有進行中的交易就接續它，`request.json` 的 `upgrade_id` 不同時忽略請求並加註，**不得**開始新交易，也不得在 `finish()` 刪除該交易的工作目錄（0.10.0 整合分支 `helper.rs:1332-1339` 目前會刪除其他目錄） | 1.0 |
| 0b-2 | **交易紀錄 schema v2**：`upgrade_id`、原版本與 commit、目標版本與 commit、`method`、目標發佈檔 SHA-256、回滾檔 SHA-256 與驗證結果（簽署者指紋）、`pulse.prev` 的 SHA-256 與是否已建立、替換前後的套件資料庫版本（`dpkg-query`／`rpm -q`）、`unit_replaced` 的預備狀態、內容 checksum；舊 schema 的遷移規則 | 1.0 |
| 0b-3 | **耐久寫入**：`write_atomic` 在 `rename` 後 fsync 目錄（用於 root 狀態、指標與 `pulse.prev`）；`read_state` 判斷「已動過手」時另外看 `pulse.prev` 的雜湊與套件資料庫版本，不只看 `rollback.pkg`／`pulse.service.prev` | 0.5 |
| 0b-4 | **路徑元件檢查**：`/var/lib/pulse-upgrade` 與其子目錄、`/opt/pulse`：擁有者 root、群組與其他人沒有寫入權限、不是符號連結；既存目錄也檢查（`create_dir_all_mode` 目前不檢查） | 0.5 |
| 0b-5 | **`Verified` 接續**：`target`、`artifact` 雜湊取自交易紀錄（目前只有 `Replaced`／`RollingBack` 這樣做）；tar 的 unit 在當機窗口的還原 | 0.5 |
| 0b-6 | **工作目錄的數量限制**：`pulse` 帳號不能以大量 UUID 塞滿 `/var/lib/pulse-upgrade`（有進行中交易時不開新交易；`rollback_failed` 的紀錄保留上限） | 0.25 |
| 0b-7 | **故障注入**（假 `System`＋單元測試）：首次寫狀態前後、`pulse.prev` 建立前後、套件管理員執行中、套件成功但 `Replaced` 未寫入、回滾成功但 `RollingBack` 未寫入、`Done` 未寫入、目錄 fsync 前後；每一點都斷言接續後的結果與 `pulse.prev` 的內容 | 1.0 |

### 9.2 階段 1（約 6.5～7.5 人日；核定後）

| 元件 | 修改 | 人日 |
|---|---|---|
| `kairos-proto` | `RollbackBundle` 型別、大小檢查、凍結型別測試、`PROTOCOL.md` 測試向量 | 0.5 |
| Kairos | 附 bundle；**實際序列化長度的三段式規則**與 `offer_too_large`（主控台可見、告警、稽核） | 1.0 |
| Pulse | 本機紀錄 → bundle 的來源順序；兩者都只是提示 | 1.0 |
| 輔助程式 | A2：解開回滾套件，比對執行檔雜湊與 commit；把回滾檔雜湊、簽署者指紋記進交易紀錄 | 1.0～1.5 |
| 輔助程式 | **降級回滾**：套件回滾失敗或撤銷 → `pulse.prev`，`rolled_back`＋新代碼＋`degraded:` 前綴 | 0.5 |
| Kairos／主控台 | 降級回滾的暫停、告警、主機標記與修復指引 | 0.5～1.0 |
| 打包 | `pulse-install.sh` 寫入種子紀錄（提示）與發佈檔雜湊 | 0.5 |
| 文件與測試 | 第 6.5 節的 `REMOTE.md` 修訂；`RELEASING.md` 的同版本政策與撤銷方式；第 9.4 節 | 1.5 |

### 9.3 階段 2（約 9～11 人日；不綁 Windows 時程）

root 封存與索引（主鍵為發佈檔 SHA-256、generation、政策快照、撤銷狀態）、清理（保留 N 份、交易期間不清除回滾目標、原子可恢復）、回滾優先使用封存；撤銷判斷（D1）；hello `install.rollback`；Kairos 資格分類與主控台；`pulse-install.sh` 寫入種子封存；`pulse upgrade archive-current`。紀錄格式與第 7.1 節 Q3 相同，放在 `kairos-proto` 的 `release` 模組，Windows 的槽紀錄沿用。

### 9.4 測試情境

**已在 0.10.0 整合分支加入**（〔文件〕＋〔程式確認〕）：單元測試 `tampered_from_version_is_refused_before_anything_changes`、`resuming_after_a_reboot_uses_the_version_recorded_before_the_replacement`、`method_and_target_come_from_the_helper_and_the_recorded_state`、`the_state_file_fails_closed`、`a_crash_between_replacing_and_recording_it_keeps_the_old_backup`（`helper/tests.rs`）；`tests/systemd` 情境 `tamper`（e，重現 `from_version` 缺口：修正後 `failed`／`PLS-E8402`，版本與套件資料庫不變）。

**仍需新增**

| 層級 | 情境 | 預期 |
|---|---|---|
| 單元 | 第 9.1 節 0b-7 的每一個當機點 | 接續結果正確、`pulse.prev` 永遠是舊版 |
| 單元 | `Replaced` 期間 `request.json` 換成新的 `upgrade_id` | 接續原交易、忽略新請求、不刪除原工作目錄 |
| 單元 | 既存的 `/var/lib/pulse-upgrade` 權限過寬、是符號連結 | 拒絕，不替換 |
| 單元 | 回滾套件的執行檔雜湊或 commit 不符 | 退回 `pulse.prev`，加註原因 |
| 單元 | 套件回滾失敗（部分完成） | `pulse.prev`、`rolled_back`＋降級代碼與 `degraded:` |
| 單元 | 簽署者自然到期／被移除／信任錨讀不到（第二階段） | 到期：使用封存；移除或讀不到：`pulse.prev`＋`rollback_signer_revoked` |
| 單元（kairos-proto） | 第 6.3 節的序列化測試（含大量跳脫） | 三段式規則的每一個分支 |
| e2e | 新 Kairos 的 bundle 通過 Pulse 驗證；0.9／0.10 型別的 Pulse 忽略 bundle；超長訊息不送出、主機不斷線 | — |
| systemd | `tamper-resume`：`Replaced` 期間以服務帳號換掉 `request.json`（新 UUID）後重新開機 | 原交易接續、`pulse.prev` 仍是舊版 |
| systemd | `pkg-rollback-fails`：失敗版本的 `prerm` 故意失敗 | 降級回滾 |
| systemd | b0 改預期（版本庫有目前版本） | 以 bundle 經套件管理員回滾 |
| systemd | `b0-nobundle`、`b0-seed`、`binary-mismatch` | 依第 5.4 節 |
| systemd | `key-expired`／`key-revoked`（第二階段） | 到期：封存回滾；撤銷：`pulse.prev`＋降級 |
| systemd | 斷電模擬（容器強制停止）於各當機點 | 同 0b-7 |

---

## 10. 安全修正的狀態

依使用者決定（P1）：只在 0.10.0 修正，不出 0.9.1，不發安全公告。CHANGELOG 0.10.0 的「安全性」一節已記錄這些修正與「0.9.0 → 0.10.0 那一次仍無保護」。

| # | 缺陷（外部審查 2.1 節對應點） | 已在 0.10.0 修正（commit） | 仍待實作 |
|---|---|---|---|
| 1 | 回滾檔的版本取自 `request.json` 的 `from_version` | **63cd981**：`trust_request` 以輔助程式自己的版本為準，第一次執行時不符就 `failed`（`PLS-E8402`，`from_version_mismatch`）；替換前寫入 `state.json`；`Request::validate` 檢查版本格式；systemd 情境 `tamper`。〔程式確認〕 | 無（第一次執行的路徑） |
| 2 | 接續時重讀 `request.json` 的關鍵欄位 | **a11d449**：`Replaced`／`RollingBack` 接續時，`from_version`、`method`、`target`、`rollback_artifact` 取自 `state.json`，被改過時加註 `request_fields_ignored`。〔程式確認〕 | **G1**：工作目錄仍以 `request.json` 的 `upgrade_id` 選擇（0.10.0 整合分支 `helper.rs:346`）。換一個新的 UUID，就會被當成新交易：`trust_request` 以「已是新版」的自身版本比對、通過後覆寫 `pulse.prev`，而 `finish()` 會刪除原交易的工作目錄（`helper.rs:1332-1339`）〔程式確認；攻擊流程推論，須在重新開機後才可行〕 → 0b-1 的 active-transaction 指標（審查 2.1 #5）。**G3**：`Verified` 接續時，`target` 沒有與 `state.json` 比對 → 0b-5 |
| 3 | `method` 由低權限端決定 | **a11d449**：`Paths.method` 由 `install::detect_method` 偵測，不符就 `failed`（`method_mismatch`）。〔程式確認〕 | 無 |
| 4 | 狀態寫入失敗被忽略、損毀當成全新請求、當機窗口覆寫 `pulse.prev` | **f08a3da**：替換前兩次寫入失敗就不替換（`state_not_saved`）；狀態檔不可用或遺失但有替換階段的檔案時 `state_unusable`；`prev_saved` 防止覆寫。〔程式確認〕（審查 2.1 #1、#3 的主要部分） | **G2**：`rename` 後沒有 fsync 目錄，斷電可能遺失 `state.json` 的改名 → 0b-3。`read_state` 只以 `rollback.pkg`／`pulse.service.prev` 判斷「已動過手」，沒有看 `pulse.prev` 與套件資料庫版本（審查 2.1 #3）→ 0b-3。交易紀錄沒有綁定發佈檔與回滾檔的雜湊、`pulse.prev` 雜湊、替換前後的套件資料庫狀態，也沒有 checksum（審查 2.1 #4）→ 0b-2。`prepared → replacing → replaced` 沒有明確的「替換中」步驟（審查 2.1 #2）；tar unit 在當機窗口不會還原 → 0b-2、0b-5。逐一當機點的故障注入只有兩個單元測試（審查第 6 節）→ 0b-7。既存目錄的擁有者與權限沒有檢查 → 0b-4。工作目錄數量沒有限制 → 0b-6 |
| 5 | 套件回滾失敗時直接 `rollback_failed` | 未修（三個 commit 都註明留給本 ADR） | **對外行為變更，要等核定**：第 5.4 節的降級回滾（階段 1） |
| 6 | 版本庫無法以新簽章替換既有版本的 `.sig` | 未修 | P8（保存多份簽章），排在階段 1 之後 |
| 7 | `REMOTE.md` 與實作不一致 | 第 5.1 節、新增的 17.8 節已在 0.10.0 更新（63cd981、a11d449、f08a3da） | 第 4.5.3 節仍寫「沒有回滾檔時 `failed`、不回復」（0.10.0 整合分支 `REMOTE.md:490-492`）；第 6.5 節的其餘章節 |

嚴重度：#1 中（0.9.0 沒有正式部署的使用者，實際暴露低）；G1 低～中（需先取得 `pulse` 帳號並配合重新開機，影響是讓回滾換不回舊版）；G2 低（需要在特定窗口斷電）；#5 低～中（可用性）；#6 低。

---

## 11. 待使用者決定的參數

| # | 參數 | 選項 | 建議／狀態 |
|---|---|---|---|
| **P1** | 安全修正的發佈方式 | 只 0.10.0／另發 0.9.1／加安全公告 | **已決定（使用者，2026-11-12）：只在 0.10.0 修正，不出 0.9.1，不發安全公告**。理由：0.9.0 還沒有正式部署的使用者。外部審查不同意，主張 0.9.1 與 0.10.0 都修，至少發布安全公告；本 ADR 照實記錄。殘餘風險 R2：已安裝 0.9.0 的主機升級到 0.10.0 的那一次仍無保護，CHANGELOG 0.10.0 已提醒手動升級 |
| P2 | 金鑰到期或撤銷後，已綁定的回滾目標（D1） | 見第 5.3 節 | **自然到期**：完整性通過即可（兩平台）。**撤銷**：Linux 不重跑維護指令碼、改用 `pulse.prev`＋降級；Windows 完整性優先＋高嚴重度紀錄。信任錨讀不到視同撤銷 |
| P3 | 保留份數 | 1／2／可設定 | Linux 封存 **1** 份（目前版本）；Windows `active`＋`previous`。前提：交易期間不得清除回滾目標，清理要原子、可恢復 |
| P4 | 回滾下限 | 不設／本機／清單 | **不設**；清單下限的概念只用於遠端降版 |
| P5 | 安裝時寫入種子 | 是／否 | **是**。第一階段寫的是提示；第二階段由 root 保存完整發佈檔、雜湊、來源與確認狀態 |
| P6 | 來源順序 | — | root 封存 > 本機紀錄（提示）> bundle（提示）> `pulse.prev`；提示來源以相同規則重驗 |
| P7 | 階段 2 的時程 | 與 Windows 同期／獨立 | **獨立**（依外部審查修改）；只共用紀錄格式與測試案例 |
| P8 | 版本庫的重簽 | 覆寫／保存多份／不做 | **保存多份簽章**（依外部審查修改）：只接受 `SHA256SUMS` 完全相同、新簽章驗證通過；記錄舊與新的簽署者、時間、操作者與稽核事件；保留歷史供鑑識 |
| P9 | hello 宣告回滾能力 | 布林／列舉／不做 | **列舉**（依外部審查修改）：`archived`、`binary_only`、`none`、`degraded`；舊 Kairos 忽略 |
| P10（新） | 降級回滾的表示 | 新狀態值／`rolled_back`＋新代碼＋`detail` 前綴 | **`rolled_back`＋新代碼＋`degraded:`**（新狀態值會被舊 Kairos 拒絕，`mgmt/mod.rs:1007`）；推送暫停並告警 |
| P11（新） | 同版本重新出包政策 | 禁止／允許 | **禁止**：同一版本只有一份 `SHA256SUMS`；寫進 `RELEASING.md` |
| P12（新） | 撤銷時是否提供「本機管理者明確核准後強制套件回滾」的命令 | 第一版提供／不提供 | **第一版不提供**；以手動重新安裝套件代替 |
| P13（新） | 階段 0b 是否列為 0.10.0 發佈的前提 | 是／否 | **已決定（2026-11-12，使用者）：是，全部做完才發佈 0.10.0**（G1、G2 都是現行信任模型的缺口，且屬內部行為） |

---

## 12. 未確認事項

1. **G1（換 `upgrade_id`）的端到端可利用性**：程式路徑都讀過，沒有實際執行；以 `tamper-resume` 情境確認。
2. **沒有 bundle 的現行 `upgrade_offer` 在實際內容下是否可能超過單行上限**：沒有實際序列化。
3. **0.8.0 是否可作為降版目標**：0.8.0 tag 已有 `packaging/sign-release.sh`，但 0.8.0 的發佈檔名能否被 0.9.0 接受，未查證。因為 0.10.0 已修且 P1 已決定，這一點只影響歷史判斷。
4. **舊版 Pulse 實際解析含 bundle 的訊息**：只做靜態確認。
5. **`PULSE_BUILD_COMMIT` 與發佈檔名中的 commit 一定一致**：依 `build.rs` 的優先順序（`BUILD_COMMIT` → `git rev-parse --short HEAD`）推論一致，長度可能不同，所以 A2 採前綴比對；正式打包流程是否一定設定 `BUILD_COMMIT`，未查證。
6. **deb／rpm 安裝後的 `/opt/pulse/pulse` 與套件內的位元組完全相同**：依套件管理員一般行為推論。
7. **dpkg 降版時失敗版本的 `prerm` 使套件回滾失敗**：依 dpkg 一般行為推論。
8. **systemd 對已存在的 `StateDirectory` 是否強制套用 `StateDirectoryMode=0700`**：未查證（T3 的假設之一）。
9. **主控台「無法自動回滾」的分類程式**（`kairos/src/remote/rollout.rs:617-691`）：引用自初版分析，未逐行重核。
10. **磁碟用量與人日**：推估。
11. **Windows 相關的一切**：尚未實作，均為推論。
12. **`pulse-v0.9.0` 與 4eb7086 的輔助程式相同**：本 ADR 以 `git diff --stat` 確認；外部審查在其限制下沒有獨立重核。

---

## 13. 參考資料

### 程式（本工作樹，4eb7086）

- `pulse/src/remote/upgrade/helper.rs`：`Paths`（152-181）；`apply`（304-340）；接續（372-396）；`prepare`（418-685，回滾檔驗證 555-587、第 572 行）；`replace`（688-759）；回滾（866-947，套件回滾失敗 931-933）；確認回滾（949-975）；`finish`（1094-1135）。
- `pulse/src/remote/upgrade/files.rs`：`Request`（39-61）、`validate`（89-115）、不可信檔案的讀取（146-221）。
- `pulse/src/remote/upgrade/mod.rs`：1241-1254、1305-1340、1647-1681、1712-1724、1748-1776。
- `pulse/src/remote/mod.rs`：`write_atomic`（rename 後沒有目錄 fsync）、`create_dir_all_mode`（71-91）。
- `crates/kairos-proto/src/release/mod.rs`：`verify_bundle`（648-685）、`check_artifact`（700-753）、`check_version_policy`（793-825）。
- `crates/kairos-proto/src/mgmt/mod.rs`：上限（661-690）、`UpgradeOffer`（777-801）、`validate_upgrade_offer`（848-893）、`upgrade_status` 狀態檢查（1007）。
- `crates/kairos-proto/src/messages.rs:4`；`crates/kairos-proto/src/lib.rs:114`；`pulse/src/network.rs:447-448`、`543`、`1529`；`pulse/src/main.rs:659`；`pulse/src/cli.rs:11`、`pulse/build.rs`。
- `kairos/src/remote/engine.rs:1253-1353`；`kairos/src/remote/ops.rs:899-907`；`kairos/src/releases_cli.rs:504-544`。
- `pulse/packaging/install.sh:147-171`；`pulse/packaging/pulse-upgrade.service`（`StateDirectoryMode=0700`、第 19 行的 `ExecStart`）。

### 0.10.0 整合分支（`kp-s10-int`）

- 63cd981 `fix(pulse): 升級輔助程式不再信任 request.json 的 from_version`；a11d449 `fix(pulse): 升級輔助程式自己偵測安裝方式，接續時只信 state.json`；f08a3da `fix(pulse): 升級輔助程式的狀態檔 fail closed；替換後中斷不再覆寫 pulse.prev`。
- `pulse/src/remote/upgrade/helper.rs`：`HelperState` 新欄位、`read_state`、`trust_request`、`replace(p, prior)`；工作目錄的選擇（346）；`finish` 的清理（1304-1345）。
- `pulse/src/remote/upgrade/helper/tests.rs:870`、`915`、`1017`、`1103`、`1211`。
- `crates/kairos-proto/REMOTE.md` 第 5.1 節（618）、第 17.8 節（1282 起）、第 4.5.3 節（490-492，未改）；`CHANGELOG.md` 0.10.0「安全性」（146 起）；`tests/systemd/README.md:79`（`tamper`）。

### 文件

- `crates/kairos-proto/REMOTE.md`：第 2.4、4.2（388-410）、4.4、4.5（472-492）、4.8（585-591）、5.1（610-622）、12.3、17.3、17.5、17.7 節。
- `RELEASING.md:42`、`120-121`；`CHANGELOG.md:168`、`253`；`tests/systemd/README.md:67-89`。
- `docs/adr/0001-windows-ota-upgrade.md` 第 4.1、4.3～4.8、4.11 節；`docs/adr/0002-rollout-console-design.md`。
- `docs/adr/reviews/0003-codex-review.md`；`docs/adr/reviews/0001-codex-review.md`（2.1 掃描、2.4 權限隔離、槽 ID）。

---

## 14. 修訂紀錄

### 2026-11-12：依外部審查修訂

**依據**：`docs/adr/reviews/0003-codex-review.md`（審查對象 4eb7086）；`docs/adr/reviews/0001-codex-review.md` 中與回滾、信任有關的部分；使用者對 P1 的決定；0.10.0 整合分支的 63cd981、a11d449、f08a3da。每一項意見都先回到程式確認（第 1.5、10 節）。

| 審查意見 | 處理 | 理由 | 修改的章節 |
|---|---|---|---|
| 2.1 root `state.json` 修法不足（寫入失敗被忽略、損毀重新開始、當機窗口、目錄 fsync、active-transaction 指標、完整 schema） | **採納** | 逐點確認成立。0.10.0 已修寫入失敗、損毀與 `pulse.prev` 覆寫；目錄 fsync、指標、完整 schema、`Verified` 接續與故障注入仍缺 | 0、1.2、1.5、5.1、5.2（INV-6）、9.1、10、11（P13） |
| 2.2 自然到期與顯式撤銷沒有區分；兩條路徑並非安全等價 | **採納** | 套件回滾會以 root 重跑維護指令碼並改寫其他檔案（`helper.rs:871-899`），與 `pulse.prev` 確實不同；`REMOTE.md:407-409` 把移除金鑰定義為撤銷 | 0、1.4、5.2（INV-7）、5.3、7.1（Q1）、8.3、11（P2） |
| 2.3 INV-11 按字面無法實作 | **採納** | `health_timeout_secs`、`target` 等本來就會改變 root 的行為；改寫為四類禁止事項＋逐欄表 | 5.2（INV-11 與逐欄表） |
| 2.4 62～63 KiB 的論證不正確 | **採納** | 沒有一致計算所有字串的跳脫與完整封裝；改為實際序列化＋三段式規則 | 1.5、6.2、6.3、9.2、9.4 |
| 2.5「Kairos 只能決定附不附」過度陳述 | **採納**，並補強 | 確認回滾驗證不檢查 commit（`helper.rs:573`）。另外查到 Pulse 知道自己的 commit（`PULSE_BUILD_COMMIT`），A2 加入 commit 綁定；殘餘風險與發佈政策明列 | 0、2（T1）、4（A、A2）、5.5、8.3（R8）、11（P11） |
| 2.6 套件回滾失敗後不能回報普通 `rolled_back` | **採納**，表示方式部分調整 | 審查建議新狀態或旗標；新狀態值會被舊 Kairos 拒絕（`mgmt/mod.rs:1007`），所以採 `rolled_back`＋新代碼＋`degraded:` 前綴 | 0、5.4、9.2、11（P10） |
| 3.1 ADR-0001 的掃描退路違反 INV-1／INV-10 | **採納**，並採 0001 審查的 generation 建議 | 以 commit generation 取代 `confirmed_at` 時間 | 5.2（INV-5、INV-10）、7.2、7.3 |
| 3.2 Linux 紀錄的信任等級不一致 | **採納**（初版已指出） | — | 7.1（Q3）、7.3 |
| 3.3 `REMOTE.md` 需修訂的章節不完整 | **採納** | 補上 4.2.2、4.2.3、4.5.2、4.8 等章節 | 6.5、首頁 |
| 3.4 `REMOTE.md` 4.5.3 與程式不一致 | **確認**（初版已指出） | 0.10.0 仍未改 | 1.4、10 |
| 3.5「不待核定」的範圍過大 | **採納** | 降級回滾改列為核定後的階段 1；只保留安全修正與內部的階段 0b | 0、5.1、10 |
| 3.6 ADR-0002 沒有實質衝突 | **採納** | 加入 Q5 日後需要 ADR-0002 定義確認 UI | 7.1（Q5） |
| 3.7「不需要 feature」需加條件 | **採納** | — | 6.2 |
| 3.8 INV-3 與 INV-8 的例外關係不清 | **採納** | INV-8 只豁免執行檔層級；套件層級一律適用 INV-3 | 5.2（INV-3、INV-8）、7.1（Q2） |
| 3.9 INV-5「更保守」沒有可操作定義 | **採納** | 改為列舉的資格與固定順序 | 5.2（INV-5）、5.4、7.2 |
| 5.5 T3「無」證據不足 | **採納** | 確認 `create_dir_all_mode` 不檢查既存目錄；改為「未發現直接路徑」並列出假設 | 2（T3）、9.1（0b-4）、12 |
| 5.6 0.9.0 tag 與 HEAD 相同未獨立重核 | **保留原判斷** | 本 ADR 撰寫時以 `git diff pulse-v0.9.0 4eb7086 --stat` 確認沒有差異；照實標註審查未獨立重核 | 1.5、12 |
| 5.7 0.8.0 降版目標未確認 | **同意**（初版已標示未確認） | P1 已決定，影響降為歷史判斷 | 12 |
| 第 6 節遺漏項（工作目錄耗盡、路徑元件、發佈檔雜湊、撤銷語意、降級的修復流程、`offer_too_large` 的可見性、序列化測試、手動切槽走試用、原子確認紀錄） | **採納** | — | 5.3、5.4、6.3、7.1、9.1、9.2、9.4 |
| P1 不同意只併 0.10.0 | **不採納（使用者已決定）** | 0.9.0 沒有正式部署的使用者；照實記錄審查的不同意見 | 0、10、11 |
| P2 不同意統一採 (c) | **採納** | 同 2.2 | 5.3、11 |
| P3、P4、P5、P6 | **採納附帶條件** | 交易期間不清除回滾目標；種子在 root 專屬位置保存完整資料；改稱「提示來源」 | 11 |
| P7 不要把 Linux 封存綁死 Windows 時程 | **採納** | Windows 實機驗證可能延後 | 5.1、9.3、11 |
| P8 保留多份簽章而非覆寫 | **採納** | 保留鑑識歷史 | 6.1、11 |
| P9 用列舉值 | **採納** | — | 6.5、11 |
| Q1～Q6 的補充條件 | **採納**（Q1 部分採納：撤銷由可更新的輔助程式記錄，但不改變切回的決定） | Windows 沒有其他版本可退，而且沒有權限隔離 | 7.1 |
| 0001 審查：Windows 沒有權限隔離 | **採納** | 不變條件在 Windows 只是縱深防禦 | 0、7.3、8.3（R9） |
| 0001 審查：槽 ID 應含發佈檔雜湊 | **採納** | 與 Q3 的主鍵一致 | 7.2、7.3 |
| 0001 審查：commit generation 取代時間 | **採納** | — | 5.2（INV-10）、7.2、7.3 |

### 2026-11-13：階段 0b 的實作紀錄（不改變任何決定）

第 9.1 節的 0b-1～0b-7 已實作（分支 `sprint11/helper-txn`）：設計、每個當機點的結果與驗證紀錄在 [`REMOTE.md`](../../crates/kairos-proto/REMOTE.md) 第 17.9 節，程式在 `pulse/src/remote/upgrade/helper.rs`、`helper/txn.rs`，測試在 `helper/tests.rs`、`helper/fault_tests.rs` 與 `tests/systemd/`（情境 `tamper-resume`、`crash-replace`、`crash-rollback`）。第 10 節表中 #2 的 G1、G3 與 #4 的 G2 及其後各項隨之完成；#5（套件回滾失敗的降級）、#6、#7 的其餘章節不在階段 0b 的範圍，沒有改。

實作與本 ADR 字面不同之處（都在 `REMOTE.md` 第 17.9 節說明）：

| 本 ADR | 實作 | 理由 |
|---|---|---|
| 0b-1：指標 `/var/lib/pulse-upgrade/active`，「在第一次寫交易紀錄前寫入（含 `upgrade_id`、交易紀錄的雜湊）」 | 檔名 `active.json`（main 上的 0005679 已用這個名稱）；順序是**紀錄先於指標、指標先於任何變更**：`preparing`、`prepared` 兩份紀錄先寫，驗證與預檢都通過後才寫指標，再進入 `replacing`。指標內容是 `upgrade_id` 與交易紀錄中「綁定欄位」的雜湊（`binding_sha256`），不是整份紀錄的雜湊 | 指標代表「已經（或即將）變更系統」：預檢失敗的請求不留下指標，也就不會把之後的請求擋下；綁定欄位在 `prepared` 之後不變，整份紀錄的雜湊則每一步都會變（指標就得每一步重寫，多一個當機窗口）。INV-6「進行中的交易由特權專屬的指標找回」不受影響：任何變更之前指標一定已經寫入 |
| 0b-3：`read_state` 判斷「已動過手」時另外看 `pulse.prev` 的雜湊與套件資料庫版本 | 沒有指標的 `preparing`／`prepared` 紀錄：比對執行檔的雜湊（`old_binary_sha256`）與套件資料庫（`pkg_db_before`），不同就 `state_unusable`；`pulse.prev` 的雜湊用在「是否已保留」「能否用來回滾」的判斷 | `pulse.prev` 在交易之前可能已經存在（上一次升級留下的），它的存在或內容不能說明這一筆交易是否動過手；執行檔與套件資料庫才是 |
| 0b-7 的當機點清單 | 22 個當機點（`REMOTE.md` 第 17.9.4 節），涵蓋並細分 0b-7 列的每一點 | — |
