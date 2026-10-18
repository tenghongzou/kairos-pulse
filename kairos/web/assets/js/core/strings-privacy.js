// 個資保護（商業審計 S6）的介面文字：S05 主機詳情的「更多」選單（匯出個資、刪除主機資料）、
// S12 的一行安裝指令（下載端點）、S14 的匯入憑證。用語依設計包 copywriting.md（主機、撤銷、稽核日誌）。

export const PRIVACY_STRINGS = {
  'perm.hosts:purge': '刪除主機資料',
  'perm.hosts:personal_data': '匯出主機的個資',

  // ---- S05「更多」 ----
  'privacy.more': '更多',
  'privacy.moreLabel': '{name} 的更多操作',
  'privacy.export': '匯出個資（JSON）',
  'privacy.exported': '已匯出這台主機的個資',
  'privacy.exportedDesc': '檔案含主機名稱、位址等個資，請妥善保管，用完即刪除。',
  'privacy.exportFailed': '無法匯出個資',
  'privacy.purge': '刪除主機資料…',
  'privacy.previewFailed': '無法取得刪除的影響',
  'privacy.onlineTitle': '無法刪除 {name} 的資料',
  'privacy.onlineLead': '這台主機目前仍連線中',
  'privacy.onlineBody': '請先在主機上停止並移除 Pulse（或撤銷這台主機的憑證讓它斷線），主機離線後再刪除它的資料。',
  'privacy.purgeTitle': '刪除 {name} 的所有資料？',
  'privacy.purgeLead': '刪除後無法復原。',
  'privacy.purgeBody': 'Kairos 會刪除這台主機的下列資料，並從主機清單移除：',
  'privacy.count.reports': '原始 report',
  'privacy.count.rollups': '彙總（5 分鐘、1 小時）',
  'privacy.count.processes': '行程快照',
  'privacy.count.inventory': '資產與變更歷史',
  'privacy.count.alerts': '告警事件',
  'privacy.count.requests': '註冊申請',
  'privacy.count.certificates': '憑證紀錄',
  'privacy.revokeTitle': '會先撤銷 {n} 張仍有效的憑證',
  'privacy.revokeBody': '撤銷原因為 cessationOfOperation（停止使用）。這台主機之後無法再以這些憑證連線。',
  'privacy.keptAudit': '稽核日誌不刪除（雜湊鏈不可修改，依稽核日誌的保留期限到期刪除）；這次刪除也會記入稽核日誌。',
  'privacy.keptCrl': '為了撤銷清單（CRL），已撤銷的 {n} 張憑證紀錄保留到憑證過期後 1 天。',
  'privacy.keptLogs': 'Kairos 的日誌檔與資料庫備份中的資料不受影響，依各自的保留期限刪除。',
  'privacy.purgeConfirm': '刪除主機資料',
  'privacy.impactChanged': '影響的內容已改變，請關閉後重新操作',
  'privacy.purgeFailed': '刪除沒有完成',
  'privacy.purged': '已刪除 {name} 的資料',
  'privacy.purgedDesc': '共刪除 {n} 列，已記入稽核日誌。',

  // ---- S12 一行安裝指令（下載端點） ----
  'token.done.commandDownload': '一行安裝指令（在 agent 主機以系統管理員身分執行）：',
  'token.done.commandDownloadHint':
    'kairos-ca.pem 是 Kairos 的根 CA（pki.server_ca_path），allowed_signers 是發佈公鑰；兩者都請從另一個管道取得，不要和套件從同一處下載。',
  'token.done.commandManual': '已先以安裝套件（MSI、deb、rpm）安裝 Pulse 時，改為執行：',

  // ---- S14 匯入憑證 ----
  'cert.import': '匯入憑證…',
  'cert.import.title': '匯入手動簽發的憑證',
  'cert.import.body': '把既有部署以外部 CA（或 kairos pki sign）直接簽發的用戶端憑證加入簽發紀錄，之後會出現在清單中、可以撤銷。憑證必須由目前的信任錨直接簽發，SAN 恰好一個 agent_id。',
  'cert.import.pem': '憑證（PEM，可貼上多張）',
  'cert.import.submit': '匯入',
  'cert.import.done': '匯入 {ok} 張、先前已匯入 {already} 張、未通過 {rejected} 張',
  'cert.import.failed': '無法匯入',
  'cert.import.empty': '請貼上至少一張 PEM 憑證',
  'cert.import.rejected': '{source}：未通過（KRS-E1305）：{reason}',
};
