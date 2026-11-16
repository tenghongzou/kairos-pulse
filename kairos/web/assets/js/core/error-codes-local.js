// 設計包尚未收錄的產品代碼與三段式（工程暫行；人工維護）。
//
// error-codes.js 由設計包產生（node kairos/web/tools/sync-design.mjs），不能手改。工程已經採用、但設計包
// （design-system/copywriting.md §3.3、§4.2）還沒有收錄的代碼放在這裡；core/errors.js 先查 error-codes.js，
// 找不到才查這裡。設計包收錄後：重新執行 sync-design.mjs（它會列出這裡已經可以刪除的代碼），再把條目刪掉。
// 設計包已收錄、但說明需要補充的代碼放在檔案後半的 LOCAL_ERROR_CODE_SUPPLEMENTS。
//
// - KRS-W2013：下載目錄的安裝腳本與 SHA256SUMS 不符（0.12.0 與缺檔共用 KRS-W2012）。**暫定號**，正式分配與
//   三段式由設計包定案。
// - KRS-W2014：下載目錄的 SHA256SUMS.sig 沒有通過 [remote] allowed_signers 的驗證（安全稽核 C3 加強）。
//   **暫定號**，正式分配與三段式由設計包定案。
// - KRS-W8417：版本庫的檔案遺失。ADR-0002 決策點 7／U10（2026-11-13 核定）：照工程的號碼正式採用；
//   三段式是 ADR 的草稿，最終文案由設計包定案。
export const LOCAL_ERROR_CODES = {
  'KRS-W2013': {
    level: 'W',
    text: '設定了下載目錄（`enroll.download_dir`），三個必要的檔案都在，但安裝腳本沒有通過檢查：`pulse-install.sh` 與同目錄 `SHA256SUMS` 記錄的雜湊不符，或不是一般檔案、超過 4 MiB、讀不到。Kairos 照常啟動，主控台的 `downloads.available` 為 `false`、`product_code` 為此碼，S12 不顯示一行安裝指令（只對管理員顯示這則警告）；啟動時記一次。0.12.0 與缺檔共用 `KRS-W2012`。暫定號。安全稽核 C3',
    title: '不顯示一行安裝指令：安裝腳本與 SHA256SUMS 不符',
    reason: '下載目錄裡的 `pulse-install.sh` 與同一個目錄的 `SHA256SUMS` 記錄的雜湊不同：腳本被修改過，或目錄裡混了不同發佈版本的檔案。一行安裝指令會以 root 執行這個腳本，所以 Kairos 不提供指令；8444 的下載端點與 `pulse enroll` 不受影響。',
    fix: '1. 取得同一個發佈版本的 `pulse-install.sh`、`SHA256SUMS`、`SHA256SUMS.sig` 與套件，一起重新放進 `enroll.download_dir`（不需要重新啟動 Kairos）。
2. 確認這個目錄只有管理者可寫（`kairos/PKI.md` 第 17 節）；腳本不是自己換的時，先查明是誰改的。',
  },
  'KRS-W2014': {
    level: 'W',
    text: '設定了下載目錄（`enroll.download_dir`）與 `[remote] allowed_signers`，但下載目錄的 `SHA256SUMS.sig` 沒有通過驗證：簽章與 `SHA256SUMS` 不符、簽署金鑰不在 `allowed_signers` 或已過期、簽章涵蓋的 `SHA256SUMS` 沒有列出 `pulse-install.sh` 或雜湊不符，或 `allowed_signers` 本身無法使用。Kairos 照常啟動，主控台的 `downloads.available` 為 `false`、`product_code` 為此碼、`signature` 為 `failed`，S12 不顯示一行安裝指令（只對管理員顯示這則警告）；啟動時記一次。沒有設定 `allowed_signers` 時不驗證、不會出現此碼。暫定號。安全稽核 C3',
    title: '不顯示一行安裝指令：下載目錄的簽章沒有通過驗證',
    reason: 'Kairos 以 `[remote] allowed_signers` 的發佈公鑰驗證下載目錄的 `SHA256SUMS.sig`，沒有通過：目錄裡的 `SHA256SUMS` 或簽章不是官方發佈包原本的檔案（被修改過，或混了不同版本），或簽署這次發佈的公鑰不在 `allowed_signers`。一行安裝指令會以 root 執行下載目錄裡的腳本，所以 Kairos 不提供指令；8444 的下載端點與 `pulse enroll` 不受影響。',
    fix: '1. 取得同一個發佈版本的 `pulse-install.sh`、`SHA256SUMS`、`SHA256SUMS.sig` 與套件，一起重新放進 `enroll.download_dir`（不需要重新啟動 Kairos）。\n2. 發佈包沒有問題時，確認 `[remote] allowed_signers` 指向的檔案存在、Kairos 的服務帳號讀得到，而且列出了簽署這次發佈的公鑰（金鑰輪替後要加入新的公鑰）。\n3. 檔案不是自己換的時，先查明是誰改了下載目錄（`kairos/PKI.md` 第 17 節：目錄應該只有管理者可寫）。',
  },
  'KRS-W8417': {
    level: 'W',
    text: '版本庫的檔案遺失：資料庫記錄已存入的發佈檔不存在或大小與記錄不符（`file_problem`：`missing`／`size_mismatch`）。子類別 84（遠端升級），只有 Kairos 端、不鏡像；「不存在」與「大小不符」同號。系統健康的「遠端管理」、Pulse 版本庫、資格預覽的 `artifact_missing`、核准時的 409 `artifact_missing`、推送中等待檔案的主機。0.10。ADR-0002 決策點 7',
    title: 'Pulse 版本庫的檔案遺失',
    reason: '版本庫記錄這個發佈檔已經存入，但在版本庫的目錄（`data/releases/`）找不到它，或大小與記錄不符。常見的原因是還原了備份（`kairos db backup` 不包含發佈檔）、叢集換手後接手的節點沒有這個檔案，或檔案被手動移動、刪除。補回之前，需要這個檔案的主機無法升級或回滾。',
    fix: '1. 取得同一個版本的官方發佈包，在「Pulse 版本庫」這個版本的詳情按「上傳發佈檔…」重新上傳（或執行 `kairos releases import <目錄>`）；只需要補上遺失或大小不符的檔案。\n2. 不再需要這個版本時可以刪除它（沒有推送使用、也不是任何主機的回滾目標時）。',
  },
  // ADR-0003 階段 1（降級回滾、提議的單行大小規則）：**暫定號**，正式分配與三段式由設計包定案
  'KRS-W8418': {
    level: 'W',
    text: '降級回滾（鏡像 `PLS-W8418`）：升級後健康確認失敗，主機已回到舊版，但 deb／rpm 只換回了執行檔——沒有可用的回滾套件、套件管理員裝回失敗，或回滾套件的簽署金鑰已撤銷。`upgrade_status.state` 仍是 `rolled_back`，`detail` 以 `degraded:` 開頭並列出原因。推送自動暫停。暫定號。ADR-0003 第 5.4 節',
    title: '升級已回滾，但主機需要修復套件',
    reason: 'Pulse 已恢復到升級前的版本並正常運作，但回滾時沒有經套件管理員（dpkg／rpm）裝回舊版的套件，只換回了執行檔，所以套件資料庫仍顯示失敗的那個版本、套件中的其他檔案（systemd unit、文件）也還是新版的。原因列在這台主機升級結果的 `detail`（例如 `no_rollback_package`、`package_rollback_failed`、`rollback_signer_revoked`）。',
    fix: '1. 在主機上重新安裝目前（舊版）的套件：deb 以 `dpkg -i <套件>`，rpm 以 `rpm -U --replacepkgs --oldpackage <套件>`；`pulse upgrade status` 會顯示是否仍需要修復。\n2. 或在找出新版失敗的原因之後再推送一次升級：升級成功時套件資料庫恢復一致。\n3. 原因是 `no_rollback_package` 時，把主機目前的版本匯入 Pulse 版本庫，之後的升級就能經套件管理員回滾；是 `rollback_signer_revoked` 時，確認信任錨（`allowed_signers`）的變更是預期的。\n4. 處理後繼續推送。',
  },
  'KRS-W8419': {
    level: 'W',
    text: '升級提議超過協定的單行上限：`upgrade_offer` 實際序列化後（省略 `rollback_bundle` 之後仍然）超過 65536 bytes，Kairos 沒有送出（主機收到 `upgrade_unavailable`，連線不受影響）。主機在推送中標為 `failed`（原因 `offer_too_large`），推送自動暫停。只有 Kairos 端。暫定號。ADR-0003 第 6.3 節',
    title: '升級提議太大，沒有送出',
    reason: '這個版本的 `SHA256SUMS`、簽章或發佈清單在轉成協定訊息之後超過單行上限。正常的官方發佈不會發生；通常代表版本庫中這個版本的簽章資料異常（含大量需要跳脫的字元）。',
    fix: '1. 到「Pulse 版本庫」檢查這個版本的 `SHA256SUMS` 與簽章；以官方的發佈包重新匯入（先刪除再匯入，或匯入為新的版本）。\n2. 修正後對失敗的主機按「重試」，再繼續推送。',
  },
};

// 設計包**已收錄**、但總表的說明還沒有涵蓋工程新增情況的代碼（工程暫行；人工維護）。這裡的欄位覆蓋
// error-codes.js 的同名欄位（core/errors.js 的 lookup）；沒有列出的欄位沿用設計包的。設計包的說明更新後：
// 重新執行 sync-design.mjs（它會列出這裡的代碼供人工比對），確認已涵蓋再把條目刪掉。
//
// - KRS-E8405：0.10.0 的 Pulse 升級輔助程式新增路徑檢查（REMOTE.md 第 17.9.7 節），不通過時同樣回報
//   PLS-E8405（`detail` 以 `unsafe_path:` 開頭）。設計包的總表只寫了「新版的預檢失敗」。三段式是工程的草稿，
//   最終文案由設計包定案。
export const LOCAL_ERROR_CODE_SUPPLEMENTS = {
  'KRS-E8405': {
    level: 'E',
    text: '新版的預檢失敗（無法執行、版本與目標不符、目前的設定無法通過新版的 `check-config`），或 0.10 起升級輔助程式的路徑檢查沒有通過（`detail` 以 `unsafe_path: <路徑>: <原因>` 開頭：`/opt/pulse`、`/opt/pulse/pulse`、`/var/lib/pulse-upgrade` 或它們的上層目錄不屬於 root、可由群組或其他人寫入，或是符號連結）。兩種情況都沒有做任何變更。F8，0.9；路徑檢查 0.10',
    title: '升級前的檢查沒有通過，主機沒有變更',
    reason: 'Pulse 的升級輔助程式在替換之前就停下來，這台主機仍是原本的版本。原因在這台主機的升級結果的 `detail`：以 `unsafe_path:` 開頭時，是安裝路徑不安全——`/opt/pulse`、`/opt/pulse/pulse`、`/var/lib/pulse-upgrade` 或它們的某一層上層目錄不屬於 root、可由群組或其他人寫入，或 `/opt/pulse` 是符號連結（`detail` 寫出是哪一個路徑與原因）；其他內容則是新版的預檢失敗——新版無法在這台主機上執行、版本與目標不符，或目前的設定無法通過新版的 `check-config`。',
    fix: '1. 路徑不安全（`unsafe_path`）：在主機上執行 `stat -c \'%U %a %n\' / /opt /opt/pulse /opt/pulse/pulse /var /var/lib /var/lib/pulse-upgrade`，把 `detail` 指出的路徑改為 root 擁有（`chown root`）並移除群組與其他人的寫入權限（`chmod go-w`）；`/opt/pulse` 是符號連結時改為實際的目錄。依 INSTALL 或套件安裝的主機本來就符合。Pulse 0.10 起會在連線時事先回報（主機詳情的「遠端升級」顯示「路徑不安全」，建立推送時的資格預覽不把它算成可升級），修正後 Pulse 下一次連線時恢復為就緒。\n2. 新版的預檢失敗：在主機上以 `journalctl -u pulse-upgrade` 查看輔助程式的訊息，修正設定或確認發佈檔適用於這個平台。\n3. 修正後重新推送；這台主機在修正之前不受影響，也可以改為手動升級。',
  },
};
