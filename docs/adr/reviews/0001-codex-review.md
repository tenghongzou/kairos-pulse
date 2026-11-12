# ADR-0001 的外部審查（codex，2026-11-12）

> 審查對象：commit 4eb7086 的版本。唯讀審查，未執行建置或測試。審查意見尚未併入 ADR。

## 1. 總評

建議：**修改後核定**。A/B 版本槽加常駐啟動器，比原本的就地 MSI／zip 替換更符合可回復目標，協定也能維持向下相容；但目前的指標損毀退路、MSI 種子直接切換、SCM 失敗復原敘述及權限隔離仍有會破壞安全保證的缺口。尤其 ADR 宣稱「任何時刻一定有可啟動版本」過強：啟動器仍是單點，而且候選代理程式與啟動器同為 LocalSystem，代理程式可以改寫整個信任與回滾區域。ADR-0003 提出的四點修改都應採納，其中掃描與種子切換兩點應列為核定前必要修改。

## 2. 重大問題

### 2.1 SCM 並不是「一天最多重新啟動 3 次」

- **位置**：ADR-0001 §1.2、§4.4、§6 S1；現有 CHANGELOG 也有相同敘述。
- **問題**：目前程式設定三個 `Restart` 動作、重設期 86400 秒；但 SCM 在失敗次數超過動作陣列長度後，會**重複最後一個動作**，不是停止。因此實際效果可能是在一天內每 10 秒無限重啟，而非最多三次。這會改變啟動器、`rollback_failed`、壞掉的 `active` 槽及支援現場處理的風險評估。
- **證據**：
  - 三個 Restart、10 秒延遲、86400 秒重設：[pulse/src/winsvc.rs:255](</F:/sync-project/kp-s11-adr/pulse/src/winsvc.rs:255>)。
  - `ensure_failure_actions()` 只檢查「已有任何 action」，不驗證完整內容，也不補設既有服務的 non-crash flag：[pulse/src/winsvc.rs:272](</F:/sync-project/kp-s11-adr/pulse/src/winsvc.rs:272>)。
  - Microsoft 規定 N 大於 action 數量時重複最後一項：[SERVICE_FAILURE_ACTIONSW](https://learn.microsoft.com/en-us/windows/win32/api/winsvc/ns-winsvc-service_failure_actionsw)。
  - non-crash flag 要到「下次系統啟動」才生效，ADR 將此標為待驗證是正確的：[SERVICE_FAILURE_ACTIONS_FLAG](https://learn.microsoft.com/en-us/windows/win32/api/winsvc/ns-winsvc-service_failure_actions_flag)。
- **建議修改**：若政策真的是最多三次，應設定 `Restart, Restart, Restart, None`，並測試第四次及重開機後的行為；`ensure_failure_actions()` 必須比較完整設定並分別查驗 flag。ADR、CHANGELOG 與測試通過條件一併更正。

### 2.2 指標檔損毀後「選最高版本」會重新選到不該啟動的槽

- **位置**：ADR-0001 §4.3、§4.6；ADR-0003 §7.2～7.3。
- **問題**：當 `current.json` 與 `.bak` 都不可用時，ADR 要掃描 `confirmed: true` 且版本最高的槽。這無法表達「最後一次提交」：管理者手動回滾後會重新選回較新版本；若 `confirmed` 更新時序不嚴謹，也可能選到失敗的試用槽。ADR 同時宣稱提交只需一次寫入 `current.json`，卻沒有定義 `slot.json.confirmed` 何時、如何持久化，兩者互相矛盾。
- **證據**：
  - 提交與掃描規則：[docs/adr/0001-windows-ota-upgrade.md:216](</F:/sync-project/kp-s11-adr/docs/adr/0001-windows-ota-upgrade.md:216>)。
  - 失敗試用槽仍會保留七天：[docs/adr/0001-windows-ota-upgrade.md:270](</F:/sync-project/kp-s11-adr/docs/adr/0001-windows-ota-upgrade.md:270>)。
  - ADR-0003 明定失敗版本不得自動選取，並指出「最高版本」會違反 INV-1／INV-10：[docs/adr/0003-rollback-bundle-trust.md:317](</F:/sync-project/kp-s11-adr/docs/adr/0003-rollback-bundle-trust.md:317>)、[docs/adr/0003-rollback-bundle-trust.md:328](</F:/sync-project/kp-s11-adr/docs/adr/0003-rollback-bundle-trust.md:328>)。
- **建議修改**：採納 ADR-0003 的方向，但不要只加牆上時間 `confirmed_at`。應使用單調遞增的 commit generation 或追加式提交紀錄；掃描選最新「成功提交／手動選定」的 generation，明確排除 `failed_at` 槽。必須定義每個寫入順序、flush、當機點及恢復結果。

### 2.3 MSI 種子直接成為 active，繞過本設計最重要的健康確認

- **位置**：ADR-0001 §4.4 第 3 步、§4.8。
- **問題**：新版 MSI 安裝後，只要啟動器版本高於 active，就直接建立 `confirmed: true` 的種子並切換，不經 trial。若 MSI 帶入的新版啟動即崩潰、被 Defender 隔離或不相容，前一槽雖仍存在，但沒有一致的自動切回流程。這與 D2、「新版必須健康後才提交」及 Android A/B 類比直接衝突。
- **證據**：
  - 直接建立 confirmed 種子：[docs/adr/0001-windows-ota-upgrade.md:224](</F:/sync-project/kp-s11-adr/docs/adr/0001-windows-ota-upgrade.md:224>)。
  - 明寫「不經健康確認的自動切回」：[docs/adr/0001-windows-ota-upgrade.md:295](</F:/sync-project/kp-s11-adr/docs/adr/0001-windows-ota-upgrade.md:295>)。
  - ADR-0003 要求種子也走 trial：[docs/adr/0003-rollback-bundle-trust.md:331](</F:/sync-project/kp-s11-adr/docs/adr/0003-rollback-bundle-trust.md:331>)。
- **建議修改**：無條件採納 ADR-0003：所有 seed 更新，只要已有 active，就必須走 trial、相同健康條件及自動切回；只有首次安裝、完全沒有槽時，才可建立初始 active。

### 2.4 LocalSystem 代理程式可改寫啟動器、槽與回滾狀態，A/B 保證沒有權限隔離

- **位置**：ADR-0001 §4.1、§4.5、§5.3；ADR-0003 §7.3 第 2 點。
- **問題**：ACL 可以擋一般本機使用者，卻擋不住同為 LocalSystem 的代理程式。候選版本若被攻陷或有嚴重檔案操作錯誤，可以改寫 `current.json`、`slot.json`、信任錨、前一槽乃至根目錄啟動器；此時 V5 雜湊沒有安全意義。故「任何當機或失敗一定保留舊版」只能涵蓋良性當機及受控寫入中斷，不能涵蓋候選程式錯誤或遭入侵。
- **證據**：
  - 現行服務建立時 `account_name: None`，即 LocalSystem，且為 own-process：[pulse/src/winsvc.rs:205](</F:/sync-project/kp-s11-adr/pulse/src/winsvc.rs:205>)。
  - MSI 服務亦未指定其他帳號：[pulse/packaging/windows/pulse.wxs:34](</F:/sync-project/kp-s11-adr/pulse/packaging/windows/pulse.wxs:34>)。
  - ADR 自己承認代理程式與啟動器同為 LocalSystem：[docs/adr/0001-windows-ota-upgrade.md:384](</F:/sync-project/kp-s11-adr/docs/adr/0001-windows-ota-upgrade.md:384>)。
  - 現有 ACL 程式只解析部分允許 ACE，沒有擁有者、重新解析點或有效權限判斷：[pulse/src/file_perms.rs:15](</F:/sync-project/kp-s11-adr/pulse/src/file_perms.rs:15>)、[pulse/src/file_perms.rs:402](</F:/sync-project/kp-s11-adr/pulse/src/file_perms.rs:402>)。
- **建議修改**：
  - 採納 ADR-0003 的 Windows 無特權分離說明，縮小安全承諾。
  - `request.json`、`health.json` 不得影響回滾目標；啟動器只信任自己保存的 active／trial。
  - 在進入「支援」前，應把代理程式改以受限 token、服務 SID 或獨立低權限服務執行；特權啟動器／updater 才能寫槽、指標與信任錨。若第一版不做，必須明列這是預覽限制，不能宣稱 INV-6 是真正的安全邊界。

### 2.5 MSI 與 OTA 同時發生時沒有定義可證明收斂的協定

- **位置**：ADR-0001 §4.8、§4.9、§6 S7。
- **問題**：S7 只列「試用期間安裝 MSI」，沒有定義預期結果。MajorUpgrade 會停止服務並換掉啟動器；新啟動器必須讀懂舊啟動器正在使用的 trial schema、ABI 與狀態。反之，較舊 MSI 通常會在 Windows Installer 層被 `MajorUpgrade` 阻擋，並不是安裝後再由啟動器決定不降版。
- **證據**：
  - WXS 明確禁止較新版本被降版 MSI 取代：[pulse/packaging/windows/pulse.wxs:24](</F:/sync-project/kp-s11-adr/pulse/packaging/windows/pulse.wxs:24>)。
  - `ServiceControl` 會停止及移除服務，但沒有 Start：[pulse/packaging/windows/pulse.wxs:36](</F:/sync-project/kp-s11-adr/pulse/packaging/windows/pulse.wxs:36>)。
  - ADR 的 S7 沒有通過條件：[docs/adr/0001-windows-ota-upgrade.md:417](</F:/sync-project/kp-s11-adr/docs/adr/0001-windows-ota-upgrade.md:417>)。
  - ProductCode 只依去掉預發佈標記的數字版本產生，不是嚴格的每個 SemVer 都不同：[packaging/build-release-common.sh:489](</F:/sync-project/kp-s11-adr/packaging/build-release-common.sh:489>)。
- **建議修改**：定義跨程序全域鎖、MSI 停止時的 trial 取消／接續規則、launcher schema 的前後向相容期，以及修復、MajorUpgrade、較舊 MSI、相同數字版本不同 prerelease 的預期結果。S7 必須逐項有斷言，不可只寫「測試互動」。

### 2.6 槽名稱不足以唯一識別內容，失敗後重試同一版本可能無法建槽

- **位置**：ADR-0001 §4.2、§4.6。
- **問題**：槽目錄只用 `<version>-<commit>`，但 ADR-0003 的信任鍵包含 artifact SHA-256。同一 version/commit 的不同重建、重新簽署或失敗後立即重試，都可能碰到既有目錄。失敗 trial 又會保留七天，使重試更容易衝突。
- **證據**：
  - 槽名稱格式：[docs/adr/0001-windows-ota-upgrade.md:169](</F:/sync-project/kp-s11-adr/docs/adr/0001-windows-ota-upgrade.md:169>)。
  - 失敗槽保留：[docs/adr/0001-windows-ota-upgrade.md:270](</F:/sync-project/kp-s11-adr/docs/adr/0001-windows-ota-upgrade.md:270>)。
  - ADR-0003 要求記錄鍵含發佈檔雜湊：[docs/adr/0003-rollback-bundle-trust.md:308](</F:/sync-project/kp-s11-adr/docs/adr/0003-rollback-bundle-trust.md:308>)。
- **建議修改**：槽 ID 至少加入 artifact SHA-256 短碼或不可重複 ID，顯示名稱再另存 version/commit。定義重試同一發佈、清除失敗槽及磁碟空間預檢順序。

### 2.7 「階段 1 可先出貨」違反本文自己的 Windows 實機原則

- **位置**：ADR-0001 §6、§7 階段 1、U11。
- **問題**：啟動器不是單純配置變更；它成為 LocalSystem 服務本體、選槽、管理工作物件並轉送所有 CLI。即使 OTA 關閉，它仍是全新的特權啟動路徑和單點。只在 GitHub runner 通過 S0／S1，未經自管 VM 的重開機、Defender、既有安裝遷移與長時間運作就先出貨，與「沒有在 Windows 實際執行過的行為不出貨」不一致。
- **證據**：
  - 階段 1 建議可先出貨：[docs/adr/0001-windows-ota-upgrade.md:465](</F:/sync-project/kp-s11-adr/docs/adr/0001-windows-ota-upgrade.md:465>)。
  - 現有 CI 只跑一般測試及 e2e，Windows 套件在 Ubuntu 打包，未安裝 MSI：[.github/workflows/ci.yml:25](</F:/sync-project/kp-s11-adr/.github/workflows/ci.yml:25>)、[.github/workflows/ci.yml:84](</F:/sync-project/kp-s11-adr/.github/workflows/ci.yml:84>)。
  - 現況仍明列服務與 MSI 未經實機驗證：[CHANGELOG.md:358](</F:/sync-project/kp-s11-adr/CHANGELOG.md:358>)。
- **建議修改**：階段 1 不單獨進正式發佈；至少要先完成 Server 2022＋Windows 11 VM 上的既有 MSI 遷移、重開機、服務停止、子行程崩潰、Defender 與 MajorUpgrade。若真的先出貨，只能進明確隔離的內部／預覽通道。

## 3. 次要問題與不一致

- ADR-0001 新增 `install.method = "slot"`、`install.upgrade = "insecure_path"`、`launcher`、`launcher_abi`，但 `REMOTE.md` 仍只列六種 method 與五種 upgrade 狀態：[REMOTE.md:69](</F:/sync-project/kp-s11-adr/crates/kairos-proto/REMOTE.md:69>)。程式的 token 驗證容許未知值，所以線上相容是安全的：[crates/kairos-proto/src/mgmt/mod.rs:204](</F:/sync-project/kp-s11-adr/crates/kairos-proto/src/mgmt/mod.rs:204>)；但規格必須同步。
- 舊 Kairos 遇到 `slot` 會安全地分類為不支援：`artifact_kind` 沒有 `slot`，[crates/kairos-proto/src/release/mod.rs:388](</F:/sync-project/kp-s11-adr/crates/kairos-proto/src/release/mod.rs:388>)；rollout 隨後回 `unsupported_platform`，[kairos/src/remote/rollout.rs:657](</F:/sync-project/kp-s11-adr/kairos/src/remote/rollout.rs:657>)。ADR 對向下相容的判斷正確。
- `REMOTE.md` 的 Windows 信任錨目前是 `%ProgramData%\Pulse\allowed_signers`：[REMOTE.md:390](</F:/sync-project/kp-s11-adr/crates/kairos-proto/REMOTE.md:390>)，程式也如此：[pulse/src/config_manager.rs:348](</F:/sync-project/kp-s11-adr/pulse/src/config_manager.rs:348>)；ADR 改成安裝根目錄，屬破壞性預設變更，必須有遷移規則，而不只是更新文件。
- `REMOTE.md` 的健康條件以 Windows 服務 `RUNNING` 與 `status.json` 為準：[REMOTE.md:472](</F:/sync-project/kp-s11-adr/crates/kairos-proto/REMOTE.md:472>)；槽式設計改成子行程與 `health.json`。這不是「只有路徑不同」，而是健康信號與信任邊界改變。
- ADR-0002 說自己與另兩份 ADR 獨立，但其資格預覽與版本庫顯示會受 `slot`、`insecure_path`、`launcher_abi`、本機槽式回滾影響。其「多少主機的回滾需要遺失檔案」必須排除 slot、tar，並依 ADR-0003 的本機封存能力重新計算：[docs/adr/0002-rollout-console-design.md:248](</F:/sync-project/kp-s11-adr/docs/adr/0002-rollout-console-design.md:248>)。
- ADR-0002 仍寫 Windows 0.10 不實作、`rollback_bundle` 尚待決：[docs/adr/0002-rollout-console-design.md:470](</F:/sync-project/kp-s11-adr/docs/adr/0002-rollout-console-design.md:470>)。現況敘述沒錯，但 ADR-0001／0003 核定後必須改成交叉引用，不能繼續稱為獨立。
- `3 × artifact.size + 64 MiB` 未涵蓋保留中的 failed trial、active、previous、下一次 staging/work/tmp 同時存在的峰值。應先定清理順序，再重算最壞空間。
- `health_timeout_secs` 在既有規格是「替換後期限」，ADR 改為只累計開機確認時間。這是語意變更，需明寫進協定規格；starts 上限只能部分補償。
- Windows Installer 對未版本化檔案的敘述基本正確；若 modified time 晚於 creation time，會當作使用者資料而不覆寫。[Microsoft File Versioning Rules](https://learn.microsoft.com/en-us/windows/win32/msi/file-versioning-rules)。但「wixl 是否產生 MsiFileHash」仍未查證，ADR 正確標為待驗證。
- `MoveFileExW` 的文件只承諾 write-through 等待搬移寫入磁碟，未給斷電後名稱繫結的完整保證；ADR 不把它當斷電原子操作是正確的。[MoveFileExW](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-movefileexw)。
- Windows Installer 預設會為失敗安裝建立 rollback script 並自動回復，這支持方案 A 的一般失敗分析；但 ADR 所稱「斷電後下一次 msiexec 會接續回復」沒有 repo 或引用的一手證據，應保留為未驗證。[Rollback Installation](https://learn.microsoft.com/en-us/windows/win32/msi/rollback-installation)。

## 4. 已核對且確認正確的關鍵事實

- Windows 現況一律回報 `install.upgrade = "unsupported"`：[pulse/src/remote/install.rs:266](</F:/sync-project/kp-s11-adr/pulse/src/remote/install.rs:266>)。
- Kairos 現況明確拒絕非 Linux 的推送資格：[kairos/src/remote/rollout.rs:657](</F:/sync-project/kp-s11-adr/kairos/src/remote/rollout.rs:657>)。
- Windows 安裝方式目前只依執行檔是否位於 `%ProgramFiles%\Pulse` 判斷 MSI／zip：[pulse/src/remote/install.rs:134](</F:/sync-project/kp-s11-adr/pulse/src/remote/install.rs:134>)。
- 服務名稱為 `Pulse`、own-process、自動啟動，未指定帳號即 LocalSystem；zip 安裝會帶 `--workdir`：[pulse/src/winsvc.rs:205](</F:/sync-project/kp-s11-adr/pulse/src/winsvc.rs:205>)。
- MSI 的服務命令是 `service run`，有 Stop／Remove、沒有 Start：[pulse/packaging/windows/pulse.wxs:34](</F:/sync-project/kp-s11-adr/pulse/packaging/windows/pulse.wxs:34>)。
- `pulse.toml` 的 MSI component 為 Permanent：[pulse/packaging/windows/pulse.wxs:41](</F:/sync-project/kp-s11-adr/pulse/packaging/windows/pulse.wxs:41>)。
- 工作目錄確實決定 `pulse.toml`、`agent.id`、`identity`、`buffer`、`logs` 的位置：[pulse/src/winsvc.rs:331](</F:/sync-project/kp-s11-adr/pulse/src/winsvc.rs:331>)。
- `status.json` 預設位於 `%ProgramData%\Pulse`，並另授予 Users 讀權：[pulse/src/status_file.rs:53](</F:/sync-project/kp-s11-adr/pulse/src/status_file.rs:53>)、[pulse/src/status_file.rs:469](</F:/sync-project/kp-s11-adr/pulse/src/status_file.rs:469>)。
- MSI 確實由 wixl 0.106 在 Linux 打包；Windows CI 沒有安裝該 MSI：[packaging/build-release-common.sh:82](</F:/sync-project/kp-s11-adr/packaging/build-release-common.sh:82>)、[.github/workflows/ci.yml:84](</F:/sync-project/kp-s11-adr/.github/workflows/ci.yml:84>)。
- WXS 固定 UpgradeCode、使用 MajorUpgrade 且禁止降版：[pulse/packaging/windows/pulse.wxs:24](</F:/sync-project/kp-s11-adr/pulse/packaging/windows/pulse.wxs:24>)。
- repo 中沒有 PE 版本資源或 Authenticode 簽署步驟；CHANGELOG 也明列尚未做程式碼簽章：[CHANGELOG.md:358](</F:/sync-project/kp-s11-adr/CHANGELOG.md:358>)。
- 發佈清單與 `UpgradeOffer` 沒有 `deny_unknown_fields`；增加選用欄位可被舊端忽略：[crates/kairos-proto/src/release/mod.rs:484](</F:/sync-project/kp-s11-adr/crates/kairos-proto/src/release/mod.rs:484>)、[crates/kairos-proto/src/mgmt/mod.rs:777](</F:/sync-project/kp-s11-adr/crates/kairos-proto/src/mgmt/mod.rs:777>)。
- `%ProgramData%` 的寬鬆預設 ACL 確實值得視為風險；但 Microsoft 沒有在目前引用的一手文件列出跨版本完全一致的 ACE。ADR 把確切 ACE 留給 `icacls` 實測是適當的。Microsoft 只給出較一般性的建議：應用程式裝在 Program Files，其他位置必須正確設定 ACL：[Windows Shell Security Considerations](https://learn.microsoft.com/en-us/windows/win32/shell/sec-shell)。

## 5. 不正確或找不到程式碼依據的主張

- **不正確**：「一天最多 3 次」服務重啟；實際會重複最後一個 Restart。
- **不精確**：「ProductCode 依版本推導、不同版本不同」；實際只依去掉 prerelease 的數字版本推導，同一數字版本的不同 prerelease 會相同。
- **過度保證**：「啟動器開機後一定會被 SCM 啟動」「不存在沒有可啟動版本的時間窗」。SCM、AppLocker／WDAC、Defender 隔離、啟動器本身損毀或 MSI 更新失敗都可能使啟動器不能啟動。
- **設計未成立**：「指標不是舊完整狀態就是新完整狀態」。正常 API 成功時可以如此，但斷電持久性沒有文件保證；兩份指標同時損毀後的掃描規則目前也不安全。
- **找不到依據**：「多數 Windows 客戶使用 MSI」。repo 沒有部署遙測或客戶資料，只能視為產品假設。
- **尚未證實**：各支援 Windows 版本的 `%ProgramFiles%` 精確預設 ACE、AppLocker／WDAC 對新路徑未簽章執行檔的實際處理、Defender 的鎖檔／隔離模式。
- **尚未證實**：wixl 0.106 的 `MsiFileHash`、自訂動作、`RemoveFolderEx` 及 MajorUpgrade 排程細節。
- **不成立**：「階段 1 沒有新的特權行為」；啟動器本身就是新的 LocalSystem 選槽、啟動及轉送元件。
- **無法客觀確認**：5.5～7 人週。依現有工作清單看屬樂觀估計，不應用作承諾值。

## 6. 遺漏的方案、風險或驗證項目

- 應把 E3「低權限代理程式＋特權 updater／launcher」列為正式的安全演進門檻，而不只是遠期選項。至少在「支援」前完成，或明列不涵蓋代理程式遭入侵的回復保證。
- 可評估由 MSI 建立 `%ProgramData%\Pulse-Upgrade` 並明確套用 ACL，而不是依賴 ProgramData 預設繼承；這符合 Windows 將可變資料放 ProgramData 的慣例，也可避免在 Program Files 累積 staging、logs、buffer。前提是先驗證 wixl 能力。
- 啟動器應考慮獨立的 `pulse-launcher.exe`。同一個完整 `pulse.exe` 雖容易遷移，但使最難更新的信任元件攜帶整套代理程式程式碼與相依項。
- 需要定義啟動器／槽格式的升級與降級矩陣：舊 launcher 讀新 metadata、新 launcher 接手舊 trial、ABI 不相容、MSI 更新中斷。
- 增加磁碟滿、唯讀磁碟、NTFS 錯誤、current 與 backup 各種寫入失敗、目錄改名成功但 flush 未持久化、`slot.json` 成功而其他檔案未持久化的故障注入。
- 封存完整性不能只雜湊 `pulse.exe`；若日後槽包含 DLL、資源或其他可執行內容，應有完整檔案清單及每檔雜湊。
- 測試 MSI uninstall 後保留 slots，再安裝較舊／較新 MSI 的行為。現設計可能重新執行殘留的高版本槽，與管理者「解除安裝後重裝」的直覺不同。
- 測試同一 version/commit 重試、失敗 trial 未清理、同版重新簽署、兩個 rollout 指向相同內容。
- ACL 測試應加入 inherited ACE、deny ACE、擁有者隱含權限、mount point／junction、硬連結、8.3 名稱、大小寫差異、祖先目錄被換掉及以 handle 使用後的最終路徑比對。
- Defender 之外，進入「支援」前至少要有一種實際企業 EDR／應用程式控制設定；否則支援矩陣應明確排除第三方 EDR、AppLocker、WDAC。
- Stage 5 的「兩次真實發佈」沒有規模與觀察期間。應定義主機數、成功率、人工復原次數、rollback_failed 必須為零、至少一輪失敗自動切回。
- 工作量建議分開：
  - 工程與自動測試：較合理的規劃範圍約 **8～12 人週**。
  - 試點與支援觀察：另計曆時，不納入人週承諾。
  - Authenticode、E3 權限分離及第三方 EDR 若列入，需另估。

## 7. 對「待使用者決定」各項的意見

| 項目 | 意見 |
|---|---|
| U1 方案 E1 | **修改後同意**。先修正掃描、種子 trial、SCM 與 MSI 互動；安全承諾須限縮。 |
| U2 驗證環境 | **同意 (a)**。GitHub runner 做快速回歸，自管 VM 是預覽必要條件；CI 不能取代斷電與 Defender。 |
| U3 DisplayVersion | **同意 (a)**。不要改登錄假裝 Installer 資料庫一致；文件、CLI、主控台應同時顯示 launcher 與 agent 版本。 |
| U4 Authenticode | **同意 (b)**，但不取代 SSHSIG。若要支援 WDAC／AppLocker 管理環境，程式碼簽章很可能應是支援門檻；不建議把所有受信任 CA 都當升級授權來源。 |
| U5 預覽預設值 | **同意 (a)**。預覽期預設關閉，且應使用編譯期／發佈通道控制，不只依賴可被誤設的本機布林值。 |
| U6 啟動器形態 | **不同意首選 (a)**；建議第一版就用獨立的小型 `pulse-launcher.exe`。它是單點且只能靠 MSI 修，應盡量減少程式碼與相依。 |
| U7 槽來源 | **同意 (a)**，前提是 zip 相依通過 deny／授權審查，並限制解壓大小、路徑穿越、重複名稱及壓縮炸彈。 |
| U8 信任錨 | **同意 (a)**。放 `<根>\allowed_signers` 優於依賴 ProgramData 預設 ACL；需有明確遷移與安全安裝命令。 |
| U9 Windows 版本 | **同意只列實際驗證且仍在供應商支援期的版本**。ADR 內「2025 為基本」與 Stage 4 只要求 2022＋11 不一致，需統一。 |
| U10 `insecure_path` | **同意新增**。舊 Kairos 會安全地歸為 `not_ready`，且維運價值高。 |
| U11 階段 1 先出貨 | **不同意**。啟動器是新的 LocalSystem 關鍵元件；至少完成自管 VM 的遷移、重開機、MajorUpgrade、Defender 與故障測試後才能進預覽。 |
| U12 ADR-0003 | **同意採用其 Q1～Q6 結論**，並採納四點修改：掃描改依最後確認順序、種子走 trial、Linux 本機紀錄不得與 Windows 特權紀錄視為同等信任、明寫 Windows 沒有特權分離。另應一併採用 `failed_at`／INV-10 的排除規則。 |

ADR-0003 的四點建議中，第 1、3 點是正確性修正，第 2 點是安全邊界澄清，第 4 點是跨平台信任模型修正；四點都不宜留到實作時再決定。