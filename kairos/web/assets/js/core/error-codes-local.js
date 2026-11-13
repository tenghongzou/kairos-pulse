// 設計包尚未收錄的產品代碼與三段式（工程暫行；人工維護）。
//
// error-codes.js 由設計包產生（node kairos/web/tools/sync-design.mjs），不能手改。工程已經採用、但設計包
// （design-system/copywriting.md §3.3、§4.2）還沒有收錄的代碼放在這裡；core/errors.js 先查 error-codes.js，
// 找不到才查這裡。設計包收錄後：重新執行 sync-design.mjs（它會列出這裡已經可以刪除的代碼），再把條目刪掉。
// 設計包已收錄、但說明需要補充的代碼放在檔案後半的 LOCAL_ERROR_CODE_SUPPLEMENTS。
//
// - KRS-W8417：版本庫的檔案遺失。ADR-0002 決策點 7／U10（2026-11-13 核定）：照工程的號碼正式採用；
//   三段式是 ADR 的草稿，最終文案由設計包定案。
export const LOCAL_ERROR_CODES = {
  'KRS-W8417': {
    level: 'W',
    text: '版本庫的檔案遺失：資料庫記錄已存入的發佈檔不存在或大小與記錄不符（`file_problem`：`missing`／`size_mismatch`）。子類別 84（遠端升級），只有 Kairos 端、不鏡像；「不存在」與「大小不符」同號。系統健康的「遠端管理」、Pulse 版本庫、資格預覽的 `artifact_missing`、核准時的 409 `artifact_missing`、推送中等待檔案的主機。0.10。ADR-0002 決策點 7',
    title: 'Pulse 版本庫的檔案遺失',
    reason: '版本庫記錄這個發佈檔已經存入，但在版本庫的目錄（`data/releases/`）找不到它，或大小與記錄不符。常見的原因是還原了備份（`kairos db backup` 不包含發佈檔）、叢集換手後接手的節點沒有這個檔案，或檔案被手動移動、刪除。補回之前，需要這個檔案的主機無法升級或回滾。',
    fix: '1. 取得同一個版本的官方發佈包，在「Pulse 版本庫」這個版本的詳情按「上傳發佈檔…」重新上傳（或執行 `kairos releases import <目錄>`）；只需要補上遺失或大小不符的檔案。\n2. 不再需要這個版本時可以刪除它（沒有推送使用、也不是任何主機的回滾目標時）。',
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
