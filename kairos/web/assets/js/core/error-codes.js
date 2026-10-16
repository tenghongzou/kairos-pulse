// 自動產生，請勿手動修改：node kairos/web/tools/sync-design.mjs
// 來源：設計包 prototype/kairos/error-codes.js（copywriting.md §3.3 錯誤代碼總表、§4.2 三段式範例；產生於 2026-10-15）
// 只含 Kairos（KRS-）的 108 個代碼；三段式為 title（發生什麼）、reason（原因）、fix（處理方式）
export const ERROR_CODES = {
 "KRS-E0101": {
  "level": "E",
  "text": "設定檔語法錯誤（Kairos：`kairos.toml` 或 `KAIROS__*` 的 TOML 語法、型別錯誤，例如數字寫成字串、未知的列舉值；值超出範圍是 `0201`、互相矛盾是 `0202`）。Kairos 拒絕啟動（結束代碼 3），`kairos --check-config` 列為錯誤。Kairos 端 2026-10-15 分配（工程暫定的「設定檔無法載入」含檔案不存在，依子類別拆出 `0401`、`0402`，`decisions/INTEGRATION.md` §28）",
  "title": "無法載入設定檔：kairos.toml 第 23 行語法錯誤",
  "reason": "`/opt/kairos/kairos.toml` 第 23 行 `max_connections = \"3000\"` 的值是字串，這個欄位需要整數。Kairos 無法以這份設定啟動。",
  "fix": "1. 把第 23 行改成 `max_connections = 3000`（數字不加引號）。\n2. 以 `sudo -u kairos /opt/kairos/kairos --check-config --config /opt/kairos/kairos.toml` 確認沒有其他錯誤後再啟動。"
 },
 "KRS-W0104": {
  "level": "W",
  "text": "使用了已棄用的設定鍵、參數、環境變數或 MSI 屬性；仍然生效，下一個大版本移除（`decisions/naming.md` §5）",
  "title": "使用了已棄用的設定",
  "reason": "設定檔、參數或環境變數使用了舊的名稱；目前仍然生效，下一個大版本會移除。",
  "fix": "依日誌中提示的新名稱修改設定，再執行 `kairos --check-config` 確認。"
 },
 "KRS-E0201": {
  "level": "E",
  "text": "設定值超出範圍",
  "title": "設定值超出範圍：`timeouts.write_secs`",
  "reason": "`timeouts.write_secs` 設為 10，允許範圍是 1～6 秒。上限 6 秒是為了讓 Pulse 在伺服器寫入緩慢時不會誤判斷線。",
  "fix": "改為 6 以下（建議維持預設 5），再執行 `kairos --check-config` 確認。"
 },
 "KRS-E0202": {
  "level": "E",
  "text": "設定互相矛盾（Kairos：`server.listen`／`enroll.listen`／`console.listen` 中任兩個的 `IP:port` 相同；Pulse：註冊端點與 agent 連線完全相同）",
  "title": "設定互相矛盾",
  "reason": "`server.listen`、`enroll.listen`、`console.listen` 中有兩個使用相同的位址與連接埠，Kairos 拒絕啟動。",
  "fix": "為三個通道設定不同的連接埠（預設 8443、8444、9443），再執行 `kairos --check-config`。"
 },
 "KRS-W0203": {
  "level": "W",
  "text": "設定值大於另一個上限而被限縮，仍照常啟動：`server.max_connections_per_allowlisted_ip`（含內建預設 4,096）大於 `server.max_connections`，允許清單內每個位址的實際上限只有 `server.max_connections`；只在 `server.per_ip_allowlist` 非空時檢查，並於啟動時記錄一次；清單為空時 `server.max_connections_per_allowlisted_ip` 不生效，不檢查也不記錄（`README.md` D15、D22；S21 同一條件）",
  "title": "允許清單內每位址上限（4,096）大於同時連線上限（1,024），實際只會生效 1,024",
  "reason": "`server.max_connections_per_allowlisted_ip` 為 4,096（內建預設），但 `server.max_connections` 為 1,024（內建預設）。同一個位址的連線也算在全部連線之內，所以網段允許清單內每個位址最多只能有 1,024 條連線。`server.max_connections` 的內建預設對應作業系統預設的檔案描述元上限。",
  "fix": "1. 單一 NAT 出口後的主機不超過 1,024 台時，不需處理；也可以把 `server.max_connections_per_allowlisted_ip` 改為 1,024 以下，讓設定與實際一致。\n2. 需要更多連線時，在 `kairos.toml` 調高 `server.max_connections`（需重新啟動），並把系統的檔案描述元上限調到大於 `server.max_connections`＋64：Linux 以 systemd 執行時在 unit 的 `[Service]` 設定 `LimitNOFILE`（發佈包預設 8192），手動執行時先執行 `ulimit -n`。Windows 沒有對應設定，Kairos 也不檢查（只在 Linux 啟動時讀取 `/proc/self/limits`）。"
 },
 "KRS-W0204": {
  "level": "W",
  "text": "設定值低於依規模規劃的容量，仍照常啟動：`server.max_connections` 低於 ceil(授權節點數 × 1.2)（係數與首次啟動精靈相同；授權節點數＝有效＋過期寬限期內、不含已除役，`README.md` D17、D24、D28）。等於或高於時不警告。警告、不推播；設定變更（含啟動時載入）或授權節點數變化使比較結果跨越門檻時記錄一次，恢復時結束同一事件。這是事前的容量規劃檢查，與實際連線使用率（`KRS-W2204`）及被拒的連線無關。S20「連線」項目與 S21「同時連線上限」同一條件",
  "title": "同時連線上限低於依授權節點數規劃的容量",
  "reason": "`server.max_connections` 低於 ceil(授權節點數 × 1.2)。授權節點數＝持有有效憑證的主機加上憑證已過期但仍在寬限期內的主機（不含已除役）；係數 1.2 與首次啟動精靈相同，保留 20% 餘裕。授權節點全部上線時，連線數會超過上限的約 83%，接近或達到使用率門檻（KRS-W2204），超過上限的主機會連不上。這是事前的容量規劃檢查，與目前的連線數無關；等於或高於規劃容量時不顯示。",
  "fix": "1. 在「系統設定 › 連線與安全」把「同時連線上限」調到建議值（max(1,024, ceil(授權節點數 × 1.2))）以上，或直接修改 `kairos.toml` 的 `server.max_connections`；需重新啟動 Kairos。\n2. 一併確認檔案描述元上限至少為新上限＋64：Linux 以 systemd 執行時設定 `LimitNOFILE`（發佈包預設 8192，`server.max_connections` ≤ 8,128 時足夠），手動執行時 `ulimit -n`。\n3. 主機數預計還會增加時，以之後幾個月的主機數計算。"
 },
 "KRS-E0301": {
  "level": "E",
  "text": "未啟用 mTLS 卻監聽非 loopback 位址，拒絕啟動（只檢查 agent 連線埠；註冊端點只接受註冊請求，不適用）",
  "title": "Kairos 拒絕啟動：未啟用用戶端驗證就監聽對外位址",
  "reason": "`server.listen` 設為 `0.0.0.0:8443`，但沒有設定 `auth.client_ca`。這樣任何人都能以任意 `agent_id` 註冊，或把合法主機踢下線。",
  "fix": "1. 建議：執行 `kairos pki init` 建立 CA 並啟用 mTLS。\n2. 只在隔離的測試環境：設定 `auth.allow_unauthenticated = true`（主控台會持續顯示安全警告，且此變更寫入稽核日誌）。"
 },
 "KRS-W0302": {
  "level": "W",
  "text": "已停用對端驗證：Kairos `auth.allow_unauthenticated = true`；Pulse `accept_invalid_certs = true`",
  "title": null,
  "reason": null,
  "fix": null
 },
 "KRS-W0304": {
  "level": "W",
  "text": "`/metrics` 拒絕經反向代理轉送的請求：沒有設定 `observability.metrics_token_file`（`/metrics` 只接受 loopback）時，帶有 `Forwarded`、`X-Forwarded-For`、`X-Forwarded-Host`、`X-Forwarded-Proto`、`X-Real-IP` 或 `Via` 標頭的請求回 403。理由：同一台主機上的反向代理轉送進來的來源都是 `127.0.0.1`，「只接受 loopback」會變成任何能連到代理的人都能讀取指標。Kairos 第一次拒絕時記一行 `WARN`（日誌事件 `observability.metrics_proxied`），之後不重複記錄；不推播、不開事件。不加這些標頭的代理 Kairos 無法發現，所以不能以這個檢查代替 token。子類別 03（安全預設）。2026-10-15 分配（工程暫定同號，`decisions/INTEGRATION.md` §29）",
  "title": "已拒絕經反向代理轉送的 /metrics 請求",
  "reason": "`/metrics` 沒有設定 token（`observability.metrics_token_file`），只接受本機（loopback）的連線；這個請求帶有 `X-Forwarded-For: 10.20.0.15`，是經由同一台主機上的反向代理轉送進來的。若允許，任何能連到代理的人都能讀取主機數量、憑證到期日等指標。",
  "fix": "1. 建立 token 檔（例如 `openssl rand -hex 32 | sudo tee /opt/kairos/metrics.token`，擁有者 `kairos`、權限 0600），在 `kairos.toml` 設定 `observability.metrics_token_file = \"/opt/kairos/metrics.token\"`，重新啟動 Kairos。\n2. Prometheus 的抓取設定加上 bearer token（`authorization: { credentials_file: … }`）。\n3. 不需要經過代理時，讓 Prometheus 直接抓 `https://127.0.0.1:9443/metrics`，或另開 `observability.listen` 只綁 Prometheus 可達的位址。"
 },
 "KRS-E0401": {
  "level": "E",
  "text": "指定的設定檔不存在（Kairos：`--config` 指定的檔案不存在，結束代碼 3；沒有指定 `--config` 且目前目錄沒有 `kairos.toml` 時使用內建預設，不是錯誤）。Kairos 端 2026-10-15 分配",
  "title": "找不到設定檔 /opt/kairos/kairos.tml",
  "reason": "`--config` 指定的 `/opt/kairos/kairos.tml` 不存在。指定了 `--config` 時 Kairos 不會改用內建預設，避免在錯誤的設定下啟動。",
  "fix": "確認路徑（常見是副檔名打錯或 systemd unit 的 `ExecStart` 路徑過時）；`ls -l /opt/kairos/*.toml` 找出實際的設定檔後修正 `--config`。"
 },
 "KRS-E0402": {
  "level": "E",
  "text": "設定檔無法讀取（Kairos：權限不足、是目錄、不是 UTF-8 等 I/O 錯誤，結束代碼 3）。Kairos 端 2026-10-15 分配",
  "title": "無法讀取設定檔 /opt/kairos/kairos.toml",
  "reason": "執行 Kairos 的帳號 `kairos` 沒有讀取權限（目前為 `root:root 0600`）。",
  "fix": "`sudo chown root:kairos /opt/kairos/kairos.toml && sudo chmod 640 /opt/kairos/kairos.toml`，再執行 `sudo -u kairos /opt/kairos/kairos --check-config --config /opt/kairos/kairos.toml` 確認。"
 },
 "KRS-W1001": {
  "level": "W",
  "text": "Kairos 伺服器憑證即將到期",
  "title": null,
  "reason": null,
  "fix": null
 },
 "KRS-W1102": {
  "level": "W",
  "text": "用戶端憑證 SAN 與 `agent_id` 不符（Pulse 本機檢查或 `auth_failed`）",
  "title": "agent 的用戶端憑證與 agent_id 不符",
  "reason": "主機出示的憑證 SAN 與它宣告的 `agent_id` 不同，Kairos 拒絕連線（`auth_failed`）。常見原因是 `agent.id` 被重新產生，或憑證拿錯台。",
  "fix": "在該主機以目前的 `agent_id` 重新註冊：`pulse enroll --token <註冊 token>`。agent 端同一情況為 PLS-E1102。"
 },
 "KRS-W1103": {
  "level": "W",
  "text": "用戶端憑證含多個 DNS SAN（Pulse 本機檢查時以警告呈現）",
  "title": null,
  "reason": null,
  "fix": null
 },
 "KRS-W1105": {
  "level": "W",
  "text": "用戶端憑證被拒：已撤銷或不再受信任（`certificate_rejected`、TLS alert `CertificateRevoked`；伺服器未細分原因時也用此碼）",
  "title": "agent 的用戶端憑證被拒",
  "reason": "主機出示的憑證已撤銷、已過期或不再受信任（`certificate_rejected`）。",
  "fix": "若撤銷是預期的，不需處理；若主機應繼續監控，重新註冊取得新憑證。憑證已過期且在寬限期內的主機會自動續簽。"
 },
 "KRS-W1108": {
  "level": "W",
  "text": "用戶端憑證已過期（本機檢查、TLS alert `CertificateExpired`）",
  "title": null,
  "reason": null,
  "fix": null
 },
 "KRS-W1109": {
  "level": "W",
  "text": "用戶端憑證的簽發者不受信任（TLS alert `UnknownCA`）",
  "title": null,
  "reason": null,
  "fix": null
 },
 "KRS-W1110": {
  "level": "W",
  "text": "用戶端憑證即將到期，且不會自動輪替",
  "title": "agent 憑證即將到期，且不會自動輪替",
  "reason": "這張憑證是手動簽發（或主機長期離線），不會在到期前自動輪替。",
  "fix": "讓主機上線以自動輪替；手動簽發的主機請改以註冊 token 重新註冊（`pulse enroll`），之後就會自動輪替。"
 },
 "KRS-W1111": {
  "level": "W",
  "text": "用戶端憑證自動更新失敗（含等待管理者核准）",
  "title": null,
  "reason": null,
  "fix": null
 },
 "KRS-W1113": {
  "level": "W",
  "text": "憑證過期已超過過期寬限期，無法自動續簽（含寬限期政策為 0；協定 `renew_grace_exceeded`）",
  "title": "憑證過期已超過寬限期，無法自動續簽",
  "reason": "這台主機的憑證在 2026-07-01 過期，已過期 104 天，超過 Kairos 設定的過期寬限期（90 天）。",
  "fix": "1. 請 Kairos 管理者在「憑證 › 需重新註冊」選取這台主機，按「允許再次自動續簽…」。Pulse 每 6 小時會再試一次，允許後就會自動恢復，不需要到現場。\n2. 或取得新的註冊 token 後重新註冊：`pulse enroll --force --token <新 token>`。"
 },
 "KRS-W1114": {
  "level": "W",
  "text": "過期續簽被拒：主機指紋與紀錄不符，或舊私鑰簽章無效（協定 `renew_identity_mismatch`）",
  "title": "Kairos 拒絕自動續簽：主機識別資訊與紀錄不符",
  "reason": "過期續簽要求「主機指紋與註冊時相符，且以原本的私鑰簽名」，Kairos 回覆兩者至少有一項不符。常見原因：主機重灌或更換主機板、從映像檔複製的主機，或私鑰檔案被替換。",
  "fix": "確認這台主機是否為原本那台。是的話，以新的註冊 token 重新註冊：`pulse enroll --force --token <新 token>`。若是複製的主機，請參考 PLS-E3102 的處理方式。"
 },
 "KRS-W1115": {
  "level": "W",
  "text": "過期主機即將超過寬限期（寬限期剩 ≤ 14 天，彙總成一則事件）",
  "title": "過期主機即將超過寬限期",
  "reason": "有主機的憑證已過期、仍在寬限期內，但寬限期剩 14 天以下；超過後就無法自動續簽，必須重新註冊。",
  "fix": "讓這些主機在寬限期內上線（會自動續簽），或確認註冊端點不是「關閉」。"
 },
 "KRS-W1201": {
  "level": "W",
  "text": "撤銷清單（CRL）即將到期",
  "title": "撤銷清單（CRL）將在 5 天後過期",
  "reason": "`agents.crl` 的下次更新時間是 2026-10-14 09:00（UTC+8）。過期後所有 agent 都會被拒絕連線。",
  "fix": "執行 `kairos pki gencrl` 重新產生 CRL；或確認排程（`kairos-crl.timer`）是否正常執行。"
 },
 "KRS-E1203": {
  "level": "E",
  "text": "CRL 已過期或缺漏，伺服器無法判定撤銷狀態",
  "title": "撤銷清單（CRL）已過期，所有 agent 都會被拒絕連線",
  "reason": "`agents.crl` 的下次更新時間（nextUpdate）是 2026-10-08 09:00（UTC+8），已經過了。Kairos 採最嚴格的撤銷檢查，CRL 過期時無法判斷任何用戶端憑證是否已被撤銷，因此一律拒絕。",
  "fix": "1. 在管理主機重新產生 CRL：`kairos pki gencrl`\n2. 不需要重新啟動，Kairos 會在 30 秒內自動載入新的 CRL。\n3. 建議啟用每週自動更新（`kairos-crl.timer`），避免再次過期。"
 },
 "KRS-E1204": {
  "level": "E",
  "text": "新的 CRL 未生效（重新載入失敗，仍使用舊版本）",
  "title": null,
  "reason": null,
  "fix": null
 },
 "KRS-E1205": {
  "level": "E",
  "text": "CRL 自動更新失敗",
  "title": "CRL 自動更新失敗",
  "reason": "線上簽發模式下，Kairos 每 24 小時（與每次撤銷）以簽發 CA 重新產生 CRL，這次沒有完成，常見原因是簽發 CA 私鑰無法使用（KRS-E1303）或資料目錄無法寫入。舊的 CRL 仍有效，但到期後所有 agent 都會被拒絕連線（KRS-E1203）。離線簽發模式不會自動更新 CRL，到期前由 KRS-W1201 提醒。",
  "fix": "1. 線上簽發：依 KRS-E1303 的說明修正私鑰權限後，在「CA 與撤銷清單」按「立即更新 CRL」。\n2. 離線簽發：在 CA 主機執行 `kairos pki gencrl --revocations …`，再以「上傳 CRL 檔」套用。"
 },
 "KRS-E1206": {
  "level": "E",
  "text": "上傳的 CRL 未通過檢查，無法套用（S15「上傳 CRL 檔」）：版本號 `crlNumber` 未大於目前版本、簽章不是目前信任的 agent CA，或 `nextUpdate` 距今不到 24 小時；「原因」寫出是哪一項與實際值。撤銷筆數少於目前版本不直接拒絕，改為 L3 確認（`kairos/screens.md` §0.3）。2026-10-10 分配",
  "title": "無法套用上傳的 CRL",
  "reason": "上傳的 CRL 沒有通過檢查（「原因」會寫出是哪一項與實際值）：版本號 `crlNumber` 未大於目前版本、簽章不是目前信任的 agent CA，或 `nextUpdate` 距今不到 24 小時。最常見的是上傳了舊檔。",
  "fix": "1. 在 CA 主機重新執行 `kairos pki gencrl`，上傳新產生的 `agents.crl`。\n2. 確認使用的是目前的 agent CA（CA 輪替後舊 CA 簽的 CRL 不會被接受）。\n3. 撤銷筆數少於目前版本時不會直接拒絕，而是要求管理員以 L3 確認，避免已撤銷的憑證復活。"
 },
 "KRS-W1301": {
  "level": "W",
  "text": "簽發 CA 私鑰存放在伺服器上（線上簽發模式，`pki.mode = \"online\"`；啟動時記錄一次，說明取捨與離線簽發）",
  "title": "簽發 CA 私鑰存放在伺服器上",
  "reason": "線上簽發模式（預設）下，簽發 CA 的私鑰與 Kairos 放在同一台主機，核准後幾秒內就能簽發，過期續簽與輪替也能全自動完成；伺服器被入侵時，攻擊者可以簽發用戶端憑證冒充主機回報，但根 CA 私鑰不在伺服器上，無法冒充 Kairos。",
  "fix": "依資安等級評估：高敏感場域可改為離線簽發（系統設定或 `pki.mode = \"offline\"`），之後核准的申請需在 CA 主機簽發後匯入，撤銷也要上傳新的 CRL 才生效。"
 },
 "KRS-E1302": {
  "level": "E",
  "text": "簽發 CA 私鑰不在伺服器上（離線簽發模式，`pki.mode = \"offline\"`），無法自動簽發：核准的申請停在「已核准，待簽發」，需在 CA 主機簽發後匯入（工程 API `issuance_unavailable`）",
  "title": null,
  "reason": null,
  "fix": null
 },
 "KRS-E1303": {
  "level": "E",
  "text": "線上簽發模式，但簽發 CA 私鑰無法使用：檔案或目錄權限過寬（群組或其他使用者有任何權限）、擁有者不是執行 Kairos 的帳號、無法讀取或與憑證不成對；Kairos 照常啟動（既有主機照常連線），線上簽發停用，核准的申請停在「已核准，待簽發」，修正後下一次重新載入自動恢復。2026-10-14 分配（工程 `ENROLL.md` §19.4 #5）",
  "title": "簽發 CA 私鑰無法使用，已暫停自動簽發",
  "reason": "`/opt/kairos/data/pki/issuing-2.key` 的權限是 0644（其他使用者可以讀取），Kairos 不使用權限過寬的私鑰。已註冊的主機照常連線；新核准的申請、過期續簽與憑證輪替會停在「已核准，待簽發」，CRL 也無法自動更新。",
  "fix": "1. 修正權限：`sudo chown kairos:kairos /opt/kairos/data/pki/issuing-2.key && sudo chmod 600 /opt/kairos/data/pki/issuing-2.key`（目錄為 `chmod 700 /opt/kairos/data/pki`）。\n2. 等待下一次重新載入（`server.reload_interval_secs`，預設 30 秒）或重新啟動 Kairos，待簽發的申請會自動簽發。\n3. 私鑰可能已被他人讀取時，視為外洩：建立新的簽發 CA 並退役這一個（「CA 與撤銷清單」的 CA 輪替，`kairos pki issuing-ca retire --compromised`）。"
 },
 "KRS-W1304": {
  "level": "W",
  "text": "簽發 CA 即將到期，需要輪替：剩餘有效期少於「憑證政策的有效天數（含 token 覆寫中最長者）＋過期寬限期」（預設 365＋90＝455 天）；剩餘少於有效天數起，新簽發的憑證會被截短到簽發 CA 到期日。不推播，S15 與 S20 為警告。2026-10-14 分配",
  "title": "簽發 CA 將於 2027-08-01 到期，請開始輪替",
  "reason": "簽發 CA「○○市政府 Kairos Agent Issuing CA 2」剩 420 天，少於憑證政策的有效天數 365 天加過期寬限期 90 天。剩餘少於 365 天後，新簽發的 agent 憑證會被截短到 2027-08-01；簽發 CA 到期後，它簽發的所有憑證都無法再連線。",
  "fix": "到「CA 與撤銷清單」開始 CA 輪替：建立新的簽發 CA（產生 CSR）→ 在管理主機以根 CA 簽發 → 匯入並啟用 → 主機會在 6 小時內陸續換成新 CA 簽發的憑證 → 剩 0 台後退役舊 CA。"
 },
 "KRS-E1305": {
  "level": "E",
  "text": "匯入的憑證未通過檢查，未套用：簽發 CA 憑證（`kairos pki issuing-ca install`、S15 CA 輪替）或離線簽發的用戶端憑證（`kairos pki queue import`）；「原因」寫出是哪一張、哪一項與實際值。2026-10-14 分配",
  "title": "無法匯入憑證：issuing-2.pem 未通過檢查",
  "reason": "這張簽發 CA 憑證缺少擴充欄位 `pathlen:0`（實際為「未限制」）。Kairos 只接受 `CA:TRUE`、`pathlen:0`、`keyCertSign`／`cRLSign`、EKU `clientAuth`、與 Kairos 產生的私鑰成對、剩餘至少 2 年的簽發 CA 憑證；離線簽發的用戶端憑證則需鏈到目前的簽發 CA、SAN 與公鑰等於申請、序號未使用、EKU `clientAuth`。",
  "fix": "1. 在管理主機以 INSTALL 的 `issuing_ca_ext` 範本（離線簽發的用戶端憑證用 `client_ext`，或 `kairos pki sign-bundle`）重新簽發後再匯入。\n2. 確認簽發時使用的是這次 `kairos pki issuing-ca new` 或 `kairos pki queue export` 產生的 CSR，不是舊檔。"
 },
 "KRS-W1306": {
  "level": "W",
  "text": "`kairos pki init` 完成時，根 CA 私鑰仍在這台伺服器上（明文 PKCS#8、0600）：請立即複製到離線媒體（加密、兩份）並從伺服器刪除；之後簽發伺服器憑證或新的簽發 CA 時在離線主機以 `kairos pki sign` 使用（範例 99）。與 `1301`（**簽發 CA** 私鑰在伺服器上，線上簽發的正常取捨）不同：根 CA 私鑰不應留在線上。2026-10-15 分配（工程暫定同號，`decisions/INTEGRATION.md` §30）",
  "title": "私有 CA 已建立：請立即把根 CA 私鑰移到離線保存",
  "reason": "`kairos pki init` 已建立根 CA、伺服器憑證與簽發 CA。根 CA 私鑰目前以檔案存在這台伺服器上（`/opt/kairos/root-ca/root-ca.key`）。根 CA 私鑰可以簽出任何 Kairos 會信任的 CA，留在線上的伺服器上，一旦伺服器被入侵就必須重建整套 PKI、所有主機重新註冊。",
  "fix": "1. 把 `/opt/kairos/root-ca` 整個目錄複製到離線媒體（加密，至少兩份，分開保管）。\n2. 確認副本可以讀取後，從伺服器刪除：`sudo shred -u /opt/kairos/root-ca/root-ca.key`。\n3. 之後要簽發新的伺服器憑證或簽發 CA 時，在離線的 CA 主機以 `kairos pki sign` 使用這把私鑰。"
 },
 "KRS-E1307": {
  "level": "E",
  "text": "`kairos pki sign`、離線 `kairos pki gencrl`（CA 主機）：CA 憑證或私鑰無法使用——私鑰檔權限過寬（群組或其他使用者有任何權限）、與憑證不成對、不是 CA、沒有 `keyCertSign`／`cRLSign`、已過期、格式不支援（加密或舊式 RSA PEM）；不簽發、不寫紀錄，結束代碼 4（範例 100）。執行中的 Kairos 的簽發 CA 私鑰問題仍是 `1303`；匯入的憑證未通過檢查仍是 `1305`。2026-10-15 分配（工程暫定同號，§30）",
  "title": "無法簽發：CA 私鑰檔的權限過寬",
  "reason": "`/media/ca/issuing-ca.key` 的群組或其他使用者有讀取權限（目前 0644）。為了避免私鑰外流，`kairos pki sign` 不使用權限過寬的私鑰檔。沒有簽發任何憑證，也沒有寫入 `kairos-ca.log`。",
  "fix": "執行 `chmod 600 /media/ca/issuing-ca.key`（擁有者為執行指令的帳號）後重新執行。若私鑰檔曾經可被其他帳號讀取，請評估是否需要輪替這張 CA。"
 },
 "KRS-E2001": {
  "level": "E",
  "text": "監聽的連接埠已被佔用",
  "title": "Kairos 無法啟動：連接埠 8443 已被使用",
  "reason": "`0.0.0.0:8443` 已被其他程式佔用，可能是另一個 Kairos 仍在執行。",
  "fix": "1. 查看佔用的程式：Linux `ss -ltnp 'sport = :8443'`；Windows `netstat -ano | findstr :8443`。\n2. 結束該程式，或修改 `server.listen` 的連接埠。"
 },
 "KRS-W2008": {
  "level": "W",
  "text": "註冊端點不受理新註冊：模式為「僅續簽」，或「開放」但新註冊已自動暫停（協定 `enroll_not_accepted`；兩者對外回應相同）",
  "title": "註冊端點不受理新註冊，但仍有有效的註冊 token",
  "reason": "註冊端點模式為「僅續簽」，或「開放」但新註冊已自動暫停；以註冊 token 送出的新申請一律被拒絕（agent 端 PLS-E2008）。",
  "fix": "要部署新主機時，到「系統設定 › 連線與安全」改為「開放」，或建立新的註冊 token（啟用自動暫停時會自動恢復）。"
 },
 "KRS-W2009": {
  "level": "W",
  "text": "過期續簽已暫停：註冊端點模式為「關閉」，且有主機在寬限期內或 30 天內將進入（系統告警，警告可升級為嚴重）",
  "title": "過期續簽已暫停：3 台已回到網路的主機無法自動續簽",
  "reason": "註冊端點模式為「關閉」，Kairos 不監聽 8444。目前有 40 台主機的憑證已過期、在寬限期內，其中 3 台最近 24 小時已在 8443 以過期憑證連線（已回到網路）。",
  "fix": "在「系統設定 › 連線與安全」把註冊端點改為「僅續簽」，這些主機會在 1 小時內自動恢復，不需要到現場。"
 },
 "KRS-I2010": {
  "level": "I",
  "text": "設定檔沒有 `[enroll]` 區段或沒有 `enroll.mode`，註冊端點依內建預設維持「關閉」（升級後啟動時記錄一次）",
  "title": "註冊端點未啟用：設定檔沒有 `[enroll]` 區段",
  "reason": "`kairos.toml` 沒有設定 `enroll.mode`，依內建預設為「關閉」，Kairos 不監聽 8444。升級前的版本沒有註冊端點，所以這不會改變既有行為。",
  "fix": "需要自動註冊或過期續簽時，到「系統設定 › 連線與安全」選擇「開放」或「僅續簽」，或在 `kairos.toml` 加上 `[enroll]` 與 `mode = \"renew_only\"`。"
 },
 "KRS-W2011": {
  "level": "W",
  "text": "註冊端點模式為「僅續簽」，但過期寬限期政策為 0 天，不會受理任何請求（等同關閉，仍監聽 8444）；啟動時與任一值變更時記錄一次，不拒絕啟動",
  "title": "註冊端點為僅續簽，但過期寬限期為 0 天",
  "reason": "過期寬限期為 0 天時，憑證過期的主機一律不能自動續簽；「僅續簽」又不受理新註冊，所以註冊端點不會受理任何請求，等同關閉，但 Kairos 仍在監聽 8444。",
  "fix": "需要過期續簽時，到「憑證 › 憑證政策」把過期寬限期設為 1 天以上（預設 90 天）；不需要時，到「系統設定 › 連線與安全」改為「關閉」以停止監聽 8444。"
 },
 "KRS-W2101": {
  "level": "W",
  "text": "協定版本不相容（`unsupported_version`）",
  "title": null,
  "reason": null,
  "fix": null
 },
 "KRS-W2102": {
  "level": "W",
  "text": "第一則訊息不是 hello（`hello_required`）",
  "title": null,
  "reason": null,
  "fix": null
 },
 "KRS-W2103": {
  "level": "W",
  "text": "單行超過 65536 bytes（`too_long`）",
  "title": null,
  "reason": null,
  "fix": null
 },
 "KRS-W2104": {
  "level": "W",
  "text": "hello 欄位不合法（`bad_request`）",
  "title": null,
  "reason": null,
  "fix": null
 },
 "KRS-W2105": {
  "level": "W",
  "text": "report 不合法（`invalid_report`）",
  "title": null,
  "reason": null,
  "fix": null
 },
 "KRS-W2108": {
  "level": "W",
  "text": "資產資訊不合法（`inventory` 訊息被以 `invalid_report` 拒絕，不保存；Pulse 對同一份內容不再重送，等內容改變；範例 96）。與 report 不合法（`2105`）分開。2026-10-15 分配（工程暫定同號，`decisions/INTEGRATION.md` §30）",
  "title": "已拒絕 his-db-01 送出的資產資訊：內容不合法",
  "reason": "Pulse 送出的資產資訊（`inventory`）有欄位不符規格：網路介面 eth0 的 MAC 位址不是 6 組以「:」分隔的小寫十六進位。這一次的資產資訊沒有保存，主控台仍顯示上一次的內容；指標回報不受影響。通常是 Pulse 的錯誤或被修改過的 Pulse。",
  "fix": "確認這台主機的 Pulse 是正式版本（主機詳情的 Pulse 版本、`pulse --version`）。若是正式版本，請以 `pulse diag` 產生診斷包並回報；Pulse 在內容改變之前不會重送同一份資產資訊。"
 },
 "KRS-W2201": {
  "level": "W",
  "text": "同一 IP 連線數超過上限（Pulse 端：伺服器在加密連線建立前中斷）。8443：NAT 後超過 `server.max_connections_per_ip`（審計 A7，可加入 `server.per_ip_allowlist`）；8444：每 IP 同時連線或新連線硬上限，可重試",
  "title": "來自 203.0.113.10 的連線數已達上限（32），新連線被拒絕",
  "reason": "同一個來源位址（203.0.113.10）的同時連線數達到 `server.max_connections_per_ip`（未設定時為 32），之後的新連線在加密連線建立前就被關閉。最常見的是很多主機經由同一個 NAT 出口連線；這個位址不在網段允許清單中。",
  "fix": "若這個位址是機關的 NAT 出口，在「系統設定 › 連線與安全 › 網段允許清單（例外放寬）」加入該位址（`server.per_ip_allowlist`，上限改用 `server.max_connections_per_allowlisted_ip`，內建預設 4,096；實際上限不超過 `server.max_connections`，內建預設 1,024，見範例 30），或把 `server.max_connections_per_ip` 調高到該 NAT 後的主機數以上；若不認得這個位址，可能是掃描，維持現狀即可。"
 },
 "KRS-W2202": {
  "level": "W",
  "text": "註冊端點速率限制：每 IP（新註冊或續簽）、每 token、每 `agent_id`、輪詢、下載（HTTP 429 `rate_limited`）；Pulse 端也用於全域過載（503 `enroll_busy`）。Pulse 仍自動重試時以「進行中／警告」呈現，不顯示為失敗",
  "title": "來自 203.0.113.10 的註冊申請超過速率上限，部分申請延後處理",
  "reason": "同一個來源位址、token 或 agent_id 在短時間內送出的註冊或續簽請求超過速率上限（這次是 203.0.113.10 的新註冊，每 IP 每分鐘 600 次、可連續 600 次）。超限的請求會延後，Pulse 依 `Retry-After` 自動重試，不算失敗；新註冊與續簽分開計數，批次部署不會擋到過期續簽。",
  "fix": "若是已知的部署來源，按「暫時放寬 2 小時」或加入「批次部署來源」（`enroll.batch_sources`）；若不認得此位址，維持現狀即可，被延後的請求不會建立申請。"
 },
 "KRS-W2203": {
  "level": "W",
  "text": "回報速率超過上限（`storage_unavailable`：`report rate limit exceeded`）",
  "title": null,
  "reason": null,
  "fix": null
 },
 "KRS-W2204": {
  "level": "W",
  "text": "同時連線數接近或達到 `server.max_connections`（使用率 90% 警告、98% 或達到時嚴重，`README.md` D19；達到時暫停接受新連線），或握手中連線達其一半而拒絕新連線；S20「因全域上限被拒」用此碼，與「因每 IP 上限被拒」（2201）分開",
  "title": "agent 連線數接近或已達同時連線上限",
  "reason": "目前的連線數接近或達到 `server.max_connections`（使用率 90% 警告、98% 或已達上限為嚴重）。達到上限時 Kairos 暫停接受新連線，超出的主機會在離線判定時間後轉為離線。",
  "fix": "1. 調高 `server.max_connections`（首次啟動精靈的算式：預計主機數 × 1.2）。\n2. 同時調高檔案描述元上限（Linux `LimitNOFILE`／`ulimit -n`，至少為上限＋64）。\n3. 重新啟動 Kairos 後生效。"
 },
 "KRS-W2205": {
  "level": "W",
  "text": "註冊端點整體負載達上限（全域連線或全域申請速率；Pulse 端看到 `PLS-E2202`）",
  "title": "註冊端點整體負載已達上限，新的註冊請求暫緩",
  "reason": "過去 1 分鐘有 1,830 次新連線或申請，超過全域上限（每秒 200 次新連線、每秒 50 筆申請）。可能是多個場域同時部署，也可能是分散式的掃描。",
  "fix": "若是已排程的大量部署，暫時調高 `enroll.max_new_connections_per_sec`／`enroll.max_requests_per_sec`；否則檢查系統健康的來源分布。8443 上已註冊主機的連線不受影響。"
 },
 "KRS-W3002": {
  "level": "W",
  "text": "註冊 token 無效（不存在或已停用；對外不區分）",
  "title": null,
  "reason": null,
  "fix": null
 },
 "KRS-W3003": {
  "level": "W",
  "text": "註冊 token 已過期",
  "title": null,
  "reason": null,
  "fix": null
 },
 "KRS-W3004": {
  "level": "W",
  "text": "註冊 token 使用次數已滿",
  "title": null,
  "reason": null,
  "fix": null
 },
 "KRS-W3005": {
  "level": "W",
  "text": "來源被暫時封鎖：1 小時內嘗試過多相異的無效 token（封鎖新註冊），或過多簽章／指紋不符、重放（`replayed_request`）的過期續簽（封鎖續簽）；兩類分開計數（協定 `source_blocked`）。全域分散式猜測的告警也用 `KRS-W3005`",
  "title": "已暫時封鎖 203.0.113.10 的註冊申請：1 小時內嘗試了 23 個不同的無效 token",
  "reason": "同一個來源位址（203.0.113.10）在 1 小時內以 23 個不同的無效 token 嘗試註冊（門檻 20 個），Kairos 暫時封鎖這個位址的新註冊。多次送出簽章或主機指紋不符、或重放的過期續簽（`replayed_request`）會以同樣方式封鎖續簽；新註冊與續簽分開計數，時鐘偏差被拒的請求（`clock_skew`）不計入。",
  "fix": "若這是自己的部署（例如指令碼混用多個舊 token），解除封鎖並更新指令碼中的 token；否則維持封鎖，封鎖會自動到期，再犯時加倍。"
 },
 "KRS-I3101": {
  "level": "I",
  "text": "單次被取代（superseded）",
  "title": "agent 連線被同一個 agent_id 的新連線取代（單次）",
  "reason": "同一台主機重新連線時，舊連線尚未逾時就被新連線取代。偶發屬正常，例如網路短暫中斷後重連。",
  "fix": "不需處理。若同一主機頻繁發生，可能是兩台主機共用同一個 `agent_id`（KRS-W3102）。"
 },
 "KRS-W3102": {
  "level": "W",
  "text": "`agent_id` 衝突（連續被取代，或 Kairos 偵測到主機指紋不同）",
  "title": "偵測到 agent_id 衝突：兩個來源輪流使用同一個身分",
  "reason": "過去 10 分鐘內，同一個 `agent_id`（`6f1c0e4a…5566`）從 10.0.3.4 與 10.0.7.21 輪流連線，互相取代了 14 次。最常見的原因是複製 VM 映像檔時連 `agent.id` 一起複製。",
  "fix": "1. 在「不是原本那台」的主機上刪除 `agent.id`，然後重新啟動 Pulse，它會產生新的 `agent_id`。\n2. 若啟用了 mTLS，該主機還必須以新的 `agent_id` 重新註冊（`pulse enroll`），否則 Pulse 會因憑證 SAN 不符而無法啟動（PLS-E1102）。"
 },
 "KRS-W3108": {
  "level": "W",
  "text": "短時間內大量主機自行除役：10 分鐘內 ≥ 20 台主機經 8443 送出除役通知（`deregister`，Pulse 完全移除；門檻可在系統預設規則調整）。只計主機自行除役，不計主控台的人工除役（已有 L2 確認）。用來及早發現誤派送的「完全移除」（例如 SCCM／GPO 選錯集合）：主機本機的身分已刪除、憑證已撤銷，無法從主控台復原，需重新安裝並註冊。彙總成一則事件，列出台數、群組與來源分布、`initiated_by`。Pulse 端看不到此事件，不設鏡像代碼（單台除役成功只寫入稽核日誌與資訊事件，不分配代碼）。2026-10-11 分配（`decisions/INTEGRATION.md` §17）",
  "title": null,
  "reason": null,
  "fix": null
 },
 "KRS-E3201": {
  "level": "E",
  "text": "狀態表已滿（`server agent registry is full`）",
  "title": "狀態表已滿",
  "reason": "Kairos 記憶體中的主機狀態表已達上限，新的主機無法連線。",
  "fix": "檢查是否有大量重複或已除役的主機佔用；需要時調高上限（`limits.max_agents`）並重新啟動 Kairos。"
 },
 "KRS-E3304": {
  "level": "E",
  "text": "註冊申請內容不合法（CSR 的 SAN 與 `agent_id` 不符、名稱或標籤不合法）",
  "title": "註冊申請內容不合法",
  "reason": "CSR 的 SAN 與申請中的 `agent_id` 不符，或主機名稱、標籤含不允許的字元；Kairos 不會簽發這張憑證。",
  "fix": "請在該主機重新執行 `pulse enroll`（會重新產生正確的 CSR）；若持續發生，以 `pulse diag` 收集診斷資訊。"
 },
 "KRS-I3307": {
  "level": "I",
  "text": "等待核准（新的註冊申請）",
  "title": null,
  "reason": null,
  "fix": null
 },
 "KRS-E3309": {
  "level": "E",
  "text": "待核准佇列已滿（`enroll.max_pending`；協定 `pending_queue_full`）",
  "title": "待核准佇列已滿（20,000 筆），新的註冊申請暫緩",
  "reason": "待核准的註冊申請數已達 `enroll.max_pending`（20,000 筆），新的申請以 HTTP 503 回應、不建立申請，Pulse 每小時自動重試。常見原因是大量部署後沒有處理待核准，或 token 外洩造成大量申請。",
  "fix": "在「待核准」批次核准或拒絕；依 token 排序檢查是否有 token 外洩造成的大量申請；確實需要時調高 `enroll.max_pending`。"
 },
 "KRS-W4001": {
  "level": "W",
  "text": "資料（狀態、日誌）所在磁碟空間不足",
  "title": "資料磁碟空間不足",
  "reason": "Kairos 資料目錄所在的磁碟剩餘空間低於門檻；空間用完後回報會無法寫入（KRS-E4101）。",
  "fix": "釋放空間；把總容量上限 `storage.max_total_bytes` 調到磁碟可容納的大小（預留至少 128 MiB 給 WAL 與日誌）或縮短保留天數，讓 Kairos 刪除最舊的資料；或擴充磁碟。"
 },
 "KRS-E4003": {
  "level": "E",
  "text": "主控台無法載入資料（查詢失敗；標題寫出物件，例如「無法載入主機清單」）",
  "title": null,
  "reason": null,
  "fix": null
 },
 "KRS-E4101": {
  "level": "E",
  "text": "回報無法寫入（`storage_unavailable`：`report could not be stored`）。雙方協商了 `durable`（Kairos 與 Pulse 都是 0.3 以上、Pulse 啟用本機緩衝）時，Pulse 保留這些資料並在恢復後補送；任一端是舊版（或停用本機緩衝）時這段資料遺失。2026-10-13 補充",
  "title": "無法寫入回報資料，新的回報目前無法保存",
  "reason": "寫入資料庫 `/opt/kairos/data/kairos.db` 失敗：磁碟空間不足。從 14:03 起共有 1,284 筆回報未保存（agent 收到 `storage_unavailable`）。是否補送依兩端的版本而定：\n- 支援補送的主機（Kairos 與 Pulse 都是 0.3 以上，連線時協商了 `durable`）：資料留在各主機的本機緩衝，空間恢復後自動補送，Kairos 會去除重複。每台最多保留 64 MiB 或 50,000 筆（預設每 5 秒一筆時約 1.5～3 天）、最長 7 天，超過時丟掉最舊的資料。\n- 不支援補送的主機（Pulse 0.2 以前，或停用本機緩衝）：這段期間的資料不會補送。",
  "fix": "1. 釋放 `/opt/kairos/data` 所在磁碟的空間；或調低 `storage.max_total_bytes`、縮短保留天數，讓 Kairos 刪除最舊的資料（刪除後的空間由資料庫重複使用，檔案不會變小）。\n2. 空間恢復後，下一筆回報會自動寫入，不需重新啟動；支援補送的主機隨即開始補送積壓。"
 },
 "KRS-E4102": {
  "level": "E",
  "text": "JSONL 資料檔已達每日上限 `storage.max_daily_bytes`：只在 `storage.backend = \"jsonl\"` 時發生（預設的 SQLite 不使用每日上限；JSONL 鏡像達上限只停止寫鏡像，回報照常保存）。2026-10-13 補充",
  "title": "今天的 JSONL 資料檔已達上限，之後的回報無法保存",
  "reason": "Kairos 使用 JSONL 儲存（`storage.backend = \"jsonl\"`），`reports-2026-10-09.jsonl` 已達 `storage.max_daily_bytes`（5 GB）。到換日（2026-10-10 08:00 UTC+8，即 UTC 午夜）之前的回報都會收到 `storage_unavailable`：支援補送的主機把資料留在本機緩衝、換日後補送（補送的資料寫入收到當天的檔案，隔天可能較快達到上限）；不支援補送的主機（Pulse 0.2 以前）這段資料遺失。",
  "fix": "改回預設的 SQLite 儲存（`storage.backend = \"sqlite\"`：依總容量上限刪除最舊的資料，不會拒收新的回報）；必須沿用 JSONL 時，調高 `storage.max_daily_bytes`（`0` 表示不限制）後重新啟動 Kairos。也請檢查是否有主機異常大量回報。"
 },
 "KRS-W4104": {
  "level": "W",
  "text": "主機的指標序列數已達上限（`storage.max_series_per_host`，預設 2,048）：新序列的指標不保存，其他照存並回 `ack`；每台主機每小時最多記一次。常見於網路介面名稱不斷變化（容器網路）或大量自訂指標（範例 97）。2026-10-15 分配（工程暫定同號，`decisions/INTEGRATION.md` §30）",
  "title": "his-ap-01 的指標序列數已達上限（2,048），部分指標未保存",
  "reason": "這台主機在資料庫中已有 2,048 個指標序列（每個「指標名稱＋掛載點或網路介面」是一個序列），新的序列不再保存，既有的序列照常保存。常見於網路介面名稱不斷變化的主機（例如每次啟動容器都建立新的虛擬介面），或回報了大量自訂指標。",
  "fix": "1. 在主機詳情的「趨勢 › 網路」檢查介面清單，找出名稱一直改變的介面。\n2. 在這台主機的 `pulse.toml` 以 `collect.interface_exclude` 排除這些介面（例如 `[\"veth*\", \"tap*\"]`），重新啟動 Pulse。\n3. 舊的序列依保留期限自然刪除，不需手動清理；確實需要更多序列時調高 `storage.max_series_per_host`。"
 },
 "KRS-W4201": {
  "level": "W",
  "text": "系統日期異常，暫停清除過期資料或日誌",
  "title": null,
  "reason": null,
  "fix": null
 },
 "KRS-W4202": {
  "level": "W",
  "text": "容量已滿，提早刪除最舊的資料。Kairos：資料庫達總容量上限 `storage.max_total_bytes`（預設 64 GiB），依序刪除最舊的原始 report、5 分鐘彙總、1 小時彙總，實際保留期短於設定；Pulse：本機緩衝達 `buffer.max_mb`（預設 64 MiB）或 `buffer.max_records`（預設 50,000 筆），丟掉最舊的待補送資料。2026-10-13 分配",
  "title": "資料庫已達容量上限，正在提早刪除最舊的資料",
  "reason": "資料庫用量達 `storage.max_total_bytes`（64 GiB，約 68.7 GB），Kairos 依序刪除最舊的原始 report（原始資料刪光後才刪 5 分鐘、1 小時彙總），刪到上限的 95% 以下為止。依目前 1,918 台、每 5 秒回報估算，預設保留期（原始 7 天、5 分鐘彙總 31 天、1 小時彙總 400 天）需要約 99.8 GB，因此原始資料實際只能保留約 3 天。新的回報照常保存。",
  "fix": "1. 在「系統設定 › 資料保留與儲存」依預估把 `storage.max_total_bytes` 調到至少 100 GB（不超過資料磁碟的剩餘空間，並預留 128 MiB 給 WAL 與日誌）後重新啟動 Kairos；或縮短保留天數。\n2. 不需要這麼長的保留期時可以忽略：刪除分批進行，不影響新的回報。"
 },
 "KRS-E4301": {
  "level": "E",
  "text": "無法產生匯出檔案或診斷包（Kairos：CSV 匯出、`kairos diag`；Pulse：`pulse diag`）：輸出位置無法寫入、磁碟已滿、輸出檔已存在（不覆寫），或產生前的秘密檢查仍找到疑似秘密而不寫出；「原因」寫出是哪一種。2026-10-15 起 Kairos 端也用於 `kairos diag`（語意延伸、不另配號，`decisions/INTEGRATION.md` §28）",
  "title": null,
  "reason": null,
  "fix": null
 },
 "KRS-W4302": {
  "level": "W",
  "text": "匯出內容已截斷（診斷包超過大小上限：日誌合計超過 50 MB 時只收集較新的部分），診斷包仍產生，結束代碼 8。Kairos 端 2026-10-15 分配",
  "title": "診斷包已產生，但日誌超過大小上限，只收集了較新的部分",
  "reason": "要求收集 30 天的日誌（共 380 MB），診斷包的日誌上限為 50 MB，只收集了最近 4 天（47 MB）。",
  "fix": "診斷包可以照常交給支援人員。若需要更早的日誌，以 `--since` 指定較短但正確的區間重新產生，或直接提供 `logs/` 中特定日期的檔案（先自行檢查內容）。"
 },
 "KRS-E4303": {
  "level": "E",
  "text": "合規報表產生失敗",
  "title": null,
  "reason": null,
  "fix": null
 },
 "KRS-E4304": {
  "level": "E",
  "text": "`kairos reports import` 無法匯入：資料庫正在使用中（Kairos 執行中，結束代碼 11）、無法開啟資料庫，或匯入中途的資料庫錯誤（結束代碼 1，已提交的部分保留）。2026-10-13 分配（工程先選用，確認未佔用後正式分配，`decisions/INTEGRATION.md` §25.9）",
  "title": "無法匯入舊的回報資料：資料庫正在使用中",
  "reason": "`kairos reports import` 需要獨占資料庫 `/opt/kairos/data/kairos.db`，但 Kairos 正在執行（或另一個 `kairos` 子命令正在使用資料庫），沒有匯入任何資料。匯入不與服務同時寫入，以免彙總重複或漏算、拖慢即時回報的寫入。",
  "fix": "1. 先試執行，估算筆數與時間（Kairos 執行中也可以，約為匯入時間的一半）：`sudo -u kairos /opt/kairos/kairos reports import /opt/kairos/data --dry-run --config /opt/kairos/kairos.toml`\n2. 停止 Kairos 後匯入，完成後再啟動：`sudo systemctl stop kairos`，`sudo -u kairos /opt/kairos/kairos reports import /opt/kairos/data --config /opt/kairos/kairos.toml`，`sudo systemctl start kairos`。停機期間支援補送的主機（Pulse 0.3 以上）會把資料留在本機緩衝，恢復後補送；Pulse 0.2 的主機這段資料會遺失，停機要短時可在升級前先匯入已寫完的舊檔（`--until`）。"
 },
 "KRS-E4305": {
  "level": "E",
  "text": "`kairos reports import` 有來源檔無法讀取（權限、符號連結、FIFO 或裝置檔、壓縮檔、讀取錯誤），這些來源未匯入，其餘照常匯入（結束代碼 1）。2026-10-13 分配",
  "title": "有 2 個來源無法讀取，未匯入",
  "reason": "`reports-2026-09-01.jsonl.gz` 是壓縮檔（只匯入未壓縮的 JSONL）；`reports-2026-09-02.jsonl` 是符號連結（為了安全不跟隨）。其餘的檔案已照常匯入。",
  "fix": "1. 壓縮檔先解壓縮到另一個目錄再匯入：`zcat reports-2026-09-01.jsonl.gz > /tmp/import/reports-2026-09-01.jsonl`，再執行 `kairos reports import /tmp/import`。\n2. 符號連結改為指定實際的檔案；權限不足時確認服務帳號 `kairos` 可以讀取。重新執行時，已匯入的部分不會重複。"
 },
 "KRS-W4306": {
  "level": "W",
  "text": "`kairos reports import` 略過壞行（不是合法的 JSON、欄位缺漏或不合法、超過 1 MiB、沒有換行的最後一行），其餘照常匯入（結束代碼 0）。2026-10-13 分配",
  "title": "2 行不是合法的 report，已略過",
  "reason": "`reports-2026-10-04.jsonl` 第 3 行不是合法的 JSON；第 17282 行是沒有換行的最後一行（檔案可能仍在寫入）。其餘 34,560 筆已匯入。",
  "fix": "確認是否為截斷或手動編輯過的檔案；最後一行不完整時，等檔案寫完後重新執行（已匯入的部分不會重複）。"
 },
 "KRS-E4307": {
  "level": "E",
  "text": "金鑰檔的加密備份無法建立、解開或還原（`kairos db backup --include-keys`、`kairos db restore-keys`，`design-system/cli-style.md` §11.12）：還原時備份密碼錯誤或 `.keys` 檔被修改、截斷（兩者在加密上無法區分）、不是 `kairos.keys-backup/v1` 格式、資料目錄已有內容不同的金鑰檔（確認資料庫來自同一份備份後加 `--overwrite`）、無法寫回 `storage.data_dir`；建立時備份密碼不符合要求（少於 12 個字元、含換行）、金鑰檔無法讀取或長度不對、`.keys` 無法寫出。一律不寫入任何金鑰檔（還原時先檢查全部再寫入，不會只還原一半；建立時不留下資料庫備份），結束代碼 1。資料庫備份本身的失敗（備份檔已存在、磁碟已滿）仍是 `4301`。2026-10-15 分配（工程暫定同號，`decisions/INTEGRATION.md` §29）",
  "title": "無法還原金鑰檔：備份密碼錯誤或檔案已被修改",
  "reason": "以提供的備份密碼無法解開 `/var/backups/kairos/kairos-2026-10-15.db.keys`。密碼錯誤與檔案被修改、截斷在加密上無法區分。資料目錄沒有寫入任何金鑰檔。",
  "fix": "1. 確認使用的是建立這份備份時的備份密碼（密碼檔的第一行；注意編輯器加上的 BOM 或結尾空白）。\n2. 確認檔案是 `kairos db backup --include-keys` 產生的 `<備份檔>.keys`，大小與備份時相同、傳輸時沒有被轉換換行。\n3. 密碼確實遺失時無法解開：通知管道的機密要在「通知管道」重新輸入、所有人的兩步驟驗證要重設（`kairos users reset-mfa`）、稽核日誌的 HMAC 無法驗證（`KRS-E6201`，以外送的鏈尾雜湊確認之前的紀錄）。"
 },
 "KRS-E5001": {
  "level": "E",
  "text": "告警規則無法儲存",
  "title": null,
  "reason": null,
  "fix": null
 },
 "KRS-E5101": {
  "level": "E",
  "text": "無法連線到 SMTP 伺服器",
  "title": "無法連線到 SMTP 伺服器",
  "reason": "連到郵件伺服器逾時或被拒絕：主機名稱錯誤、連接埠未開放，或防火牆阻擋 Kairos 主機對外的 SMTP 連線。",
  "fix": "1. 確認主機名稱與連接埠（STARTTLS 通常為 587、TLS 為 465）。\n2. 請網路管理員開放 Kairos 主機到郵件伺服器的連線。"
 },
 "KRS-E5102": {
  "level": "E",
  "text": "SMTP 驗證失敗",
  "title": "SMTP 驗證失敗",
  "reason": "郵件伺服器拒絕 SMTP 帳號的登入（例如 `535 5.7.8`）。",
  "fix": "確認 SMTP 帳號與密碼；若郵件伺服器要求應用程式密碼或限制來源 IP，請聯絡郵件管理員。"
 },
 "KRS-E5103": {
  "level": "E",
  "text": "通知目的地的伺服器憑證不受信任",
  "title": "通知目的地的伺服器憑證不受信任",
  "reason": "郵件伺服器或 Webhook 目的地出示的憑證由不在信任清單中的 CA 簽發。",
  "fix": "在管道設定的「信任的 CA」選擇或上傳機關內部 CA；不建議關閉憑證驗證。"
 },
 "KRS-E5104": {
  "level": "E",
  "text": "無法連線到外部網路（LINE、Teams）",
  "title": "無法連線到外部網路（LINE、Teams）",
  "reason": "LINE Messaging API 與 Teams Workflows 都是雲端服務，Kairos 主機需要連外網路或代理伺服器。",
  "fix": "設定可連外的代理伺服器；離線（air-gapped）場域請改用 Email，或以 Webhook 送到內部的訊息閘道。"
 },
 "KRS-E5105": {
  "level": "E",
  "text": "LINE channel access token 無效或已撤銷",
  "title": "LINE channel access token 無效或已撤銷",
  "reason": "LINE 回應 401：token 不正確、已過期，或已在 LINE Developers 重新發行。",
  "fix": "到 LINE Developers 的 Messaging API 設定重新發行 channel access token，在 S11 按「更換」貼上後再測試。"
 },
 "KRS-E5106": {
  "level": "E",
  "text": "Teams Webhook 回應 4xx",
  "title": "Teams Webhook 回應 4xx",
  "reason": "Teams Workflows 回應 4xx（常見為 403 或 404）：流程已被刪除或停用，或 Webhook URL 已重新產生。舊式 Office 365 Connector 的 URL 也會得到這個錯誤。",
  "fix": "在 Teams 頻道的 Workflows 重新建立「收到 Webhook 要求時張貼到頻道」流程，複製新的 URL，在 S11 按「更換」貼上後寄送測試通知。"
 },
 "KRS-E5107": {
  "level": "E",
  "text": "通知管道連續發送失敗（已改由其他管道通知）",
  "title": "通知管道連續發送失敗",
  "reason": "同一個管道連續 3 次發送失敗，狀態改為「失敗」。這則系統事件不會經由失敗的管道送出，改送其他健康的管道；待送的通知保留在佇列最多 24 小時。",
  "fix": "開啟該管道查看最近一次的錯誤代碼並修正，寄送測試通知成功後儲存，狀態會恢復，佇列中的通知補送一則彙整。"
 },
 "KRS-E5108": {
  "level": "E",
  "text": "Webhook 目的地無法連線，或回應不是 2xx（逾時、連線被拒、HTTP 4xx／5xx；S11 測試發送與發送狀態寫出回應碼）。憑證不受信任用 `5103`，Teams Workflows 用 `5106`。2026-10-10 分配",
  "title": "Webhook 目的地無法連線或回應錯誤",
  "reason": "連到 Webhook URL 被拒絕或逾時，或接收端回應不是 2xx（例如 404、500）。",
  "fix": "1. 確認 URL 與連接埠，以及接收端（SIEM、工單系統）服務正常。\n2. 回應 4xx 時確認接收端的驗證設定（自訂標頭、HMAC 簽章）與 payload 格式（Kairos JSON 或 Alertmanager 相容）。\n3. 回應 5xx 時請接收端管理員查看其服務日誌。"
 },
 "KRS-E5109": {
  "level": "E",
  "text": "SMTP 伺服器拒絕寄件者、收件者或內容（`MAIL FROM`、`RCPT TO`、`DATA` 回 5xx，例如寄件者不允許轉送、收件者不存在、內容被判為垃圾郵件）；連線與驗證已成功（無法連線是 `5101`、驗證失敗是 `5102`）。「原因」附 SMTP 回應碼與伺服器訊息（已移除機密）。用於發送紀錄與 S11 測試發送；連續 3 次失敗另成立 `5107`。2026-10-15 分配（`decisions/INTEGRATION.md` §28）",
  "title": "Email 未送出：SMTP 伺服器拒絕收件者 ops@example.gov.tw",
  "reason": "連線與登入都成功，但 `mail.example.gov.tw` 對收件者回應 `550 5.7.1 Relaying denied`：伺服器不允許以寄件者 `kairos@monitor.local` 轉送到這個網域。",
  "fix": "1. 把寄件者改為 SMTP 伺服器接受的網域地址（例如 `kairos@example.gov.tw`），或請郵件管理員允許 Kairos 主機轉送。\n2. 在「通知管道」按「寄送測試通知」確認。"
 },
 "KRS-E5110": {
  "level": "E",
  "text": "通知管道的設定無效，無法儲存或發送：缺少必填欄位、網址或 Email 格式錯誤、Webhook 用 `http://` 但未允許（`allow_http`）、`security = \"none\"` 卻設定了 SMTP 密碼、`ca_file` 無法讀取等（主控台 API 400 `channel_invalid`，`details.parameter` 指出欄位）。S11 在欄位旁就地顯示；已儲存的管道在發送時才發現（例如 `ca_file` 被刪除）時記在發送紀錄。2026-10-15 分配。0.7 起資料目錄的 `alerts-secrets.key` 遺失或與資料庫不成對時，管道狀態為 `secrets_unavailable`（「無法解開機密」）也用此碼：不嘗試發送，「通知管道失敗」以此碼開事件（`kairos/alerts-ux.md` 5.5）",
  "title": "無法儲存通知管道「值班 Webhook」：網址必須是 https://",
  "reason": "Webhook 網址為 `http://10.20.0.8/hook`。Kairos 預設只送 HTTPS，避免告警內容與簽章金鑰以明文經過網路。",
  "fix": "改用 `https://` 網址；內部閘道確實只支援 HTTP 時，勾選「允許未加密的 HTTP」（`allow_http`）並了解內容會以明文傳送。"
 },
 "KRS-W5111": {
  "level": "W",
  "text": "測試通知太頻繁（HTTP 429 `rate_limited`，附 `Retry-After`；`details.scope` 為 `channel` 或 `user`）：同一個管道 60 秒內最多 3 次（不分使用者）、同一個使用者 10 分鐘內最多 10 次（不分管道，含尚未儲存的管道）。超過時不送出、不寫發送紀錄；計數只在記憶體中，Kairos 重新啟動後歸零。理由：測試通知會真的送到值班群組或信箱。與驗證失敗的限速（`6011`）是不同的情況，HTTP 的 `code` 相同，主控台依 `product_code` 區分。2026-10-15 分配（工程暫定同號，`decisions/INTEGRATION.md` §29）",
  "title": "測試通知太頻繁，請稍後再試",
  "reason": "「值班 LINE」在 60 秒內已寄送 3 次測試通知。測試通知會真的送到值班群組或信箱，為了避免洗版，每個管道 60 秒內最多 3 次、每個人 10 分鐘內最多 10 次。",
  "fix": "45 秒後再按「寄送測試通知」。要確認設定是否正確，先看上一次測試的結果與回應碼（在按鈕下方）。"
 },
 "KRS-E6001": {
  "level": "E",
  "text": "帳號或密碼不正確",
  "title": "帳號或密碼不正確",
  "reason": "輸入的帳號不存在，或密碼與紀錄不符。為了避免帳號被猜測，畫面不會說明是哪一項錯誤。",
  "fix": "1. 確認登入方式（機關帳號 AD／本機帳號）是否選對。\n2. 確認大小寫與輸入法（全形／半形）。\n3. 連續失敗會鎖定帳號（KRS-E6003）；忘記本機帳號密碼請聯絡管理員重設。"
 },
 "KRS-E6002": {
  "level": "E",
  "text": "無法連線到目錄服務（DNS、連接埠、防火牆）",
  "title": "無法連線到目錄服務",
  "reason": "Kairos 連不到 LDAP／AD 伺服器：主機名稱無法解析、連接埠（LDAPS 636、StartTLS 389）未開放，或防火牆阻擋。",
  "fix": "1. 在 Kairos 主機確認 DNS 能解析網域控制站名稱。\n2. 請網路管理員開放 Kairos 主機到網域控制站的 636（或 389）連接埠。\n3. 設定備援伺服器，避免單一網域控制站維護時無法登入。本機帳號仍可登入。"
 },
 "KRS-E6003": {
  "level": "E",
  "text": "帳號已鎖定（連續登入失敗）",
  "title": "帳號已鎖定",
  "reason": "連續登入失敗次數超過上限，帳號被暫時鎖定，避免密碼被猜測。",
  "fix": "連續失敗 5 次會鎖定 15 分鐘（可設定），畫面會顯示解鎖剩餘時間；等待結束後再試，若需要立即解除，請聯絡管理員。AD 帳號的鎖定由 AD 管理，請聯絡網域管理員。"
 },
 "KRS-E6004": {
  "level": "E",
  "text": "首次啟動精靈的設定碼不正確或已過期",
  "title": "首次啟動精靈的設定碼不正確或已過期",
  "reason": "設定碼是 Kairos 尚未初始化時在終端機印出的一次性代碼，有效 30 分鐘。輸入的代碼與目前有效的設定碼不符，或已超過 30 分鐘。",
  "fix": "在伺服器上執行 `kairos setup new-code` 取得新的設定碼（舊的設定碼立即失效），再於 30 分鐘內輸入。"
 },
 "KRS-E6005": {
  "level": "E",
  "text": "目錄服務的伺服器憑證不受信任",
  "title": "目錄服務的伺服器憑證不受信任",
  "reason": "網域控制站出示的憑證由不在信任清單中的 CA 簽發（例如機關自建的 AD CS 根憑證），或憑證名稱與伺服器位址不符。",
  "fix": "1. 在「目錄服務 › 信任的 CA」上傳簽發網域控制站憑證的根 CA（PEM）。\n2. 確認伺服器位址使用憑證上的名稱（例如 `dc01.city.gov.tw`，而不是 IP）。\n3. 不建議改用未加密的 `ldap://`（需要 L3 確認）。"
 },
 "KRS-E6006": {
  "level": "E",
  "text": "目錄服務 bind 帳號或密碼錯誤",
  "title": "目錄服務 bind 帳號或密碼錯誤",
  "reason": "網域控制站拒絕服務帳號的登入（LDAP result 49，invalidCredentials）。常見原因是服務帳號密碼已到期或被變更。",
  "fix": "1. 確認 Bind 帳號的完整 DN（例如 `CN=svc-kairos,OU=Service,DC=city,DC=gov,DC=tw`）。\n2. 請網域管理員確認服務帳號沒有被停用、密碼沒有到期，並把服務帳號設為「密碼永久有效」。\n3. 在 S17 按「更換」輸入新密碼後重新測試。"
 },
 "KRS-E6007": {
  "level": "E",
  "text": "目錄服務搜尋基底不存在",
  "title": "目錄服務搜尋基底不存在",
  "reason": "設定的使用者搜尋基底（Base DN）在目錄中找不到（LDAP result 32，noSuchObject），可能是 OU 名稱拼錯或已搬移。",
  "fix": "以 AD 管理工具確認 OU 的完整 DN，例如 `OU=Staff,DC=city,DC=gov,DC=tw`；不確定時可先設為網域根（`DC=city,DC=gov,DC=tw`）再縮小。"
 },
 "KRS-E6008": {
  "level": "E",
  "text": "目錄服務查無使用者",
  "title": "目錄服務查無使用者",
  "reason": "在搜尋基底內以登入屬性（預設 `sAMAccountName`）找不到這個帳號。帳號可能不在搜尋基底的 OU 底下，或篩選條件排除了它。",
  "fix": "確認帳號所在的 OU 在搜尋基底內，並檢查篩選條件（例如 `(objectClass=user)`）；也確認登入屬性是否應改為 `userPrincipalName`。"
 },
 "KRS-E6009": {
  "level": "E",
  "text": "目錄服務帳號已停用或鎖定",
  "title": "目錄服務帳號已停用或鎖定",
  "reason": "AD 回報這個帳號已停用或鎖定，因此無法登入 Kairos。依目錄服務的同步設定，該帳號在 Kairos 的工作階段也會被結束。",
  "fix": "請網域管理員確認帳號狀態；這是 AD 端的設定，Kairos 無法解除。"
 },
 "KRS-E6010": {
  "level": "E",
  "text": "主控台 API 驗證失敗（HTTP 401 `unauthorized`）：沒有 `Authorization` 標頭或格式錯誤、API token 不存在、已撤銷或已過期；對外不區分是哪一種，附 `WWW-Authenticate: Bearer realm=\"kairos\"`。出示了 API token 或工作階段 cookie 而失敗時扣該來源的驗證失敗額度（`6011`）；**完全沒有出示憑據**（沒有 `Authorization`、也沒有工作階段 cookie）的請求仍回 401，但**不扣額度**（0.7 起，`decisions/INTEGRATION.md` §29：未登入的瀏覽器開啟主控台時會先讀取 `/api/v1/me`，否則重新整理登入頁幾次就會被限速）。登入畫面的帳號密碼錯誤用 `6001`。2026-10-13 分配（`decisions/INTEGRATION.md` §26）。**主控台的工作階段**（cookie）結束、閒置或絕對逾時、使用者被停用或刪除時也用此碼，`details.reason = \"session_expired\"`：主控台不顯示錯誤頁，而是帶 `next` 回到 S01 並顯示「工作階段已結束，請重新登入」（2026-10-15 定案，§26.8 #4、§28）",
  "title": "API token 無效或已過期，請求被拒絕",
  "reason": "請求沒有可用的 API token。可能是沒有帶 `Authorization` 標頭或格式不是 `Bearer kat_…`、只貼了 token_id 而不是整串 token、token 已撤銷，或已過期（預設建立後 90 天）。Kairos 對外不區分這幾種情況，避免透露哪些 token 存在。",
  "fix": "1. 確認請求帶的是建立時顯示的整串 token：`Authorization: Bearer kat_<token_id>_<秘密>`。\n2. 請 Kairos 管理者查 token 的狀態與到期日：`sudo -u kairos /opt/kairos/kairos api-token list --all --config /opt/kairos/kairos.toml`。已撤銷或已過期時建立新的 token（`cli-style.md` §11.6），更新到使用它的系統後，撤銷不再使用的舊 token。\n3. 修正前先停止會自動重試的腳本：連續失敗超過 10 次時，同一來源 IP 的所有請求會被暫時拒絕（`KRS-W6011`）。"
 },
 "KRS-E6012": {
  "level": "E",
  "text": "兩步驟驗證碼不正確或已使用過（HTTP 401 `invalid_mfa_code`）：TOTP 碼不符（接受前後各 1 個 30 秒時間步）、同一時間步的碼已用過（重放），或復原碼不存在、已用過。錯誤計入帳號的連續失敗次數（`6003`）與來源的失敗額度（`6011`）；登入第二步的挑戰 5 分鐘有效、最多 5 次，用完要從帳號密碼重新開始。2026-10-15 分配（`decisions/INTEGRATION.md` §28）",
  "title": "驗證碼不正確或已使用過",
  "reason": "輸入的驗證碼不符合，或這組驗證碼剛才已用過（每組只能用一次，請等驗證器顯示下一組）。也可能是手機或主機的時間不準。",
  "fix": "1. 等驗證器 App 換成下一組 6 位數後再輸入。\n2. 仍然失敗時，確認手機的時間設為自動；或改用一組復原碼（`XXXXX-XXXXX`，每組只能用一次）。\n3. 手機遺失且沒有復原碼時，請管理員重設兩步驟驗證（`kairos users reset-mfa <帳號>`）。"
 },
 "KRS-E6013": {
  "level": "E",
  "text": "必須先變更密碼（HTTP 403 `password_change_required`）：管理員建立或重設密碼（含系統產生的一次性密碼）的本機帳號，登入後只能讀取自己的身分、變更密碼與登出，其他請求都回此碼。主控台登入後直接進入「變更密碼」，不顯示錯誤頁。2026-10-15 分配",
  "title": "請先變更密碼",
  "reason": "這個帳號的密碼是管理員設定或系統產生的一次性密碼，第一次登入時必須改成只有你知道的密碼，之後才能使用主控台。",
  "fix": "輸入目前的密碼與新密碼（至少 12 個字元、不包含帳號名稱、不是常見的密碼）。變更後其他裝置上的工作階段會結束，目前這個保留。"
 },
 "KRS-E6105": {
  "level": "E",
  "text": "請求未通過跨站請求偽造（CSRF）檢查（HTTP 403 `csrf_failed`）：以工作階段送出的寫入請求缺少或帶錯 `X-CSRF-Token`、`Origin` 不是主控台本身，或 `Sec-Fetch-Site` 為 `cross-site`。最常見的原因是主控台頁面開著時工作階段已換新（在其他分頁重新登入），或反向代理改寫了 `Host`。以 API token 驗證的請求不檢查。2026-10-15 分配",
  "title": "操作未送出：請重新整理頁面後再試",
  "reason": "這個請求沒有帶目前工作階段的安全檢查值，或來自其他網站。常見情況是你在另一個分頁重新登入過，這個分頁還是舊的工作階段資料。",
  "fix": "1. 重新整理頁面後再做一次。\n2. 經反向代理使用主控台時仍一直出現：請管理員確認代理保留原本的 `Host` 標頭，且沒有移除 `Origin`。"
 },
 "KRS-W6011": {
  "level": "W",
  "text": "同一來源驗證失敗太多次，暫時拒絕該來源的**所有** API 請求（HTTP 429 `rate_limited`，附 `Retry-After`，不再檢查 token）：依來源 IP 計算（IPv6 以 /64 合併），可連續失敗 10 次、之後每 6 秒恢復 1 次，最多約 1 分鐘；成功的請求、完全沒有出示憑據的請求（0.7 起）不扣額度。同一 NAT 或代理後的其他用戶端會被連帶限制。每個來源第一次被限制時 Kairos 記一行 `WARN`（不記 token 內容）。與註冊端點的速率限制（`2202`）無關。2026-10-13 分配",
  "title": "驗證失敗太多次，這個來源暫時無法使用主控台 API",
  "reason": "來自 10.20.5.31 的請求在短時間內驗證失敗超過 10 次（例如腳本帶著已過期的 token 反覆重試），Kairos 暫時拒絕這個來源的所有 API 請求，帶有效 token 的請求也一樣。額度每 6 秒恢復 1 次，停止失敗後最多約 1 分鐘恢復；同一個 NAT 或代理後的其他用戶端也會一起被限制。",
  "fix": "1. 找出並停止帶錯誤 token 的程式：Kairos 日誌中 `KRS-W6011` 那一行寫出來源位址；來源是 NAT 或代理時，檢查同一出口後的所有用戶端。\n2. 依回應的 `Retry-After` 秒數等待後再試，不需要重新啟動 Kairos。"
 },
 "KRS-E6104": {
  "level": "E",
  "text": "權限不足（HTTP 403 `forbidden`）：已通過驗證，但 API token 的範圍（`scope`）或使用者角色不包含這個操作。0.4 的 token 只有 `read`、端點也只需要 `read`，保留給之後的寫入端點與角色權限（`kairos/ia.md` §4）。2026-10-13 分配",
  "title": "沒有執行這個操作的權限",
  "reason": "API token「grafana」（token_id `3f9c2a71b4e05d18`）的範圍是 `read`，這個操作需要寫入權限。",
  "fix": "改用具備所需範圍的 token；`kairos api-token` 不能修改既有 token 的範圍，需要時請 Kairos 管理者建立新的 token。只讀取資料的整合（Grafana、報表腳本）維持 `read` 即可。"
 },
 "KRS-E6201": {
  "level": "E",
  "text": "稽核日誌完整性驗證失敗（`kairos audit verify` 結束代碼 9、API `POST /api/v1/audit/verify` 的 `broken`、每 24 小時的自我驗證）。「原因」寫出第一個斷點的編號與是哪一種：**雜湊鏈斷裂**——`hash_mismatch`（內容被修改）、`prev_mismatch`（前一筆被刪除或順序被改）、`missing_hash`（缺少雜湊）、`anchor_mismatch`（保留清除後的起點不符）、`head_mismatch`（最後幾筆被刪除）；**HMAC 不符**（0.7 起，稽核紀錄另以資料目錄的 `audit-hmac.key` 計算 HMAC）——`missing_hmac`（HMAC 起點之後的紀錄沒有 HMAC）、`hmac_mismatch`（紀錄或雜湊被整段重新計算）、`hmac_boundary_mismatch`（HMAC 的起點被移動）；**無法驗證 HMAC**——`hmac_key_unavailable`（金鑰檔遺失或無法讀取）、`hmac_key_mismatch`（金鑰檔與 HMAC 起點記錄的不同）。前兩類表示紀錄可能被修改、刪除或插入，處理方式是保全資料庫檔並與匯出或外送的鏈尾雜湊（`6202`）比對；第三類雜湊鏈本身完整，但無法排除整段被重新計算，處理方式是從備份還原 `audit-hmac.key`（`kairos db restore-keys`）。三類同號：都代表「無法證明稽核日誌沒有被竄改」，系統規則「稽核日誌完整性」（緊急）一律開事件；處理方式以「原因」區分（`decisions/INTEGRATION.md` §29）。HMAC 起點之前的紀錄（0.7 之前寫入）只有雜湊鏈，不是錯誤，驗證結果寫出邊界",
  "title": "稽核日誌完整性驗證失敗：第 12,391 筆的 HMAC 不符",
  "reason": "第 12,391 筆的雜湊鏈前後相接，但 HMAC 與以 `audit-hmac.key` 重新計算的值不同：紀錄被修改後，整條雜湊鏈被重新計算過（只有能改寫資料庫檔的人做得到）。",
  "fix": "1. 保全資料庫檔（複製 `kairos.db` 與 `kairos.db-wal`），不要再寫入。\n2. 與先前匯出的 JSONL（`kairos audit export`）或外送的鏈尾雜湊（`audit.chain_anchor`，`KRS-I6202`）比對，找出被修改、刪除或插入的紀錄。\n3. 原因是「HMAC 的金鑰檔遺失或無法讀取」或「金鑰檔與啟用時不同」時，雜湊鏈本身完整：從備份還原 `audit-hmac.key`（`kairos db restore-keys`）後再執行 `kairos audit verify`。"
 },
 "KRS-I6202": {
  "level": "I",
  "text": "稽核鏈尾的定期外送：每 `audit.anchor_log_minutes` 分鐘（預設 60；0 不寫，範圍 5～1440）寫一行固定日誌事件 `audit.chain_anchor`（`INFO`；`fields`：`last_id`、`last_hash`、`last_hmac`、`hmac_since`），經 syslog 或日誌收集器送到其他主機後，就是資料庫以外的副本，用來發現「改寫資料庫並重新計算整條鏈」或「刪除 HMAC 把鏈降級」。只在日誌，不在主控台、不推播、不寫稽核紀錄。2026-10-15 分配（工程暫定同號，`decisions/INTEGRATION.md` §29）",
  "title": "稽核鏈尾已記錄：第 12,408 筆",
  "reason": "Kairos 每 60 分鐘（`audit.anchor_log_minutes`）把最後一筆稽核紀錄的編號、雜湊與 HMAC 寫一行到日誌。這些行經 syslog 送到其他主機後，就是資料庫以外的副本：即使有人改寫資料庫並重新計算整條鏈，也會與外送的這一行對不上。",
  "fix": "不需要處理。請確認這些行確實送到另一台主機（`[log.syslog]` 或日誌收集器）並保存至少與稽核日誌保留期間一樣久。調查時取外送的某一行，確認資料庫中同一筆的雜湊相同、之後的紀錄能從它接上（`kairos audit verify`、`kairos audit export`）。"
 },
 "KRS-E7001": {
  "level": "E",
  "text": "授權檔簽章驗證失敗",
  "title": "授權檔簽章驗證失敗",
  "reason": "授權檔的數位簽章無法驗證：檔案在傳遞過程中被修改或截斷，或不是原廠簽發的授權檔。",
  "fix": "1. 重新從原廠提供的媒體（USB、Email 附件）複製完整的授權檔，不要用文書軟體開啟後另存。\n2. 仍失敗時請聯絡原廠，並提供授權編號與錯誤代碼 KRS-E7001。"
 },
 "KRS-E7002": {
  "level": "E",
  "text": "授權檔的伺服器識別碼不符",
  "title": "授權檔的伺服器識別碼不符",
  "reason": "授權檔綁定的伺服器識別碼與這台 Kairos 不同。授權檔只能用在申請時提供識別碼的那台伺服器。",
  "fix": "在 S22 下載「授權請求檔」（含這台伺服器的識別碼）提供給原廠重新簽發；更換硬體或重新安裝後識別碼會改變，也需要重新申請。"
 },
 "KRS-E7101": {
  "level": "E",
  "text": "授權節點數已滿，無法核准或註冊新主機",
  "title": "授權節點數已滿，無法核准新主機",
  "reason": "授權節點用量（持有有效憑證、且未除役的主機，加上憑證已過期但仍在寬限期內的主機）超過授權節點數 10% 以上。既有主機的監控與告警不受影響，只限制新增。",
  "fix": "除役不再使用的主機以釋出節點（寬限期內、確定不會再回來的主機也可以除役），或向原廠申請擴充授權並在 S22 匯入。"
 },
 "KRS-W8302": {
  "level": "W",
  "text": "主機時鐘與 Kairos 相差超過 5 分鐘（Pulse 安裝前檢查 PF03 以警告呈現、不中止安裝；註冊時（含過期寬限期續簽，2026-10-12）Kairos 以 `clock_skew` 暫不受理（不消耗 token、不計入失敗封鎖；Kairos 端彙總為一則資訊事件），時間校正後 Pulse 自動重送；服務執行期為錯誤（日誌 `clock.skew` 為 `ERROR`、同時寫 stderr，2026-10-13 定案，`pulse/log-ux.md` §4.5）。D27，`pulse/install-ux.md` §3.1、`pulse/enrollment-ux.md` §5.12）",
  "title": "3 台主機的註冊因時鐘偏差暫不受理",
  "reason": "這些主機送出註冊申請（或過期續簽）時附上的時間，與 Kairos 相差超過 5 分鐘（例如 plc-hmi-07 比 Kairos 慢 7 分 42 秒）。時間差這麼大時，Kairos 簽發的憑證在主機上會被當成「尚未生效」，所以先暫不受理。Kairos 沒有消耗 token 的使用次數、沒有建立待核准申請，也不計入失敗封鎖；這不是 token 或註冊端點的問題。",
  "fix": "1. 請現場修正這些主機的時間同步：Windows 以系統管理員身分執行 `net start w32time` 與 `w32tm /resync`；Linux 執行 `sudo timedatectl set-ntp true`。\n2. 修正後 Pulse 會在 5 分鐘內自動重新送出，不需要新的 token，也不需要重新安裝。\n3. 同一個網段有多台主機偏差時，檢查該網段使用的 NTP 伺服器，或防火牆是否阻擋 UDP 123。"
 }
};
