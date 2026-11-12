# ADR-0003：遠端升級的回滾檔（`rollback_bundle`）如何被信任

| 項目 | 內容 |
|---|---|
| 狀態 | 提議 |
| 日期 | 2026-11-12 |
| 決策者 | 待使用者核定 |
| 處理的待決 | [`REMOTE.md`](../../crates/kairos-proto/REMOTE.md) 第 17.5 節第 1 項（回滾檔的信任）、第 3 項（套件資料庫與執行檔不一致）；第 17.3 節第 1 項（與規格的差異） |
| 取代／修訂 | 核定後修訂 `REMOTE.md` 第 2.4.1、4.4.2、4.5.3、5.1、17.3、17.5 節 |
| 相關 | [ADR-0001](0001-windows-ota-upgrade.md)（Windows 的 OTA 式版本槽，第 4.11 節向本 ADR 提出 Q1～Q6）；`RELEASING.md` 第 3 節 |

**標記方式**：**〔程式確認〕**＝讀程式碼確認（附檔案與行號）；**〔文件〕**＝只見於規格、CHANGELOG 或測試說明；**〔推論〕**＝依程式與文件推導，沒有實測。本 ADR 的撰寫只讀程式與文件，沒有建置、執行或實測。路徑一律相對於儲存庫根目錄。

---

## 0. 結論先行

1. **先修一個現行缺陷（不必等本 ADR 核定）**：root 輔助程式驗證回滾檔時，版本的期望值取自服務帳號 `pulse` 寫的 `request.json` 的 `from_version`（`pulse/src/remote/upgrade/helper.rs:572`），而不是輔助程式自己的版本。取得 `pulse` 帳號的攻擊者可以讓 root 經 `dpkg -i`／`rpm -U --oldpackage` 裝回任何一個**有官方簽章的舊版**，繞過本機的 `upgrade.allow_downgrade = false`。已發佈的 Pulse 0.9.0 有這個缺陷（`pulse-v0.9.0` tag 的 `helper.rs`、`files.rs` 與目前程式完全相同）。嚴重度：**中**。〔程式確認：缺少檢查；攻擊流程為推論，未實測〕
2. **採用 17.5 原建議的 `rollback_bundle`**：`upgrade_offer` 新增選用欄位，Kairos 附上「回滾檔所屬發佈」的 `SHA256SUMS`／`.sig`。本機紀錄優先，沒有時才用 Kairos 附上的資料；兩者都由 root 輔助程式在**替換前**以本機信任錨重新驗證。協定不升版、**不需要新的 feature**（`UpgradeOffer` 沒有 `deny_unknown_fields`）。
3. **再加一道內容綁定**：回滾套件中的 `/opt/pulse/pulse` 的 SHA-256 必須等於替換前實際安裝的執行檔（即 `pulse.prev`）。有了這一條，Kairos 能影響的只有「要不要附資料」，不能影響「回到哪裡」。
4. **任何驗證失敗都退回 `pulse.prev`，不放寬驗證**；套件管理員執行回滾失敗時也退回 `pulse.prev`（目前會直接 `rollback_failed`）。
5. **第二階段**（建議與 Windows 一起做）：輔助程式在升級成功後，把剛裝好的套件封存在 root 專屬目錄，以整包 SHA-256 綁定；回滾時不再依賴 Kairos、外部 URL 或舊金鑰的有效期。
6. **金鑰過期或被移除後，封存的回滾目標還能不能用**（兩位分析師的分歧）：**建議可以用**，回滾當下的簽章狀態只記錄、不阻擋。理由：受 INV-2 綁定的回滾目標就是「替換前正在執行的那份位元組」，退路 `pulse.prev` 也是同一份位元組；阻擋它不會移除任何程式，只會讓套件資料庫不一致（Linux）或沒有可退的版本（Windows）。這也回答了 ADR-0001 的 Q1：Windows 的啟動器不需要內含簽章驗證。
7. **不採用**「由 Kairos 簽署回滾目標」；**不單獨採用**版本下限。

信任規則寫成與平台無關的不變條件 INV-1～INV-11（第 5.2 節），Windows 的「前一槽」直接套用。

---

## 1. 脈絡

### 1.1 術語（第一次出現時的一句話說明）

| 術語 | 說明 |
|---|---|
| 回滾檔（`rollback_artifact`） | deb／rpm 主機「目前版本」的同格式套件，健康確認失敗時以套件管理員裝回，讓套件資料庫與實際檔案一致 |
| `rollback_bundle` | 本 ADR 提議的新欄位：回滾檔所屬那次發佈的 `SHA256SUMS` 全文與 `SHA256SUMS.sig` 簽章 |
| 本機紀錄 | Pulse 每次暫存一個遠端升級時，把**目標版本**的 `SHA256SUMS`／`.sig` 存到 `/var/lib/pulse/upgrade/releases/<版本>/`；服務帳號可寫，只是「提示」 |
| 信任錨（`allowed_signers`） | 本機由 root 擁有的公鑰清單，所有簽章驗證的根；Kairos 不能遠端修改 |
| 輔助程式（helper） | `pulse-upgrade.service` 以 root 執行的 `/opt/pulse/pulse upgrade apply`，負責重新驗證、替換、健康確認與回滾；它就是**目前安裝的那一版**執行檔 |
| `pulse.prev` | 輔助程式在替換前，以 root 把 `/opt/pulse/pulse` 複製出來的備份 |
| 健康確認 | 替換後在期限內確認新版正常執行並連上 Kairos；失敗就回滾 |
| `valid-before` | `allowed_signers` 中某把金鑰的失效時間，以本機時鐘判斷 |
| 降版政策 | 遠端降版必須「推送允許」且「本機 `upgrade.allow_downgrade = true`」兩者都成立（`REMOTE.md` 第 4.8 節）；自動回滾不經過這個判斷 |
| 種子（seed） | 由本機管理者安裝（`pulse-install.sh`、MSI、zip）時就建立的回滾來源，不是遠端升級產生的 |
| 取得時驗證／回滾當下驗證 | 前者是在替換**之前**、決定要不要採用某個回滾來源時做的驗證（目前的做法）；後者是在健康確認失敗、**即將回滾時**再驗一次 |

### 1.2 現況的資料流

| 步驟 | 誰 | 做什麼 | 出處 |
|---|---|---|---|
| 組提議 | Kairos | deb／rpm 主機：從版本庫找主機目前版本（hello 的 `agent_version`）同格式、同架構的檔案當 `rollback_artifact`；目前版本是外部 URL 版本庫時，只在目標也是外部 URL 時才附。**不附**回滾檔所屬發佈的簽章資料 | `kairos/src/remote/engine.rs:1261-1275`、`1317-1319`、`1327-1328` 〔程式確認〕 |
| 判斷可信 | Pulse（`pulse` 帳號） | `rollback_is_trusted`：讀 `releases/<自身執行中版本>/`，以**當下**的信任錨與時間驗證，再確認回滾檔名與雜湊列在其中、`version = current_version`、`commit` 不檢查。不可信就不下載、照常升級 | `pulse/src/remote/upgrade/mod.rs:1241-1254`、`1305-1340` 〔程式確認〕 |
| 暫存 | Pulse | 可信時把本機紀錄複製成暫存目錄的 `rollback.SHA256SUMS(.sig)`；另外把**目標**版本的簽章資料存成 `releases/<to_version>/`；只保留目前與目標兩份 | `mod.rs:1647-1681`、`1712-1724` 〔程式確認〕 |
| 交付 | Pulse | 寫 `request.json`（0644，服務帳號擁有），含 `from_version`、`method`、`rollback_artifact` | `mod.rs:1755-1776` 〔程式確認〕 |
| 複製與重新驗證 | 輔助程式（root） | 以 `O_NOFOLLOW`、`st_nlink == 1`、大小上限把檔案複製到 `/var/lib/pulse-upgrade/<id>/`（0700）；目標的版本政策以**自己的** `current_version` 判斷，但回滾檔的版本期望值用 **`req.from_version`** | `helper.rs:418-452`、`534-554`、`555-587`（第 572 行） 〔程式確認〕 |
| 替換 | 輔助程式 | `/opt/pulse/pulse` → `pulse.prev`；驗證過的回滾檔 → 工作目錄的 `rollback.pkg`（重開機接續時不再驗證） | `helper.rs:688-701` 〔程式確認〕 |
| 回滾 | 輔助程式 | deb／rpm 且有 `rollback.pkg`：`dpkg -i`／`rpm -U --oldpackage --replacepkgs`；其他：以 `pulse.prev` 換回執行檔（deb／rpm 加註 `binary_restored_package_db_shows_<版本>`）。套件回滾失敗**直接** `rollback_failed`，不退回 `pulse.prev` | `helper.rs:866-933` 〔程式確認〕 |
| 確認回滾 | 輔助程式 | 期望版本為 **`req.from_version`** | `helper.rs:949-954` 〔程式確認〕 |
| 重開機接續 | 輔助程式 | 每次執行都重新讀 `request.json`（`apply` 第一行），接續 `Replaced`／`RollingBack` 時沿用其中的 `from_version`、`method`、`target` | `helper.rs:304-340`、`372-396` 〔程式確認〕 |

### 1.3 三種安裝方式的差異

| | tar | deb | rpm |
|---|---|---|---|
| 回滾方式 | `pulse.prev`（＋unit 備份） | 有可信回滾檔：`dpkg -i`；否則 `pulse.prev` | 有可信回滾檔：`rpm -U --oldpackage --replacepkgs`；否則 `pulse.prev` |
| 需要回滾檔 | 否 | 是（只為了套件資料庫一致） | 是 |
| 回滾的信任根 | 「它就是替換前在執行的位元組」（不檢查簽章） | 發佈簽章（本機紀錄）；退路同 tar | 同 deb |
| 回滾時以 root 執行的其他程式 | 無 | 舊版套件的維護指令碼（`postinst` 等），以及失敗那一版的 `prerm`〔推論：dpkg 一般行為〕 | 舊版 scriptlet |
| 退回 `pulse.prev` 的副作用 | 無 | `dpkg -l` 顯示失敗的版本；unit 等其他檔案仍是新版 | `rpm -q` 顯示失敗的版本 |

〔程式確認：`helper.rs:866-930`；文件：`REMOTE.md` 第 4.4.2 節、17.2 #20〕

### 1.4 已知限制

1. **第一次遠端升級沒有套件回滾**：本機紀錄只在暫存遠端升級時為目標版本寫入；手動安裝（含會驗證簽章的 `pulse-install.sh`，`pulse/packaging/install.sh:147-171`）都不寫。所以從手動安裝的版本第一次遠端升級時，回滾檔一律被判為不可信，失敗時以 `pulse.prev` 回復，套件資料庫停在失敗的版本。系統層測試 b0 已實測（`tests/systemd/README.md:73`，`REMOTE.md` 第 12.3 節）。〔程式確認＋實測紀錄〕
2. **主控台的「可自動回滾」與主機實際能力不一致**：Kairos 只看版本庫有沒有主機目前的版本，不知道 Pulse 端有沒有本機紀錄。〔推論〕
3. **規格與實作不一致**：`REMOTE.md` 第 4.5.3 節寫「沒有回滾檔時回報 `failed`、不做任何回復」，實作一律有 `pulse.prev` 退路；第 5.1 節宣稱「`pulse` 帳號被入侵……同樣受降版政策限制」，但第 0 節第 1 點的缺陷使它不成立。〔程式確認〕
4. **金鑰輪替後失去套件回滾**：本機紀錄在每次使用時以當下的信任錨與時間驗證；舊金鑰過了 `valid-before` 或被移除後就驗證不過。`RELEASING.md:121` 要求「以新金鑰重新簽署仍在支援期內的發佈」，但 Kairos 版本庫無法替換既有版本的 `.sig`（第 1.5 節第 4 列）。〔程式確認〕

### 1.5 兩份分析的交叉核對與查證結果

| # | 主張 | 資安分析 | 工程分析 | 本 ADR 查證 |
|---|---|---|---|---|
| 1 | 輔助程式以 `request.json` 的 `from_version` 驗證回滾檔，可被 `pulse` 帳號利用來降版 | 提出（F1），建議修正 | 獨立提出（1.6） | **成立**。`helper.rs:572` 用 `req.from_version`；`files.rs:89-115` 的 `validate` 不檢查它；版本政策卻用 `self.paths.current_version`（`helper.rs:534-548`，值為 `env!("CARGO_PKG_VERSION")`，`helper.rs:178`）。`check_version_policy` 把「相同版本」視為重新安裝、不受降版限制（`crates/kairos-proto/src/release/mod.rs:800-801`），攻擊者可以用「重新安裝目前版本」當目標。`git diff pulse-v0.9.0 HEAD -- helper.rs files.rs` 沒有差異，**0.9.0 受影響**。〔程式確認；攻擊流程推論〕 |
| 2 | 修正應以輔助程式第一次執行時記下的自身版本為準 | 建議存入 root 的 `state.json` | 相同 | **同意**，而且範圍要擴大：接續時 `request.json` 會被重新讀取（`helper.rs:305`），`method`、`target.version`、`rollback_artifact` 也都來自它。`state.json` 的第一次寫入在 `replace()`（`helper.rs:702-714`，`Step::Verified`），之後 `/opt/pulse/pulse` 已是新版，所以必須在那時把值存下來。〔程式確認〕 |
| 3 | `UpgradeOffer` 是否有 `deny_unknown_fields` | 未確認 | 沒有 | **沒有**。`crates/kairos-proto/src/mgmt/mod.rs:778-801` 只有 `Serialize, Deserialize`；`crates/kairos-proto/src/messages.rs:4` 明文「解析時忽略未知欄位（不使用 `deny_unknown_fields`）」。0.9.x／0.10 的 Pulse 會忽略新欄位。〔程式確認（靜態）；沒有以凍結型別實際解析〕 |
| 4 | 加上 bundle 後是否仍在單行上限內 | 未提 | 上限常數未查到 | 上限是 `MAX_LINE_LENGTH = 65536` bytes（`crates/kairos-proto/src/lib.rs:114`），Pulse 以 `LinesCodec::new_with_max_length` 讀取，超過即連線錯誤（`pulse/src/network.rs:447-448`、`543`）。Kairos 送出前**只檢查欄位上限、不檢查序列化後的總長**（`engine.rs:1350`、`mgmt/mod.rs:848-893`）。最壞情況估算見第 6.3 節：約 62～63 KiB，**仍在上限內但只剩約 2.5 KiB**。〔常數與行為：程式確認；總長：推論（算術估計）〕 |
| 5 | `kairos releases import` 遇到相同 `SHA256SUMS`、不同 `.sig` | 未確認 | 未提 | **新的 `.sig` 會被驗證後丟棄，不會替換**。API 對已存在的版本回 409 `release_exists`（`kairos/src/remote/ops.rs:899-907`）；CLI 接著以 `SHA256SUMS` 內容找到同一版本，只補上發佈檔（`kairos/src/releases_cli.rs:504-544`）。要換簽章只能刪除後重新匯入，而刪除在版本是主機回滾目標時會被 409 `release_in_use` 擋下（`REMOTE.md` 第 4.1.1 節）。〔程式確認〕 |
| 6 | 套件回滾失敗時不退回 `pulse.prev` | 未提 | 提出 | **成立**。`helper.rs:931-933`：`restored` 為 `Err` 時直接 `rollback_failed`；0.9.0 相同（tag 中為第 932 行）。〔程式確認〕 |
| 7 | 回滾套件內的執行檔應與 `pulse.prev` 相同（內容綁定） | 提出（e2） | 未提 | 採用。預檢已有解開套件的程式（`helper.rs:607-631`）。〔程式確認：可重用的程式存在〕 |
| 8 | `pulse-install.sh` 驗證後應保存 `SHA256SUMS`／`.sig` 當種子 | 未提 | 提出 | 採用。腳本已驗證（`install.sh:147-171`），只是用完就丟。〔程式確認〕 |
| 9 | 本機封存（root 專屬） | 暫不採用（無法涵蓋手動 `apt install`） | 第二階段採用 | 第二階段採用，但以第一階段的 bundle 為備援，兩人的顧慮都成立（第 4 節方案 B）。 |
| 10 | 金鑰 `valid-before` 過期後封存的回滾檔 | 驗證失敗就退回 `pulse.prev` | 可用，只有從 `allowed_signers` 移除才算撤銷 | **兩者都不採用**，建議第三種：以 INV-2 綁定的目標，回滾當下的簽章狀態只記錄、不阻擋（第 5.3 節 決策點 D1）。 |
| 11 | 輔助程式也信任 `request.json` 的 `method` | 未提 | 未提 | **本 ADR 新發現**：輔助程式沒有自己偵測安裝方式，`method` 直接決定以 tar／dpkg／rpm 哪條路徑替換與回滾（`helper.rs:513`、`607-620`、`716-729`、`871`）。影響有限（仍只能安裝簽章過的發佈），但同屬「低權限輸入決定 root 行為」，併入立即修正。〔程式確認：沒有偵測；影響為推論〕 |
| 12 | 0.8.0 是否也是有效的降版目標 | 未提 | 未提 | 0.8.0 tag 已有 `packaging/sign-release.sh`，推論 0.8.0 的發佈有官方簽章；它的檔名能否通過 `ReleaseFile::parse`、能否被 0.9.0 的輔助程式接受**未確認**（第 12 節）。若可以，0.9.0 主機可被降到沒有遠端升級的 0.8.0，脫離集中管理。 |

---

## 2. 威脅模型摘要

| 攻擊者 | 現況能做到什麼 | 決策後（立即修正＋第一、二階段） |
|---|---|---|
| T1 被入侵的 Kairos／主控台帳號（`releases:write`、`rollouts:write`） | 不能讓 root 安裝未簽章的東西；回滾檔被綁在主機自身版本。可以不附回滾檔（造成套件資料庫不一致）、或故意讓健康確認失敗觸發大量回滾（影響可用性）〔程式確認／推論〕 | 相同。`rollback_bundle` 只是公開的簽章資料，加上 INV-2 的內容綁定，Kairos 能決定的只有「附不附」，不能決定「回到哪裡」 |
| T2 網路中間人（8443 或外部 URL） | 只能讓下載失敗 → 退回 `pulse.prev` | 相同 |
| T3 主機上的非特權使用者 | 無（`/var/lib/pulse/upgrade` 0750、`/opt/pulse` 屬 root）〔推論〕 | 相同 |
| **T4 被入侵的 `pulse` 服務帳號** | **可以讓 root 裝回任一有官方簽章的舊版**，繞過 `allow_downgrade = false`（第 0 節第 1 點）；可以在接續時改寫 `from_version` 讓回滾確認失敗 → `rollback_failed`；可以改 `method`；可以偽造或破壞 `status.json` 影響健康確認〔程式確認＋推論〕 | 降版與接續時的改寫被擋下（INV-1、INV-6）；即使將來又有欄位取自不可信來源，INV-2 也讓回滾套件只能是「目前安裝的那份」。**仍能**影響健康確認（殘餘風險 R1） |
| T5 外部 URL 檔案伺服器被入侵 | 只能讓下載失敗 | 相同；第二階段起回滾不依賴它 |
| T6 持有公開的舊版官方套件與簽章 | 單獨做不到；與 T4 結合即第 0 節第 1 點 | 無法利用 |
| T7 發佈金鑰外洩 | 直接推一個惡意「新版」即可，與回滾無關；`pulse.prev` 退路本來就不檢查簽章 | 相同。回滾不會擴大影響（目標只能是替換前在執行的位元組，見第 5.3 節） |
| T8 時鐘操控（讓過期金鑰重新有效） | 全域問題，不是回滾特有 | 相同；第二階段的封存不再受時間影響 |

資產：主機 root 權限、Pulse 版本完整性（不被降到有漏洞的舊版）、監控的可用性、信任錨、套件資料庫與實際檔案的一致性。

---

## 3. 決策驅動因素

| # | 因素 | 說明 |
|---|---|---|
| D1 | 不擴大 Kairos 被入侵時的能力 | `REMOTE.md` 第 1 節原則 2、第 5.1 節：Kairos 沒有私鑰，主機只接受本機信任錨驗證過的官方發佈 |
| D2 | 特權分離成立 | `pulse` 帳號的任何輸出對 root 都是不可信輸入（第 4.4.1 節）；第 5.1 節的宣稱必須真的成立 |
| D3 | 回滾的可用性 | 回滾是「最需要成功」的時刻；不因 Kairos 停機、外部 URL、金鑰輪替或時間而失效 |
| D4 | 套件資料庫一致 | 弱點掃描與合規以 `dpkg -l`／`rpm -q` 判斷版本 |
| D5 | 相容性 | 0.9.x／0.10 的 Kairos 與 Pulse 任意組合都能運作，不新增 feature |
| D6 | 跨平台一致 | 規則要能直接套用到 ADR-0001 的 Windows 版本槽，共用測試案例 |
| D7 | 成本與元件大小 | 回滾路徑上的程式要少；Windows 的啟動器要小而穩定（ADR-0001 第 4.1 節） |

---

## 4. 考慮的方案

共同前提：輔助程式不連網路（`RestrictAddressFamilies=AF_UNIX`），回滾當下能用的只有本機檔案；**立即修正項（第 10 節）不論選哪個方案都要做**，否則任何以簽章為基礎的方案都會被 `from_version` 繞過——方案 A 甚至會讓攻擊更容易（攻擊者不必自備簽章資料）。

### 方案 0：維持現狀（本機紀錄＋`pulse.prev` 退路）

- **防護範圍**：T1、T2、T3、T5；T4 擋不住（立即修正後可擋）。
- **可用性**：第一次遠端升級沒有套件回滾；金鑰輪替後失去套件回滾；外部 URL 版本庫有額外限制（`engine.rs:1265`）。
- **相容性**：無變更。**成本**：0。
- **評價**：已知限制持續存在（`CHANGELOG.md:253`、17.5 #3）。

### 方案 A：`rollback_bundle`（17.5 原建議）

Kairos 在 `upgrade_offer` 附上回滾檔所屬發佈的 `SHA256SUMS`／`.sig`；Pulse 本機紀錄優先、沒有時用 bundle；輔助程式以信任錨重新驗證，版本以自身記錄的版本綁定。

- **防護範圍**：與方案 0 相同。被入侵的 Kairos 能提供的只是一份公開的簽章資料，效果等同本機紀錄。細微風險：本機紀錄代表「本機驗證並安裝過這次發佈」，bundle 只代表「存在這樣一次官方發佈」；同一版本號若有不同 commit 的簽章建置（重新出包），Kairos 可以挑另一個。以 A2（內容綁定）消除。〔推論〕
- **可用性**：直接解決第一次遠端升級（前提是版本庫有主機目前的版本——主控台本來就建議一起匯入）。金鑰輪替問題仍在。
- **相容性**：新增選用欄位，不需要 feature（第 6 節）。
- **成本**：約 3.5～4 人日（含測試）。

#### 方案 A2：內容綁定（A 的補強）

輔助程式在預檢時解開回滾套件，確認其中 `/opt/pulse/pulse` 的 SHA-256 等於**替換前的** `/opt/pulse/pulse`（即將成為 `pulse.prev`）。不符就不用回滾檔、退回 `pulse.prev`。
- 擋下重新出包的同版本、也替將來類似 `from_version` 的錯誤提供縱深防禦。
- 單獨使用不夠：套件中執行檔以外的檔案（維護指令碼、unit）仍需要簽章保證，所以 A2 必須與 A 並用。
- 成本約 1 人日（重用 `helper.rs:607-631` 的解開程式）。

### 方案 A'：回滾**當下**再驗一次簽章

把驗證從 `prepare()` 移到（或加在）`rollback()` 前。
- 安全收益很小：回滾檔驗證後就在 root 0700 的目錄裡，能改它的人已經是 root。〔推論〕
- 可用性代價：健康確認最長 1 小時，重開機接續可能更久；這段期間跨過某把金鑰的 `valid-before` 或正好在輪替信任錨，就會在最需要回滾的時候驗不過。
- **不採用**（另見第 5.3 節 D1）。

### 方案 B：root 專屬的本機封存＋雜湊綁定

輔助程式在升級**成功**後，把剛裝好、已驗證的套件連同 `SHA256SUMS`、`.sig`、簽署者指紋、整包 SHA-256、套件內執行檔的 SHA-256、版本、commit、來源與時間，封存到 `/var/lib/pulse-upgrade/archive/<版本>-<commit>/`（root 0700），索引 `archive/index.json`（root 0600）。回滾時優先使用封存，以雜湊綁定。

- **防護範圍**：與 A＋A2 相同，另外完全不依賴 Kairos。
- **可用性**：最好：Kairos 停機、外部 URL、金鑰輪替、版本庫遺失檔案都不影響。`pulse-install.sh` 安裝時可寫入種子封存；**直接 `apt install ./x.deb` 的主機沒有種子**（dpkg 不保留 `.deb`，`postinst` 也拿不到自己的路徑），這些主機的第一次升級仍要靠方案 A——這是資安分析「暫不採用」的理由，也是本 ADR 保留 A 作為備援的理由。〔推論〕
- **磁碟**：每台主機多一個套件（數 MB 至十數 MB）；有封存時不必下載回滾檔，暫存峰值可降為 `2 × size + 64 MiB`。〔推論〕
- **相容性**：可選擇在 hello 的 `install` 加 `rollback: "archived" | "none"`（物件、忽略未知欄位，不需要 feature）讓 Kairos 的資格分類更準確。
- **成本**：約 9～11 人日（輔助程式、Pulse、Kairos 資格分類、主控台文字、安裝腳本、測試與文件）。
- **與 Windows 的關係**：ADR-0001 的「前一槽」本質上就是方案 B（寫入槽時驗證、`slot.json` 記雜湊、切回時比對）。

### 方案 C：回滾版本下限（anti-rollback floor）

C1 本機設定 `min_rollback_version`；C2 發佈清單加 `min_rollback_to`（離線金鑰保護）。
- 回滾的目標本來就是「替換前的版本」，下限只在目標可以被別人指定時才有意義；INV-1 讓目標只能是替換前版本，這已是最嚴格的下限。
- C2 可能讓合法回滾失敗（目前版本剛被宣告有漏洞、新版又沒通過健康確認 → 無處可回 → 監控停止），需要例外規則，例外規則又回到 INV-1。
- **不單獨採用**；C2 的價值在**遠端降版**（第 4.8 節），另案處理。

### 方案 D：由 Kairos 簽署回滾目標

- 被入侵的 Kairos 能簽任何東西，而這正是要防的對象；對 Kairos 簽章的信任不能高於發佈簽章，最後仍得由發佈簽章把關，Kairos 的簽章沒有增加防禦力，反而把「選哪個舊版」交給 Kairos。
- 違反「Kairos 沒有私鑰」（第 5.1 節）與「信任錨不能被遠端修改」（第 4.2.1 節）；要處理 HA 多節點共用金鑰、輪替與撤銷。約 8～12 人日加金鑰維運流程。
- **不採用**。若真的需要「指定回滾目標」，應由**離線的發佈金鑰**在清單中宣告（即 C2）。

### 比較

| | 0 現狀 | A＋A2 | A' | B（＋A 備援） | C | D |
|---|---|---|---|---|---|---|
| 擋 T4 降版（立即修正後） | ✓ | ✓（縱深） | — | ✓（縱深） | 部分 | ✗ |
| 第一次升級有套件回滾 | ✗ | ✓（版本庫有目前版本時） | — | ✓（種子或備援） | — | ✓ |
| 不依賴 Kairos／外部 URL | — | ✗ | — | ✓ | — | ✗ |
| 金鑰輪替後仍可用 | ✗ | ✗（需重簽，第 10 節 #4） | ✗ | ✓ | — | 視 Kairos 金鑰 |
| 需要新 feature | 否 | 否 | 否 | 否 | C2 否 | 是（實質上） |
| 成本（人日） | 0 | 約 4.5～5 | 0.5～1 | 約 9～11 | 0.5～2 | 8～12＋維運 |
| 可直接套用到 Windows 槽 | — | 不需要（槽不用回滾檔） | — | ✓ | — | ✗ |

---

## 5. 決策（建議）

### 5.1 分階段

| 階段 | 內容 | 時程建議 |
|---|---|---|
| **階段 0：立即修正**（第 10 節） | 輔助程式以自身版本綁定回滾版本並存入 root 的 `state.json`，接續時不再讀 `request.json` 的 `from_version`／`method`／`target`／`rollback_artifact`；`method` 以自身偵測為準；套件回滾失敗退回 `pulse.prev`；規格文件更正 | 併入 0.10.0（尚未發佈），不等本 ADR 核定 |
| **階段 1** | 方案 A（`rollback_bundle`）＋A2（內容綁定）＋`pulse-install.sh` 寫入種子紀錄＋Kairos 送出前檢查序列化長度 | 0.10.0 或下一個次版本 |
| **階段 2** | 方案 B（root 專屬封存）；A 降為「沒有封存時的備援」；hello 的 `install.rollback` 與資格分類 | 與 Windows W1／W2 同期（共用不變條件與測試） |
| 另案 | 版本庫支援為既有版本換上新金鑰的簽章（第 10 節 #4）；C2 只用於遠端降版 | 依需求 |

### 5.2 信任不變條件（與平台無關）

以下「特權元件」在 Linux 指 root 輔助程式，在 Windows 指 ADR-0001 的啟動器與輔助程式；「低權限元件」在 Linux 指以 `pulse` 帳號執行的 Pulse，在 Windows 指代理程式（目前同為 LocalSystem，見第 7.3 節）。

- **INV-1（回滾目標由特權元件自己決定）**：自動回滾的目標版本，等於特權元件**自己記錄**的「替換前實際執行的版本」——Linux 為輔助程式第一次執行時的 `CARGO_PKG_VERSION`，存在 root 專屬的狀態；Windows 為指標檔的 `active`。不得取自 `request.json`、Kairos 的提議或任何低權限元件可寫的檔案。
- **INV-2（內容綁定）**：回滾後要執行的程式，必須與替換前實際執行的位元組相同：以執行檔的 SHA-256 等於 `pulse.prev`（或 Windows 前一槽 `slot.json` 記錄的雜湊）判斷；以整包封存的回滾來源，另以整包 SHA-256 綁定。
- **INV-3（執行檔以外的特權內容需要簽章，在取得時驗證）**：回滾時以特權執行、但不在 INV-2 綁定範圍內的內容（套件維護指令碼、unit 等），必須來自以**本機信任錨**完整驗證（含 `valid-after`／`valid-before`）通過的官方發佈；驗證由特權元件在**取得時**（替換前的 `prepare`，或封存時）執行，結果與雜湊一起存在特權專屬位置。
- **INV-4（Kairos 不選擇、回滾不需要網路）**：Kairos 最多提供「證明資料」（回滾檔與 `rollback_bundle`），讓特權元件自己驗證與綁定；回滾本身不需要網路，也不需要 Kairos 在回滾當下提供任何東西。
- **INV-5（只往更保守的方向退）**：任何驗證或綁定失敗、或套件管理員執行回滾失敗，只能退到下一個更保守的來源（第 5.4 節的順序），最後是 `pulse.prev`／前一槽；**不得放寬驗證**。每次退回都在結果的 `detail` 標明原因。
- **INV-6（信任的紀錄只存在特權專屬位置）**：低權限元件可寫的紀錄（Linux 的 `/var/lib/pulse/upgrade/releases/`、`request.json`）只是提示，使用前必須由特權元件重新驗證；特權元件做過的判斷（版本、方法、雜湊、驗證結果）只存在只有特權元件可寫的位置，重開機接續時以它為準。
- **INV-7（回滾當下的簽章狀態不阻擋已綁定的目標）**：受 INV-2 綁定的回滾目標，在回滾當下**不**因金鑰過期或被移除而被拒絕；特權元件可以重新驗證並記錄結果（供診斷與主控台提示），但不據此阻擋。（決策點 D1，第 5.3 節）
- **INV-8（本機管理者安裝的版本是合法來源）**：本機管理者安裝且正在執行的版本（tar 的 `pulse.prev`、Windows 的種子槽、安裝時寫入的種子封存）是合法的回滾來源；它的信任根是「本機管理者的安裝行為」，不是簽章。
- **INV-9（自動回滾與遠端降版的界線）**：自動回滾不受降版政策限制，**正因為** INV-1 使它只能回到替換前的版本。任何回到「替換前版本以外」的動作都是遠端降版，受第 4.8 節的兩個開關限制，且必須是明確的白名單動作（第 5.5 節），不得藏在其他訊息中。本機管理者的手動回滾不受遠端降版政策限制，但仍須通過 INV-2 的完整性檢查。
- **INV-10（失敗的版本永遠不是回滾目標）**：沒有通過健康確認的版本（失敗的試用、`rolled_back` 的目標）不得成為任何自動選擇的目標，也不得被封存為回滾來源。
- **INV-11（低權限輸入只能讓結果更保守）**：低權限元件提供的任何欄位，只能讓特權元件「少做」（例如不使用回滾檔），不能讓它「多做」或「做不同的事」（例如換一個版本、換一條安裝路徑）。

### 5.3 決策點 D1：金鑰過期或被移除後，已綁定的回滾目標還能不能用

| 選項 | 內容 | 支持者 |
|---|---|---|
| (a) | 回滾當下以當下的信任錨完整重新驗證；失敗就退回 `pulse.prev` | 資安分析 |
| (b) | 只要求簽署金鑰「仍列在」`allowed_signers`，忽略 `valid-before`；移除才算撤銷 | 工程分析 |
| **(c) 建議** | 已綁定的目標只檢查完整性（INV-2）；簽章狀態只記錄、不阻擋 | 本 ADR、ADR-0001 Q1 的建議 |

**理由**：

1. **阻擋不會移除任何程式**。在 Linux，(a)／(b) 失敗時的退路是 `pulse.prev`——它和被拒絕的封存套件裡的執行檔是**同一份位元組**（INV-2）。差別只在套件資料庫是否一致，安全性沒有差異。〔推論，依 INV-2 的定義〕
2. **回滾目標就是正在執行回滾的程式**。在 Linux，輔助程式就是目前安裝的版本（`pulse-upgrade.service` 執行 `/opt/pulse/pulse`），而回滾目標正是這一版。若這一版是外洩金鑰簽的惡意版本，它在執行輔助程式時早已是 root，回滾當下的檢查保護不了任何東西。〔程式確認：`pulse/packaging/pulse-upgrade.service:19`；結論為推論〕
3. **在 Windows，(a)／(b) 會讓主機沒有可退的版本**。前一槽被拒絕時沒有別的版本可用，只能 `rollback_failed` 或停在失敗的試用版——比退回已知可用的版本更糟。而且 (a)／(b) 都要求啟動器內含 SSHSIG 驗證與 `allowed_signers` 解析，違反 ADR-0001「啟動器小而穩定」的設計（D7）。
4. **撤銷的正確作法不在回滾路徑上**：金鑰外洩時，移除金鑰會讓**新的**安裝與提議全部失敗（第 4.2.3 節）；要清除已安裝的惡意版本，靠的是推送以新金鑰簽署的乾淨版本。如果那一版的健康確認失敗而回滾到原本的版本，主機只是回到原狀，不會更糟。
5. **INV-3 仍保留撤銷能力於「取得」這一步**：從 Kairos 或本機紀錄**新取得**的回滾套件，在替換前以完整規則驗證（含 `valid-before`），失敗就不用。因此 (c) 只影響「已經在本機、已綁定」的目標。

**取捨**：(c) 放棄了「移除金鑰後，拒絕把以該金鑰簽署的套件**再執行一次維護指令碼**」這一點：第二階段的封存套件在回滾時會以 root 再跑一次舊版的 `postinst`。這些指令碼與先前安裝時執行過的完全相同（整包 SHA-256 綁定），所以新增的風險只有「同一段程式再執行一次」。若使用者認為這仍不可接受，**(b) 是可接受的折衷**（Linux 輔助程式已有解析 `allowed_signers` 的程式，成本約 0.5 人日），但 Windows 的前一槽仍建議採 (c)（理由 3）。**不建議 (a)**：它讓正常的金鑰輪替（`valid-before` 到期）也讓回滾退化。

### 5.4 回滾來源的優先順序（deb／rpm）

1. 第二階段的 root 封存（版本＝INV-1 記錄的版本；整包雜湊相符；套件內執行檔雜湊＝`pulse.prev`）。
2. 本機紀錄（`releases/<自身版本>/`）驗證通過的回滾檔，執行檔雜湊＝`pulse.prev`。
3. `rollback_bundle` 驗證通過的回滾檔，執行檔雜湊＝`pulse.prev`。
4. `pulse.prev`（`detail` 加 `binary_restored_package_db_shows_<版本>` 與退回的原因）。
5. 第 1～3 項的套件管理員命令失敗時，執行第 4 項（`detail` 另加 `package_rollback_failed`）；第 4 項也失敗才 `rollback_failed`。

第 2、3 項以同一套規則驗證，所以順序只影響「是否依賴 Kairos」；本機優先。`detail` 的字詞與代碼由設計包正式分配，本 ADR 不暫定號碼。

---

## 6. 協定與相容性

### 6.1 欄位

```jsonc
// upgrade_offer（Kairos → Pulse）新增選用欄位
"rollback_bundle": {
  "sha256sums": "<回滾檔所屬發佈的 SHA256SUMS 全文>",   // 1..=16384 bytes（沿用 MAX_SHA256SUMS_BYTES）
  "signature":  "<該發佈的 SHA256SUMS.sig 全文>"        // 1..=4096 bytes（沿用 MAX_SIGNATURE_BYTES）
}
```

- 只有在 `rollback_artifact` 存在時才允許；`validate_upgrade_offer` 檢查大小，沒有 `rollback_artifact` 卻有 bundle 時拒絕（`PLS-E8402`）。
- 型別：`#[serde(default, skip_serializing_if = "Option::is_none")] pub rollback_bundle: Option<RollbackBundle>`。
- 不帶清單（manifest）：回滾只需要確認「檔名與雜湊列在該次發佈的 `SHA256SUMS` 中、版本等於自身記錄的版本」，0.9.0 以前的發佈也沒有清單。
- Kairos 只對 `install.method` 為 `deb`／`rpm` 的主機附上（ADR-0001 的 `slot` 與 `tar` 不附；Pulse 收到也忽略）。Kairos 版本庫已存有每個版本的 `sha256sums`／`signature`（`engine.rs:1327-1328` 直接使用），**不需要改 schema**。〔程式確認〕
- 不需要 `request.json` 的 schema 變更：Pulse 仍把採用的簽章資料寫成暫存目錄的 `rollback.SHA256SUMS(.sig)`（`files.rs:35-37`），輔助程式不需要知道它來自本機紀錄還是 bundle。

### 6.2 是否需要新的 feature

**不需要**。`UpgradeOffer` 沒有 `deny_unknown_fields`，協定明文規定忽略未知欄位（第 1.5 節第 3 列）；這與 `REMOTE.md` 第 17.7 節新增外部 URL 時的處理一致。

### 6.3 單行上限

`MAX_LINE_LENGTH = 65536` bytes。以各欄位上限估算最壞情況〔推論：算術估計，未實際序列化〕：`sha256sums` 16 KiB＋`signature` 4 KiB＋`manifest.content` 8 KiB（內容是 JSON 字串，引號跳脫後最多約 16 KiB）＋兩個 URL 各 2 KiB＋bundle 20 KiB＋換行跳脫與其他欄位約 1.5 KiB ≈ **62～63 KiB**，在上限內但餘裕只有約 2.5 KiB。實際的發佈（`SHA256SUMS` 約 20 行、數 KB）遠小於此。超過上限時 Pulse 會把它當成連線錯誤（`network.rs:447-448`），所以：

- **Kairos 在送出前計算序列化長度**，超過 `MAX_LINE_LENGTH` 時**省略 `rollback_bundle`** 並記 `WARN`（主機退回本機紀錄或 `pulse.prev`），不得送出超長的行。
- 新增單元測試：所有欄位取上限時的序列化長度（比照 `crates/kairos-proto/src/metrics_spec_tests.rs:447`）。

### 6.4 版本組合

「新版」指含階段 1 的版本（暫稱 N；可能就是 0.10.0）。

| Kairos ＼ Pulse | 0.9.x | 0.10.0（不含本 ADR） | N |
|---|---|---|---|
| 0.9.x | 現況 | 現況 | 收不到 bundle → 本機紀錄或 `pulse.prev`；**仍有** INV-1、INV-2 與套件回滾失敗的退路 |
| 0.10.0（不含本 ADR） | 現況 | 現況 | 同上 |
| N | 忽略 bundle，行為同現況；**仍有**第 0 節第 1 點的缺陷 | 同左 | 第一次遠端升級即可套件回滾 |

- 第 0 節第 1 點的缺陷要等主機**執行**含修正的版本後才關閉：執行升級的輔助程式是**目前安裝的那一版**，所以 0.9.0 主機升級到 N 的那一次，仍由 0.9.0 的輔助程式執行。〔程式確認：`pulse-upgrade.service:19`〕
- 第二階段的 `install.rollback` 是 hello `install` 物件的新欄位，舊 Kairos 忽略（`REMOTE.md` 第 2.1 節），不需要 feature。

---

## 7. 與 ADR-0001（Windows OTA 式版本槽）的關係

### 7.1 對 ADR-0001 第 4.11 節 Q1～Q6 的回答

| # | 問題 | 結論 | 理由 |
|---|---|---|---|
| Q1 | 回滾當下要不要以**目前的**信任錨重新驗證簽章？ | **不要**。前一槽只檢查完整性（ADR-0001 的 V5：`slot.json` 與 `pulse.exe` 的 SHA-256）；簽章重新驗證的結果只記錄、不阻擋（INV-7）。**啟動器不需要內含簽章驗證**，ADR-0001 第 4.1 節不必調整。 | 第 5.3 節理由 1～4：前一槽就是替換前在執行的位元組，拒絕它不會移除任何程式，只會讓主機沒有可退的版本。若需要記錄簽章狀態，由**輔助程式**（已含驗證程式、隨 OTA 更新）在下一次升級的 `prepare` 或 `pulse launcher status` 時做，不放在啟動器。 |
| Q2 | 由 MSI／zip 安裝、沒經過 `allowed_signers` 驗證的**種子槽**能否當回滾目標？ | **可以**（INV-8）。 | 它是本機管理者安裝並正在執行的版本，與 Linux tar 的 `pulse.prev`（同樣不檢查簽章）同一類。條件：它是升級前的 `active`（INV-1），且 `slot.json` 的雜湊相符（INV-2）。`slot.json` 應記錄 `source: "seed"`，供診斷與主控台顯示。 |
| Q3 | Linux 與 Windows 的「已驗證發佈的本機紀錄」是否統一格式？鍵用什麼？ | **統一內容格式，不統一位置**；鍵用 (`version`, `commit`, 發佈檔 SHA-256)，另記綁定用的執行檔 SHA-256。 | 內容：`SHA256SUMS`、`SHA256SUMS.sig`、清單（有的話）＋一個 `record.json`（`schema`、`version`、`commit`、`artifact`、`artifact_sha256`、`binary_sha256`、`signer_fingerprint`、`verified_at`、`source: ota｜seed｜install`）。位置依平台與 INV-6：Windows 的 `slots\<槽>\release\` 在特權目錄中，屬於「信任的紀錄」；Linux 的 `/var/lib/pulse/upgrade/releases/` 屬於服務帳號，只是提示，信任的紀錄是第二階段的 `/var/lib/pulse-upgrade/archive/`。只用 (版本, commit) 不足以區分同一 commit 的不同格式／平台發佈檔；只用發佈檔雜湊又不便人工查詢，所以三者並用。共用的讀寫與驗證程式放在 `kairos-proto` 的 `release` 模組。 |
| Q4 | `rollback_bundle` 是否只對 deb／rpm 送？ | **是**。Kairos 對 `tar`、`slot`（以及舊式的 `msi`／`zip`，它們本來就是 `unsupported`）不送；Pulse 收到也忽略。 | 只有需要讓套件資料庫一致的安裝方式才需要回滾檔；槽與 tar 的回滾來源都在本機（INV-4）。 |
| Q5 | 是否允許 Kairos 遠端要求「切回前一槽」？ | **第一版不提供**。若日後提供，它是**遠端降版**：必須同時符合第 4.8 節的兩個開關、是新的白名單動作（第 5.5 節），而且只能切到特權元件記錄的 `previous`（INV-1、INV-9、INV-10）。 | 自動回滾不受降版政策限制的前提是「只能回到替換前的版本」；由 Kairos 觸發的切換已不滿足這個前提。 |
| Q6 | 提交後 `previous` 槽保留多久？手動回滾是否受版本政策限制？ | **保留到下一次升級提交成功為止**（磁碟上最多 `active`＋`previous`＋升級中的 `trial`）。**本機管理者的手動回滾不受遠端降版政策限制**，但仍須通過完整性檢查（INV-9）。 | 與 ADR-0001 的建議相同。手動回滾的人已有系統管理員權限，降版政策是為了限制**遠端**的能力。Linux 第二階段的封存同樣預設保留 1 份（目前版本）。 |

### 7.2 ADR-0001 必須遵守的條款

1. **INV-1**：自動切回的目標只能是指標檔的 `active`（替換前最後一個確認過的槽）；不讀 `request.json`、`health.json` 或代理程式寫的任何檔案來決定目標。
2. **INV-2**：每次啟動任何槽（含切回）前比對 `slot.json` 記錄的 `pulse.exe` SHA-256（ADR-0001 的 V5 已涵蓋）。
3. **INV-3**：OTA 建立的槽，在建槽（V3）時以本機信任錨完整驗證，`release\` 與 `slot.json` 在封存前寫好；切回時不重新驗證簽章。
4. **INV-4**：切回不需要網路，不接受 `rollback_artifact`／`rollback_bundle`（ADR-0001 第 4.11 節已寫明）。
5. **INV-5**：`slot.json` 不完整或雜湊不符時，只能退到下一個更保守的來源（`current.json.bak` → 掃描 → 啟動器自身重建種子槽），並記錄原因。
6. **INV-6**：`slots\`、`current.json`、`slot.json`、`work\` 的狀態只由特權元件寫入；位置須通過 ADR-0001 第 4.5 節的 ACL 檢查。
7. **INV-10**：失敗的試用槽不得被任何自動選擇（含第 4.3 節的「掃描」退路）選中。
8. **INV-9**：遠端「切回前一槽」若日後提供，依第 7.1 節 Q5。

### 7.3 與 ADR-0001 的衝突點與建議修改（不直接修改 ADR-0001）

1. **第 4.3 節的掃描退路與 INV-10、INV-1**：「選擇 `slot.json` 完整、雜湊相符、`confirmed: true` 的槽中**版本最高**的」有兩個問題：（a）`confirmed` 寫在 `slot.json`，而提交寫在 `current.json`，兩者不是同一次寫入，必須保證 `confirmed: true` **只在提交之後**寫入、且失敗的試用槽永遠不會是 `true`；（b）管理者手動回滾到較舊的槽後，若兩個指標檔都損毀，「版本最高」會把主機切回管理者剛退掉的版本。**建議**：`slot.json` 另記 `confirmed_at`（提交時間），掃描時選「最近一次被確認」的槽而不是「版本最高」；手動回滾時把被退掉的槽的 `confirmed` 清除。
2. **Windows 沒有特權分離，INV-1／INV-6／INV-11 的防護在 Windows 只是縱深**：代理程式與輔助程式都是 LocalSystem（ADR-0001 第 4.5 節），所以 T4（`pulse` 帳號被入侵）在 Windows 等於整台主機被入侵。這不是衝突，但 ADR-0001 應明寫：這些不變條件仍要實作（日後採方案 E3 降權時不必重新設計），而且**啟動器**作為唯一不隨 OTA 更新的元件，不得讀取代理程式可寫的檔案來決定切換目標（第 4.6 節已對 `status.json` 這樣做，應擴及 `request.json` 的所有欄位）。
3. **第 4.4 節第 3 步「啟動器版本高於 `active` 時建立種子槽並切換，不經健康確認」**：這不違反本 ADR（INV-8：本機管理者的安裝行為），但切換後原本的 `active` 成為 `previous`；若新種子槽不健康，ADR-0001 應明定是否以同樣的健康確認與自動切回處理。**建議**：種子切換也走 `trial` 與健康確認，失敗切回原本的 `active`，讓「本機安裝的新版不健康」也有一致的保護。
4. **第 4.11 節第 1 點「這與 Linux 的 `<狀態目錄>/upgrade/releases/<版本>/` 是同一種資料」**：內容相同，但**信任等級不同**（Linux 的那份屬於服務帳號、只是提示）。建議改寫為「與 Linux 第二階段的 root 封存同一種資料（第 7.1 節 Q3 的格式）」。
5. **第 4.6 節「試用槽保留到下一次升級或 7 天後清除（供診斷）」**：與 INV-10 不衝突，但建議在 `slot.json` 標記 `failed_at`，讓所有自動選擇明確排除它。

---

## 8. 後果

### 8.1 正面

- 第 5.1 節「`pulse` 帳號被入侵……受降版政策限制」的宣稱真正成立。
- deb／rpm 主機第一次遠端升級即可套件回滾（版本庫有目前版本，或以 `pulse-install.sh` 安裝），消除 17.5 #3 的套件資料庫不一致。
- 回滾的意義收斂為「回到替換前正在執行的那一份」，與 tar、Windows 槽的語意一致；三種平台共用一組不變條件與測試。
- 不新增 feature、不改 Kairos 的資料庫 schema、Kairos 仍不持有私鑰。
- 套件回滾失敗不再直接 `rollback_failed`，減少需要到現場處理的情況。

### 8.2 負面

- 第一階段多一次解開回滾套件（預檢時間與暫存空間，空間已在 `3 × size + 64 MiB` 中）。
- 第二階段每台 deb／rpm 主機多一個套件的磁碟用量，多一個需要清理與診斷的目錄。
- `upgrade_offer` 的最壞長度逼近單行上限，Kairos 必須做長度檢查與省略。
- INV-7 讓「移除金鑰」不影響已封存的回滾目標，需要在文件中說清楚撤銷的正確作法（推送新版）。

### 8.3 殘餘風險

| # | 風險 | 嚴重度 | 處理 |
|---|---|---|---|
| R1 | `pulse` 帳號被入侵時仍能影響健康確認（偽造 `status.json` 或製造不健康） | 中（可用性；完整性仍受簽章保護） | 接受並明列；另案以 root 可見的 systemd 屬性強化 |
| R2 | 0.9.0（與不含修正的 0.10.0）主機在升級到含修正版本之前仍有降版缺陷 | 中 | CHANGELOG 安全事項；建議盡快推送；不需要提高 `min_upgrade_from` |
| R3 | 金鑰輪替後，第一階段的 bundle 與本機紀錄驗證不過 → 退回 `pulse.prev` | 低（只影響套件資料庫一致性） | 第二階段封存；版本庫支援重簽（第 10 節 #4） |
| R4 | 時鐘操控使過期金鑰重新有效 | 低～中，全域問題 | 既有的時鐘偏差提示；不在本 ADR 範圍 |
| R5 | 回滾時以 root 執行舊版套件的維護指令碼 | 低 | 受簽章（取得時）與整包雜湊保護；與客戶手動安裝同一檔案相同 |
| R6 | 直接 `apt install ./x.deb`、版本庫又沒有目前版本的主機，第一次升級仍無套件回滾 | 低 | 主控台預覽照舊標示「無法自動回滾」；提供 `pulse upgrade archive-current`（第二階段） |
| R7 | 發佈金鑰外洩（T7） | 高，但與回滾無關 | 既有對策（移除金鑰、以新金鑰推送乾淨版本）；回滾不擴大影響 |

---

## 9. 實作計畫

人日為推估（以現有程式結構與 `REMOTE.md` 第 13 節的估工粒度），含單元測試，不含 kp-build 上的執行時間。

### 9.1 階段 0：立即修正（約 2.5～3 人日）

| 項目 | 內容 | 人日 |
|---|---|---|
| 0-1 | `HelperState` 新增 `from_version`、`method`、`target_version`、`rollback_artifact`（`#[serde(default)]`）。`prepare()` 要求 `req.from_version == self.paths.current_version`，不符時 `failed`（`PLS-E8402`）；回滾檔的 `ArtifactExpectation.version` 改用 `self.paths.current_version`。`replace()` 在第一次 `save` 時寫入上述值；`after_replace`、`rollback`、`confirm_rollback`、`result()` 一律使用 `state` 中的值。接續時 `request.json` 的這些欄位與 `state` 不同 → 記 `WARN`、以 `state` 為準 | 1.0 |
| 0-2 | `method`：輔助程式以 `remote/install.rs` 的偵測判斷安裝方式，與 `req.method` 不同時 `failed`（`PLS-E8402`） | 0.25～0.5 |
| 0-3 | `rollback()`：套件管理員回滾失敗 → 以 `pulse.prev` 換回，`notes` 加 `package_rollback_failed` 與 `binary_restored_package_db_shows_<版本>`；仍失敗才 `rollback_failed` | 0.5 |
| 0-4 | 文件：`REMOTE.md` 第 4.5.3 節改為實際行為；第 5.1 節補上不變條件；第 17.3 節記錄修正；CHANGELOG 的安全事項 | 0.25 |
| 0-5 | `tests/systemd` 情境（見 9.4） | 0.5～1 |

單元測試（`pulse/src/remote/upgrade/helper/tests.rs`）：
- **重現缺陷**：`request.json` 的 `from_version` 為較舊的官方版本、`rollback.SHA256SUMS(.sig)` 為該版本的有效簽章、目標為「重新安裝目前版本」→ 修正前：回滾檔被採用、回滾時以 `DpkgInstall` 裝回舊版（以假的 `System` 記錄的命令斷言）；修正後：`failed`（`PLS-E8402`），沒有任何替換。
- 接續 `Replaced`／`RollingBack` 時把 `request.json` 的 `from_version`、`method` 改掉 → 仍以 `state.json` 的值確認回滾，結果為 `rolled_back` 而不是 `rollback_failed`。
- `method` 與偵測結果不符 → `failed`。
- 假套件管理員的回滾命令失敗 → 以 `pulse.prev` 回復、`rolled_back`、`detail` 含 `package_rollback_failed`。
- 舊格式的 `state.json`（沒有新欄位）接續時的行為：沿用 `request.json` 的值並記 `WARN`（只發生在升級到含修正版本的那一次，見 R2）。

### 9.2 階段 1（約 5～6 人日）

| 元件 | 修改 | 人日 |
|---|---|---|
| `kairos-proto` | `RollbackBundle` 型別、`validate_upgrade_offer` 的大小與「需有 `rollback_artifact`」檢查、上限總長測試、凍結 0.9 型別解析新訊息的測試、`PROTOCOL.md` 測試向量 | 0.5 |
| Kairos | `engine.rs:1263-1275` 找到 `cur` 時附 `cur.sha256sums`／`cur.signature`（只對 deb／rpm）；送出前的序列化長度檢查與省略 | 0.5 |
| Pulse | `rollback_is_trusted` 的來源依序為本機紀錄 → bundle；採用 bundle 時寫成暫存的 `rollback.SHA256SUMS(.sig)`，並另存 `releases/<自身版本>/`（仍只是提示） | 1.0 |
| 輔助程式 | 內容綁定：預檢時另外解開回滾套件（`DebExtract`／`RpmExtract` 到另一個目錄），比對 `opt/pulse/pulse` 的 SHA-256 與替換前的 `/opt/pulse/pulse`；不符 → 不用回滾檔、`notes` 加 `rollback_binary_mismatch` | 1.0 |
| 打包 | `pulse-install.sh` 在 `install.sh:171` 驗證之後，把 `SHA256SUMS(.sig)` 以 `pulse:pulse`、0640 放到 `/var/lib/pulse/upgrade/releases/<版本>/`（目錄 0750；版本從套件名解析） | 0.5 |
| 測試＋文件 | 見 9.4；`REMOTE.md` 第 2.4.1、4.4.2、17.3、17.5 節 | 1.5～2 |

### 9.3 階段 2（約 9～11 人日；建議與 Windows W1／W2 同期）

輔助程式的封存與索引（root 0700／0600，`O_NOFOLLOW`）、清理（保留 N 份、永不刪除目前版本）、回滾優先使用封存；Pulse 有封存時不下載回滾檔；hello `install.rollback`；Kairos 資格分類與主控台文字；`pulse-install.sh` 寫入種子封存；`pulse upgrade archive-current --from <目錄>`（root，驗證後封存，給 tar 或手動安裝的主機）；`pulse diag` 收集 `index.json`。第 7.1 節 Q3 的共用紀錄格式在此階段實作，Windows 的 `slot.json`／`release\` 使用同一套程式。

### 9.4 測試情境

**單元測試**（除 9.1 外）
- Pulse：本機紀錄 → bundle → 都沒有的優先順序；bundle 簽章錯誤、金鑰不在信任錨中、`valid-before` 已過、回滾檔不在其中、版本不等於自身版本 → 不下載；bundle 可信時寫成 `releases/<自身版本>/`，`prune` 後仍保留。
- 輔助程式：回滾套件內執行檔雜湊與 `/opt/pulse/pulse` 不同 → 退回 `pulse.prev`；（階段 2）封存雜湊不符、索引被換成符號連結、金鑰已移除或過期仍可回滾（D1 採 (c) 時）、清理不刪目前版本、失敗的版本不被封存（INV-10）。
- Kairos：有／沒有目前版本時的 bundle；外部 URL 組合；超過單行上限時省略 bundle；`tar`／`slot` 不附。

**e2e（`tests/e2e`）**
- `remote.rs`：新 Kairos 的 bundle 能通過 Pulse 的驗證，第一次升級失敗時以套件回滾（以假套件管理員）。
- `remote_interop.rs`／`compat.rs`：0.9 型別的 Pulse 收到含 bundle 的提議照舊流程；新 Pulse 連 0.9 型別的 Kairos 沒有 bundle 也能升級。

**`tests/systemd`（kp-build）**
| 情境 | 內容 | 預期 |
|---|---|---|
| `forged-request`（新增，**重現缺陷**） | deb／rpm 主機在 test.3；以 `sudo -u pulse` 手寫 `/var/lib/pulse/upgrade/<新 UUID>/`：目標＝test.3（重新安裝）的檔案與簽章、`rollback.SHA256SUMS(.sig)`＝test.1 的簽章資料、回滾檔＝test.1 的套件；`request.json` 的 `from_version = "0.9.1-test.1"`；替換後以 `pulse` 身分結束自己的行程觸發回滾 | 修正前：`dpkg-query`／`rpm -q` 與 `--version` 變成 test.1、`rolled_back`（記錄為已知缺陷的重現）；修正後：`failed`（`PLS-E8402`），版本仍是 test.3 |
| `forged-resume`（新增） | 在 `replaced` 期間以 `pulse` 身分改寫 `request.json` 的 `from_version`、`method`，再讓新版崩潰 | 修正後：`rolled_back`，不是 `rollback_failed` |
| `pkg-rollback-fails`（新增） | 失敗版本的 `prerm` 故意失敗 | 以 `pulse.prev` 回復、`rolled_back`、`detail` 含 `package_rollback_failed` |
| b0 改預期 | 0.9.x → test.2（崩潰），版本庫有目前版本 | 以 bundle 經套件管理員回滾，套件資料庫回到原版本，`detail` 沒有 `binary_restored` |
| `b0-seed`（新增） | `pulse-install.sh` 安裝、版本庫**不匯入**目前版本 | 階段 1：Kairos 不附回滾檔 → 仍是 `pulse.prev`（種子紀錄存在但沒有回滾檔）；階段 2：以種子封存回滾 |
| `b0-nobundle`（保留現況的回歸） | 版本庫沒有目前版本 | `pulse.prev`、`binary_restored_…` |
| `binary-mismatch`（新增） | 手動以不同建置換掉 `/opt/pulse/pulse` 後推送 | 回滾檔不被採用、`rollback_binary_mismatch`、`pulse.prev` 回復 |
| `key-rotation`（新增） | K1 簽目前版本、K2 簽目標；`allowed_signers` 中 K1 的 `valid-before` 已過 | 階段 1：退回 `pulse.prev`；階段 2＋D1(c)：以封存回滾 |
| c2 重跑 | 回滾確認期間重新開機 | 階段 2 下仍正確接續 |

註：階段 1 的 `b0-seed` 在版本庫沒有目前版本時仍無套件可裝——種子**紀錄**只解決「信任」，不解決「檔案」；檔案要等階段 2 的種子**封存**。

---

## 10. 立即修正項（不待本 ADR 核定）

| # | 缺陷 | 查證結果 | 嚴重度 | 影響已發佈的 0.9.0？ |
|---|---|---|---|---|
| 1 | 輔助程式以 `request.json` 的 `from_version` 驗證回滾檔（`helper.rs:572`），`Request::validate` 不檢查（`files.rs:89-115`）；被入侵的 `pulse` 帳號可讓 root 裝回有官方簽章的舊版，繞過 `allow_downgrade = false` | 〔程式確認〕缺少檢查；攻擊路徑的每個判斷點都讀過（目標以「重新安裝」通過 `check_version_policy`，`release/mod.rs:800-801`；回滾確認也用 `from_version`，`helper.rs:953`）；端到端未實測 | **中**（需先取得 `pulse` 帳號，是特權分離防線上的缺口；若舊版有可遠端利用的漏洞，實際風險更高） | **是**。`pulse-v0.9.0` 與 HEAD 的 `helper.rs`、`files.rs` 無差異；0.10.0 尚未發佈，修正應併入 |
| 2 | 接續時重新讀取 `request.json`，`from_version`、`method`、`target.version`、`rollback_artifact` 都來自服務帳號可寫的檔案（`helper.rs:305`、`372-396`、`953`）；`pulse` 帳號可在接續時改寫使回滾確認失敗（`rollback_failed`） | 〔程式確認〕 | 低～中（可用性；需先取得 `pulse` 帳號） | 是 |
| 3 | 輔助程式不自行偵測安裝方式，`req.method` 決定替換與回滾的路徑（`helper.rs:513`、`607-620`、`716-729`、`871`） | 〔程式確認〕；可利用性〔推論〕：仍只能安裝簽章過的發佈，但可讓 deb 主機走 tar 路徑或使回滾走錯路徑 | 低 | 是 |
| 4 | 套件回滾失敗直接 `rollback_failed`，不退回 `pulse.prev`（`helper.rs:931-933`） | 〔程式確認〕；觸發條件（失敗版本的 `prerm` 出錯等）為〔推論〕 | 低～中（可用性：監控停止、需到現場） | 是（0.9.0 為第 932 行，內容相同） |
| 5 | Kairos 版本庫無法以新簽章替換既有版本的 `.sig`（API 409 `release_exists`，`ops.rs:899-907`；CLI 只補發佈檔，`releases_cli.rs:504-544`），與 `RELEASING.md:121`「金鑰外洩時以新金鑰重新簽署仍在支援期內的發佈」衝突 | 〔程式確認〕 | 低（流程缺口；金鑰外洩時才會遇到，影響是舊版主機無法驗證、回滾退化） | 是（Kairos 0.9.0；不是 Pulse） |
| 6 | 規格與實作不一致：`REMOTE.md` 第 4.5.3 節（沒有回滾檔時 `failed`、不回復）與實作（`pulse.prev`）不同；第 5.1 節的降版宣稱因 #1 不成立 | 〔程式確認〕 | 文件 | 是（0.9.0 的文件） |

#1～#3 建議同一個修改完成（第 9.1 節 0-1、0-2）。#5 可以排在階段 1 之後：新增「同一份 `SHA256SUMS`、新的有效簽章 → 替換 `signature`／`signer_*` 並記稽核事件」的 API 與 `kairos releases resign`，約 1～1.5 人日。

---

## 11. 待使用者決定的參數

| # | 參數 | 選項 | 建議 |
|---|---|---|---|
| P1 | 立即修正項 #1～#4 的發佈方式 | 併入 0.10.0／另發 0.9.1／兩者 | **併入 0.10.0**，CHANGELOG 列為安全修正。只有在 0.10.0 會延後很久時才另發 0.9.1（注意：0.9.0 主機升級到修正版的那一次仍由 0.9.0 的輔助程式執行，見 R2） |
| P2 | 金鑰 `valid-before` 過期或被移除後，已綁定的回滾目標能否使用（D1） | (a) 完整重驗、失敗退回／(b) 金鑰仍列在信任錨即可／(c) 只檢查完整性、簽章只記錄 | **(c)**；若要保留「移除金鑰即停止使用封存套件」，Linux 可採 (b)，但 Windows 前一槽仍採 (c)。不建議 (a) |
| P3 | 本機封存保留幾版（階段 2）；Windows 槽 | 1／2／可設定 | Linux **1**（目前版本，第一版不開放設定）；Windows `active`＋`previous`（ADR-0001） |
| P4 | 是否設回滾下限 | 不設／本機 `min_rollback_version`／清單 `min_rollback_to` | **不設**；INV-1 已是最嚴格的下限。`min_rollback_to` 的概念若需要，只用於遠端降版 |
| P5 | 安裝時是否寫入種子紀錄 | 是／否 | **是**：階段 1 寫本機紀錄（提示）；階段 2 寫 root 封存。MSI／zip 由啟動器建立種子槽（ADR-0001） |
| P6 | 本機紀錄與 bundle 的優先順序 | 本機優先／提議優先 | **本機優先**（第 5.4 節）；兩者同規則驗證，且都受 INV-2 綁定 |
| P7 | 階段 2 的時程 | 與 Windows W1／W2 同期／先於 Windows／不做 | **與 Windows 同期**，共用紀錄格式與測試 |
| P8 | 是否提供版本庫的重簽（立即修正項 #5） | 是／否 | **是**，排在階段 1 之後 |
| P9 | 是否在 hello `install` 宣告 `rollback`（階段 2） | 是／否 | **是**（不需要 feature，讓推送預覽更準確） |

---

## 12. 未確認事項

1. **第 0 節第 1 點的端到端可利用性**：每個判斷點都讀過程式，但沒有實際執行；以 `tests/systemd` 的 `forged-request` 情境確認（第 9.4 節）。
2. **0.8.0 是否可作為降版目標**：0.8.0 tag 已有 `packaging/sign-release.sh`，推論其發佈有官方簽章；0.8.0 的發佈檔名能否通過 0.9.0 的 `ReleaseFile::parse`、其 deb／rpm 能否被接受，未查證。
3. **舊版 Pulse 實際解析含 bundle 的訊息**：只做靜態確認（沒有 `deny_unknown_fields`、規格明文忽略未知欄位）；以凍結型別的單元測試確認。
4. **`upgrade_offer` 最壞情況的序列化長度**：第 6.3 節是算術估計，`manifest.content` 的跳脫比例依實際內容而定；以上限測試確認。
5. **deb／rpm 安裝後的 `/opt/pulse/pulse` 與套件內的位元組完全相同**：依套件管理員一般行為推論成立（不經 strip、prelink 等改寫）；`binary-mismatch` 與 b0 情境會順帶驗證。
6. **dpkg 降版時執行失敗版本的 `prerm` 而使套件回滾失敗**：依 dpkg 一般行為推論，未實測。
7. **磁碟用量與人日**：皆為推估。
8. **Kairos 主控台「無法自動回滾」的資格分類位置**（`kairos/src/remote/rollout.rs:617-691`）：引用自分析師，本 ADR 未逐行重核。
9. **Windows 相關的一切**（ADR-0001 的槽、啟動器、ACL）：尚未實作，均為推論。

---

## 13. 參考資料

### 程式（路徑相對於儲存庫根目錄）

- `pulse/src/remote/upgrade/helper.rs`：`Paths` 與 `current_version`（152-181）；`apply` 每次重讀 `request.json`（304-340）；接續（372-396）；`prepare` 的複製（418-452）、目標驗證（474-533）、版本政策（534-554）、回滾檔驗證與第 572 行的 `req.from_version`（555-587）、預檢的解開與 `--version`（595-669）；`replace` 與 `pulse.prev`、`rollback.pkg`、第一次 `save`（688-759）；回滾（866-947，套件回滾失敗 931-933）；確認回滾（949-975）；`finish`（1094-1135）；`locate_binary`（1139-1163）。
- `pulse/src/remote/upgrade/files.rs`：`Request`（39-61）、`read_untrusted`／`validate`（78-116）、不可信檔案的讀取（146-221）、`ROLLBACK_*`（35-37）。
- `pulse/src/remote/upgrade/mod.rs`：回滾檔判斷（1241-1254）、`rollback_is_trusted`（1305-1340）、暫存與本機紀錄（1647-1681）、`prune_releases`（1712-1724）、`write_request`（1748-1776）。
- `pulse/src/remote/upgrade/helper/tests.rs`：既有的回滾測試（451、581、611、792 起）。
- `crates/kairos-proto/src/release/mod.rs`：`TrustPolicy`／`verify_bundle`（616-685）、`check_artifact`（700-753）、`check_version_policy`（793-825，相同版本為 `Reinstall`：800-801）。
- `crates/kairos-proto/src/mgmt/mod.rs`：上限常數（661-690）、`UpgradeOffer`（777-801）、`validate_upgrade_offer`（848-893）。
- `crates/kairos-proto/src/messages.rs:4`（忽略未知欄位）；`crates/kairos-proto/src/lib.rs:114`（`MAX_LINE_LENGTH`）；`crates/kairos-proto/src/metrics_spec_tests.rs:447`（長度測試的先例）。
- `pulse/src/network.rs:447-448`、`543`（超長行為連線錯誤）。
- `kairos/src/remote/engine.rs:1253-1353`（組提議、回滾檔選擇、`validate_upgrade_offer`）。
- `kairos/src/remote/ops.rs:899-907`（`release_exists`）；`kairos/src/releases_cli.rs:504-544`（相同 `SHA256SUMS` 只補發佈檔）。
- `pulse/packaging/install.sh:147-171`（安裝時驗證簽章）；`pulse/packaging/pulse-upgrade.service:19-24`；`pulse/packaging/pulse.service:65-70`、`86`。
- Git：`pulse-v0.9.0` tag 與 HEAD 的 `pulse/src/remote/upgrade/helper.rs`、`files.rs` 無差異（`git diff pulse-v0.9.0 HEAD --stat`）；`pulse-v0.8.0` tag 含 `packaging/sign-release.sh`。

### 文件

- `crates/kairos-proto/REMOTE.md`：第 4.2（388-410）、4.4.1～4.4.3（424-455）、4.5（472-492）、4.8（585-591）、5.1（610-622，第 618 行）、12.3（1015-1032）、17.2 #14、#20、#22（1220、1226、1228）、17.3 #1、#6（1234、1239）、17.5（1252-1256）、17.7（1272-1279）、18（1281-1330）節。
- `tests/systemd/README.md:67-89`（情境 b0～d 與未涵蓋項目）。
- `CHANGELOG.md:168`、`253`（0.9.0 的回滾限制）；`RELEASING.md:120-121`（金鑰輪替與外洩）。
- `docs/adr/0001-windows-ota-upgrade.md` 第 4.1、4.3～4.8、4.11 節。
- 分析筆記（本 ADR 的輸入，不在儲存庫中）：資安威脅分析、工程與維運分析（2026-11-12）。
