// 設計包尚未收錄的產品代碼與三段式（工程暫行；人工維護）。
//
// error-codes.js 由設計包產生（node kairos/web/tools/sync-design.mjs），不能手改。工程已經採用、但設計包
// （design-system/copywriting.md §3.3、§4.2）還沒有收錄的代碼放在這裡；core/errors.js 先查 error-codes.js，
// 找不到才查這裡。設計包收錄後：重新執行 sync-design.mjs（它會列出這裡已經可以刪除的代碼），再把條目刪掉。
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
