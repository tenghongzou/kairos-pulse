# Pulse

輕量級的**地端系統監控代理程式（on-premise monitoring agent）**，以 Rust 撰寫。

Pulse 在本機收集系統資訊（CPU、記憶體、磁碟、網路、元件溫度），並透過 **TLS 加密連線**週期性地將資料回報給中央伺服器，同時維持存活訊號（heartbeat）。

## 功能特色

- 🖥️ 收集系統資訊：CPU 使用率、記憶體、磁碟、網路流量、元件溫度
- 🔐 透過 TLS 加密回報系統資訊（JSON 格式）
- 🔁 連線失敗自動重試，採指數退避（exponential backoff）
- 💓 週期性存活訊號（heartbeat）
- ⚙️ 設定檔於編譯時嵌入二進位檔（單一執行檔即可部署）
- 📝 日誌同時輸出到終端與 `logs/pulse.log`

## 系統需求

- [Rust](https://www.rust-lang.org/)（2021 edition，建議使用最新穩定版）

## 建置與執行

```bash
# 建置
cargo build            # debug
cargo build --release  # release

# 執行
cargo run
# 或直接執行編譯後的二進位檔
./target/release/pulse
```

程式啟動後會：

1. 印出一次完整的系統資訊
2. 啟動 heartbeat 背景任務
3. 依設定的間隔，週期性地透過 TLS 將系統資訊回報給伺服器

> 若目標伺服器（預設 `127.0.0.1:8080`）尚未啟動，日誌會出現 `Connection refused` 並依退避策略重試，屬正常現象。

## 設定

設定檔為專案根目錄的 `config.toml`，於**編譯時**以 `include_str!` 嵌入二進位檔。**修改設定後需重新編譯**才會生效。

```toml
[server]
address = "127.0.0.1"          # 伺服器位址
port = 8080                    # 伺服器連接埠
domain = "localhost"           # TLS SNI / 憑證驗證用的主機名稱
accept_invalid_certs = true    # 是否略過 TLS 憑證驗證

[retry]
max_retries = 5                # 最大重試次數
initial_delay = 2              # 初始重試延遲（秒），之後每次加倍

[heartbeat]
interval = 1                   # heartbeat 間隔（秒）

[report]
interval = 5                   # 系統資訊回報間隔（秒）

[log]
level = "info"                 # 日誌級別：trace / debug / info / warn / error
```

### ⚠️ 安全性提醒

`server.accept_invalid_certs = true` 會**停用 TLS 憑證驗證**，僅供本機自簽憑證的開發測試使用。
**正式環境務必設為 `false`** 並填入正確的 `server.domain`，否則容易遭中間人攻擊（MITM）。啟用略過驗證時，程式會於日誌輸出警告。

## 回報資料格式

每次回報以 JSON 送出下列欄位：

```json
{
  "cpu_usage": 17.84,        // 全機 CPU 平均使用率（%）
  "total_memory": 49152,     // 總記憶體（MB）
  "used_memory": 37034,      // 已使用記憶體（MB）
  "hostname": "my-host"      // 主機名稱
}
```

## 專案結構

```
src/
├── main.rs            進入點：初始化日誌、讀取設定、啟動背景任務
├── config_manager.rs  設定管理（嵌入式 config.toml 解析）
├── system_info.rs     以 sysinfo 收集系統資訊
├── heartbeat.rs       週期性存活訊號
└── network.rs         TLS 連線、指數退避重試、週期性回報
config.toml            設定檔（編譯時嵌入）
```

## 日誌

- 同時輸出到終端（stdout）與 `logs/pulse.log`
- 級別由 `config.toml` 的 `[log] level` 控制
- heartbeat 為 `debug` 級別，在 `info` 級別下不會顯示

## 授權

尚未指定。
