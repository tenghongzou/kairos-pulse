//! Kairos 與 Pulse 共用的可觀測性元件（商業審計 O3、O8、S7；設計包 `pulse/log-ux.md`、
//! `kairos/screens.md` S20 與 §24 #8）。
//!
//! | 模組 | 內容 |
//! |---|---|
//! | [`logfmt`] | 日誌行的格式：`text`（RFC 3339 帶時區）、`json`（NDJSON，UTC）、`legacy`（0.5 以前的格式） |
//! | [`syslog`] | RFC 5424 的格式與外送（UDP、TCP、本機 socket），在背景執行緒送出、不阻塞記錄日誌的執行緒 |
//! | [`prom`] | Prometheus 文字格式（0.0.4）的輸出、原子計數器與直方圖，以及測試用的嚴格解析器 |
//! | [`archive`] | 診斷包用的 tar.gz 產生器（不依賴外部套件的 deflate） |
//! | [`check`] | 多項檢查（`--check-config`）的結果、人看的輸出、`--json` 與結束代碼 |
//! | [`bundle`] | 診斷包的共用部分：遮蔽後加入、日誌收集與大小上限、檔案清單、外部命令、以 0600 寫出 |
//! | [`redact`] | 診斷包與日誌的敏感資料遮蔽（私鑰、token、密碼、bearer），可選擇把主機名稱與 IP 換成代號 |
//!
//! 本 crate 不讀設定檔、不決定措辭以外的產品行為；各產品把設定轉成這裡的型別。

#![forbid(unsafe_code)]

pub mod archive;
pub mod bundle;
pub mod check;
pub mod logfmt;
pub mod prom;
pub mod redact;
pub mod syslog;
