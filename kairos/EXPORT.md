# 指標轉出：Prometheus exporter、OTLP、Grafana（商業審計 F7）

狀態：0.9.0〔草稿〕已實作（衝刺 8，分支 `sprint8/export`）。錯誤代碼 `KRS-E4311`～`KRS-E4315` 為**暫定**，待設計包分配（第 8 節）。

Kairos 收到的主機指標（Pulse 的 report，`crates/kairos-proto/METRICS.md`）可以轉出到既有的監控系統：

| 方式 | 方向 | 適合 |
|---|---|---|
| **Prometheus exporter**（`GET /api/v1/export/prometheus`） | Prometheus 來抓（pull） | 已有 Prometheus／Grafana、VictoriaMetrics、Thanos、Mimir 的環境 |
| **OTLP/HTTP 推送**（`[export.otlp]`） | Kairos 推送（push） | 已有 OpenTelemetry collector、Datadog／New Relic／Elastic 等 OTLP 相容的後端 |
| **Grafana** | 經由上面的 Prometheus | 附儀表板 `kairos/packaging/grafana-kairos-hosts.json`（第 4 節） |

兩者都**只轉出主機指標**（標準指標與 Pulse 送的自訂指標，另可選行程排行），不轉出告警、稽核與資產。Kairos 自身的狀態仍在 `/metrics`（審計 O3，README「健康檢查與 Prometheus」）。

## 1. 設計取捨

| 項目 | 決定 | 理由與替代方案 |
|---|---|---|
| exporter 的位置 | 主控台 API 的新端點 `/api/v1/export/prometheus`（9443、HTTPS） | 沿用主控台的 TLS、API token 與 RBAC，不必再開連接埠或另一套認證。另開純 HTTP listener 會讓主機清單在沒有 TLS 的連接埠上外流；`/metrics`（O3）只放 Kairos 自身的指標，兩者不混用（`/metrics` 的 token 是單一共用秘密，沒有群組範圍） |
| 認證與權限 | API token（或主控台工作階段），權限 **`hosts:read`**，依主機群組篩選；**不新增 scope** | 與 S04、趨勢 API 相同的資料、相同的權限：唯讀角色的 token 就能抓，群組範圍的使用者 token 只看得到自己的群組。新增「只能抓取」的權限需要自訂角色（v1.0 之後），目前用 viewer 角色的 token（不能寫入任何東西）已經是最小權限 |
| 資料來源 | 每台主機**最新一筆 report 的完整內容**（記憶體，連線保存成功後更新） | 不查 SQLite（每次抓取都查數千台的最新值太貴）；也不用狀態表的 `last_report` 摘要（少了溫度、封包、錯誤與自訂指標）。只在 exporter 啟用時保存；Kairos 重新啟動後在下一筆 report（預設 5 秒）就有資料 |
| 標籤 | 每個序列 `agent_id`、`hostname`、`host_group`（有時）＋指標本身的標籤 | 顯示名稱、tags、Pulse 版本、作業系統只放在 `kairos_host_info`（值 1，以 `group_left` 併入）：它們會變動或含高基數的字串，放在每個序列上會在改名時斷開所有序列 |
| 高基數 | 行程排行預設**不輸出**（`include_processes`） | 標籤含 pid 與行程名稱，行程重啟就是新序列；開啟時每台主機 CPU 與記憶體各前 N 名（預設 5） |
| 離線與過期 | `kairos_host_up` 一律輸出；離線或最後一筆超過 `stale_after_secs`（預設 300）的主機**不輸出指標**，樣本不帶時間戳記 | 不輸出就讓 Prometheus 依 staleness 規則把序列標為過期，圖表出現缺口，而不是一條停在最後值的直線。帶時間戳記的樣本不會被標為過期，且太舊的樣本會被拒絕（out of bounds），因此不帶；取樣時間另外以 `kairos_host_last_sample_timestamp_seconds` 提供 |
| 服務探索（`http_sd`／`file_sd`） | **不做** | Pulse 的 `/metrics` 只綁 loopback（Pulse README），讓 Prometheus 直接抓 agent 不可行；所有主機都由 Kairos 一個端點轉出，Prometheus 只需要一個 static target。大量主機時以 `?group=` 分成多個 job（第 2.6 節）即可分散 |
| OTLP 傳輸 | **OTLP/HTTP＋protobuf**；不做 gRPC、不壓縮 | gRPC 需要 HTTP/2（hyper 的 http2、h2）與 tonic／prost 的整串依賴；collector 一律同時提供 4318 的 HTTP。gzip 需要 flate2 等壓縮依賴；collector 接受未壓縮的本文（一批 8192 個資料點約 0.6 MB，第 5 節） |
| OTLP 編碼 | **手寫 protobuf 編碼器**（`kairos/src/export/proto.rs`，不到 200 行） | opentelemetry SDK／opentelemetry-proto 會帶進 tonic、prost、prost-build（建置時需要 protoc）與數十個傳遞依賴；指標只用到十來個訊息。**本功能沒有新增任何 crate**，`Cargo.lock` 與 `deny.toml` 不變 |
| OTLP 的名稱 | 預設對應 OpenTelemetry semantic conventions（`naming = "semconv"`），可改回 Kairos 的名稱（`kairos`） | Kairos 的 `system.filesystem.usage` 是百分比，semconv 的同名指標是 bytes；直接送原名會與 collector 的 hostmetrics receiver 衝突。對應表見第 3.3 節 |
| 設定的位置 | 常用的鍵可在主控台（S21「整合」）修改，存在資料庫 `settings` 表、優先於 `kairos.toml`；環境變數最優先且鎖定 | 與註冊端點模式、憑證政策相同的規則（`kairos/PKI.md` 第 8 節）。逾時、佇列、TLS 檔案等少改的鍵只在 `kairos.toml` |
| OTLP 的機密 | HTTP 標頭（例如 `Authorization`）由主控台輸入時以 `alerts-secrets.key` 加密存入資料庫；設定檔以 `headers_file` 指向另一個檔案 | 與告警通知管道的機密相同的處理（`kairos/ALERTS.md`）：API、日誌、稽核只出現名稱與遮蔽值；`kairos config show` 也不會印出 |
| Grafana | Prometheus 資料來源＋儀表板 JSON；**不做 Grafana plugin** | 見第 4 節 |

## 2. Prometheus exporter

### 2.1 啟用

- 主控台：**系統設定 → 整合 → 指標轉出**，勾選「提供 Prometheus 端點」→ 儲存（管理員，`export:write`）。
- 或 `kairos.toml`：

```toml
[export.prometheus]
enabled = true
stale_after_secs = 300        # 30..=86400
include_processes = false
processes_top_n = 5           # 1..=10（只在 kairos.toml）
```

未啟用時端點回 `404 export_disabled`（`KRS-E4315`）。

### 2.2 端點

`GET /api/v1/export/prometheus[?group=<群組>&group=…]`

| 項目 | 規則 |
|---|---|
| 驗證 | `Authorization: Bearer <API token>`（或主控台的工作階段 cookie）；權限 `hosts:read` |
| 範圍 | 全站範圍的 token 看到所有主機；群組範圍（使用者的群組角色）只看到該群組的主機。`group` 參數再篩選（取「或」，不分大小寫） |
| 格式 | 預設 Prometheus text 0.0.4（`text/plain; version=0.0.4`）；`Accept` 明確接受 `application/openmetrics-text` 時為 OpenMetrics 1.0（計數器的 `TYPE` 以 family 名稱宣告、`unknown` 型別、`# EOF`） |
| 其他參數 | 不接受（`400 invalid_parameter`）；只接受 `GET`／`HEAD` |
| 壓縮 | 不壓縮（Prometheus 接受未壓縮的回應） |

### 2.3 名稱與型別

Kairos 的名稱（`system.cpu.usage`）的 `.` 換成 `_`，依 OpenTelemetry 轉 Prometheus 的慣例加單位後綴，計數器加 `_total`：

| 單位 | 後綴 | 例 |
|---|---|---|
| `By` | `_bytes` | `system_memory_total_bytes`、`system_network_received_bytes_total` |
| `s` | `_seconds` | `system_uptime_seconds` |
| `ms` | `_seconds`（值 ÷ 1000） | `system_disk_io_time_seconds_total` |
| `%` | `_percent` | `system_cpu_usage_percent`、`system_filesystem_usage_percent` |
| `Cel` | `_celsius` | `system_temperature_celsius` |
| `1`、`{packets}` 等標註 | 不加 | `system_cpu_load_1m`、`system_network_packets_received_total`、`system_process_count` |

- 型別：`METRICS.md` 第 3 節的計數器為 `counter`（原始累計值，Prometheus 以 `rate()` 處理重置），量測值為 `gauge`；自訂指標（以及單位與標準不同的指標）為 `untyped`（OpenMetrics 為 `unknown`）。
- 自訂指標的名稱以 `kairos_host_`、`kairos_process_`、`kairos_exporter_` 開頭時加上 `custom_`，避免冒用 exporter 自己的序列。
- 同一台主機中對應到同一個名稱與標籤的第二個指標略過（Prometheus 會拒絕重複的序列）。

### 2.4 標籤與 exporter 自己的序列

| 序列 | 標籤 | 值 |
|---|---|---|
| 主機指標（上表） | `agent_id`、`hostname`、`host_group`（有群組時）＋指標的標籤（`mountpoint`、`fstype`、`device`、`sensor`…） | 最新一筆 report 的值 |
| `kairos_host_up` | `agent_id`、`hostname`、`host_group` | 1：與 Kairos 連線中；0：離線、尚未連線 |
| `kairos_host_info` | 同上＋`display_name`、`agent_version`、`os_family`、`tags`（排序後以 `,` 串接） | 固定 1 |
| `kairos_host_last_sample_timestamp_seconds` | 同 `kairos_host_up` | 最後一筆 report 的取樣時間（Unix 秒；這次啟動後收到的） |
| `kairos_exporter_hosts` | `state`：`fresh`、`stale`、`offline`、`no_data` | 各狀態的主機數 |
| `kairos_process_cpu_usage_percent`、`kairos_process_memory_bytes`（`include_processes`） | 主機標籤＋`pid`、`process` | 行程排行（占整台主機的 CPU %、常駐記憶體 bytes） |

指標的標籤與主機標籤重名時改為 `exported_<名稱>`（Prometheus 的慣例）；不合法的字元換成 `_`。序列的 `instance` 是 Kairos 本身，依主機查詢請用 `hostname` 或 `agent_id`。

PromQL 範例：

```promql
# 離線的主機
kairos_host_up == 0
# 記憶體使用率
100 * system_memory_used_bytes / system_memory_total_bytes
# 網路接收（bytes/s）
rate(system_network_received_bytes_total[5m])
# 帶顯示名稱與標籤
system_cpu_usage_percent * on(agent_id) group_left(display_name, tags) kairos_host_info
```

### 2.5 `scrape_config`

完整範例見 `kairos/packaging/prometheus-export.example.yml`：

```yaml
scrape_configs:
  - job_name: kairos-hosts
    scheme: https
    metrics_path: /api/v1/export/prometheus
    scrape_interval: 30s
    scrape_timeout: 25s
    authorization:
      type: Bearer
      credentials_file: /etc/prometheus/kairos.token   # kairos api-token create --name prometheus --role viewer
    tls_config:
      ca_file: /etc/prometheus/kairos-console-ca.pem
    static_configs:
      - targets: ['kairos.example.com:9443']
```

- token 用 viewer 角色（預設）即可；到期前 14 天 `kairos api-token list` 與 S21 會提示輪替。
- 主控台預設只綁 `127.0.0.1:9443`（`console.listen`）；Prometheus 在別台時改綁管理網段並以防火牆限制來源。

### 2.6 大量主機

每台主機約 40～50 個序列（0.8 的標準指標、2～3 個掛載點／磁碟／介面），一次抓取的大小與時間見第 5 節。超過數千台時：

- `scrape_interval` 30～60 秒（Pulse 預設每 5 秒一筆，抓更密也只是同一個值）、`scrape_timeout` 留足時間；
- 以 `params: {group: [...]}` 依主機群組分成多個 job（或多台 Prometheus 分擔），每次抓取只產生該群組的輸出；
- 輸出在主控台的 blocking 執行緒上產生，不佔用 API 的工作執行緒；Kairos 不快取輸出（每次抓取都是當下的狀態）。

## 3. OTLP 推送

### 3.1 設定

```toml
[export.otlp]
enabled = true
endpoint = "https://otel-collector.example.com:4318/v1/metrics"
headers_file = "/opt/kairos/otlp-headers"   # 每行「名稱: 值」，例如 Authorization: Bearer …（chmod 600）
ca_file = ""                  # 私有 CA（PEM）；不提供略過驗證
client_cert = ""              # collector 要求 mTLS 時
client_key = ""
interval_secs = 10            # 1..=3600
timeout_secs = 10             # 1..=60
max_batch_points = 8192       # 100..=100000
queue_max_points = 500000     # 1000..=10000000，不可小於 max_batch_points
drop_policy = "oldest"        # oldest | newest
max_retries = 5               # 0..=20
naming = "semconv"            # semconv | kairos
resource_attributes = ["deployment.environment=production"]
```

主控台可修改 `enabled`、`endpoint`、`interval_secs`、`naming` 與 HTTP 標頭（第 6 節）；其他鍵只在 `kairos.toml`（重新啟動後生效）。

### 3.2 傳輸、批次、重試與丟棄

- 每一筆保存成功的 report（含 durable 補送的舊資料，帶原本的取樣時間）進入記憶體佇列；推送工作每 `interval_secs` 秒、或佇列累積到 `max_batch_points` 時取出一批（以 report 為單位，一筆不拆開）送出。
- `POST <endpoint>`、`Content-Type: application/x-protobuf`、`User-Agent: Kairos/<版本>`＋自訂標頭；HTTP/1.1、一個請求一條連線。TLS：作業系統的 CA＋`ca_file`，可帶用戶端憑證。
- **重試**：連線失敗、逾時、HTTP 429／502／503／504 → 退避 1、2、4… 秒（最多 30 秒；429／503 的 `Retry-After` 優先，最多 300 秒），最多 `max_retries` 次，用完時丟棄這一批（`KRS-E4311`）。重試期間新的 report 繼續進佇列。
- **拒絕**：其他 4xx 不重試、直接丟棄（`KRS-E4312`，錯誤本文 `google.rpc.Status` 的訊息寫入狀態與日誌）；2xx 但回應帶 `partial_success.rejected_data_points` 時，被拒的資料點計入「collector 拒絕」並記 `WARN`。
- **佇列已滿**：`drop_policy = "oldest"`（預設）丟掉最舊的、`newest` 丟掉新收到的（`KRS-W4313`，每分鐘最多記一行 `WARN`）。
- **日誌**：第一次失敗記 `WARN`（之後連續的失敗只記 `DEBUG`），恢復時記 `INFO`；丟棄一批每分鐘最多記一行。錯誤訊息中的標頭值以 `••••` 取代。
- **關機**：佇列中的資料再送一次（最多 2 秒、不重試），其餘丟棄並記 `INFO`。
- 停用 OTLP（主控台或重新啟動）時清空佇列。

### 3.3 名稱與屬性的對應（`naming = "semconv"`）

| Kairos | OTLP | 單位 | 型別 | 屬性 |
|---|---|---|---|---|
| `system.cpu.usage`（%） | `system.cpu.utilization`（÷ 100） | `1` | Gauge | |
| `system.cpu.load_1m`／`5m`／`15m` | `system.cpu.load_average.1m`／`5m`／`15m` | `{thread}` | Gauge | |
| `system.memory.total` | `system.memory.limit` | `By` | Gauge | |
| `system.memory.used` | `system.memory.usage` | `By` | Gauge | `system.memory.state=used` |
| `system.memory.available` | `system.linux.memory.available` | `By` | Gauge | |
| `system.swap.used` | `system.paging.usage` | `By` | Gauge | `system.paging.state=used` |
| `system.filesystem.total` | `system.filesystem.limit` | `By` | Gauge | `system.filesystem.mountpoint`、`system.filesystem.type` |
| `system.filesystem.used`／`available` | `system.filesystem.usage` | `By` | Gauge | ＋`system.filesystem.state=used`／`free` |
| `system.filesystem.usage`（%） | `system.filesystem.utilization`（÷ 100） | `1` | Gauge | 同上 |
| `system.disk.read`／`written` | `system.disk.io` | `By` | Sum（cumulative、單調） | `system.device`、`disk.io.direction=read`／`write` |
| `system.disk.read_ops`／`write_ops` | `system.disk.operations` | `{operation}` | Sum | 同上 |
| `system.disk.io_time`（ms） | `system.disk.io_time`（÷ 1000） | `s` | Sum | `system.device` |
| `system.network.received`／`transmitted` | `system.network.io` | `By` | Sum | `network.interface.name`、`network.io.direction=receive`／`transmit` |
| `system.network.packets_*` | `system.network.packets` | `{packet}` | Sum | 同上 |
| `system.network.errors_*` | `system.network.errors` | `{error}` | Sum | 同上 |
| `system.process.count` | `system.process.count` | `{process}` | Gauge | |
| `system.uptime` | `system.uptime` | `s` | Gauge | |
| `system.swap.total`、`system.temperature`、自訂指標 | 原名 | 原單位 | 計數器為 Sum，其他 Gauge | 原標籤 |

- 計數器的 `start_time_unix_nano` 是開機時間（取樣時間 − 同一筆的 `system.uptime`；沒有時省略），collector 依此判斷重置。
- 整數值以 `as_int`、小數與換算過的值以 `as_double` 送出。
- `naming = "kairos"` 時一律原樣（名稱、單位、標籤與 `METRICS.md` 相同），計數器仍為 Sum。
- `ResourceMetrics.schema_url` 為 `https://opentelemetry.io/schemas/1.26.0`（semconv 時）。instrumentation scope 為 `kairos`／Kairos 的版本。

**resource 屬性**（每台主機一個 `ResourceMetrics`）：`host.name`（report 的主機名稱）、`host.id`（`agent_id`）、`host.arch`（`amd64`、`arm64`…，來自 hello）、`os.type`（`linux`、`windows`，來自資產）、`kairos.host.group`、`kairos.host.display_name`、`kairos.host.tags`（`,` 串接），再加上 `resource_attributes`。主機清冊每 60 秒讀一次（JSONL 後端沒有清冊，只有前兩項）。

### 3.4 collector 設定範例

```yaml
receivers:
  otlp:
    protocols:
      http:
        endpoint: 0.0.0.0:4318
        tls:                       # 建議；Kairos 端以 ca_file 信任
          cert_file: /etc/otelcol/server.pem
          key_file: /etc/otelcol/server.key
exporters:
  prometheusremotewrite:
    endpoint: https://mimir.example.com/api/v1/push
service:
  pipelines:
    metrics:
      receivers: [otlp]
      exporters: [prometheusremotewrite]
```

## 4. Grafana

**建議：Prometheus 資料來源＋儀表板 JSON**（`kairos/packaging/grafana-kairos-hosts.json`）。

1. Prometheus 依第 2.5 節抓取 Kairos。
2. Grafana：Dashboards → New → Import → 上傳 `grafana-kairos-hosts.json`（發佈包根目錄也有一份） → 選擇 Prometheus 資料來源。
3. 儀表板有「群組」「主機」兩個變數，面板：線上／離線主機數、最高 CPU 與磁碟使用率、過期的主機、主機清單（`kairos_host_info`）、CPU、記憶體、負載、置換空間、磁碟使用率、磁碟忙碌比例、磁碟讀寫、網路流量、運作時間、行程數。

**評估過的替代方案**：

| 方案 | 結論 |
|---|---|
| Grafana Infinity／JSON API 資料來源直接查 `/api/v1/hosts/{id}/metrics/{name}` | **不建議作為主要方式**：每個面板、每台主機各一個 HTTP 請求，沒有跨主機的聚合（topk、sum by group），時間範圍與解析度要自己對應 `from`／`to`／`resolution`；適合在沒有 Prometheus 的環境看單一主機的長期趨勢（Kairos 的 1 小時彙總保留 400 天）。API 規格見 `kairos/API.md` 第 5.3 節，以 viewer 角色的 token 設定 Infinity 的 Bearer 驗證即可 |
| Grafana plugin（自訂資料來源） | **不做**：要另外維護一個 Go＋TypeScript 專案、簽章與 Grafana 版本相容性，收益與 Prometheus 路線重疊 |
| OTLP → collector → Prometheus／Mimir → Grafana | 可行（第 3 節）；名稱為 semconv（`system_cpu_utilization` 等），附的儀表板是 exporter 的名稱，需要自行調整查詢 |

## 5. 效能量測

`cargo test -p kairos --release -- --ignored --nocapture export_bench`（`kairos/src/export/bench.rs`；kp-build：4 vCPU、4 GB，與另一條建置線同時執行）。10,000 台主機、每台 43 個指標（3 個掛載點、2 個介面、2 個磁碟，0.8 的標準指標），10% 離線；最佳 5 次：

| 項目 | 10,000 台 | 換算 1,000 台 |
|---|---|---|
| 每台主機保存的最新一筆（記憶體，只在 exporter 啟用時） | 約 7 KB／台，合計約 69 MB | 約 7 MB |
| Prometheus text 0.0.4 輸出 | 359 ms、387,000 個序列、56.9 MB | 約 36 ms、5.7 MB |
| OpenMetrics 輸出 | 305 ms、56.9 MB | 約 31 ms |
| OTLP 編碼（semconv） | 430,000 個資料點 521 ms（約 83 萬點／秒）；每批 8,192 點約 0.65 MB | — |

- 一次抓取約 5.7 KB／台：1,000 台以下用預設的 30 秒間隔即可；數千台以上依第 2.6 節分群組抓取，或拉長間隔。Prometheus 端的解析成本通常大於 Kairos 的產生成本（同一份輸出以 `kp_obs::prom::parse` 嚴格解析需 1.9 秒）。
- OTLP：以預設每 5 秒一筆 report 估算，1,000 台約每秒 8,600 個資料點，每 10 秒約 86,000 點、分 11 個請求（各約 0.65 MB）；編碼成本約為單一核心的 1%。

## 6. 主控台與 API

### 6.1 S21「整合」

系統設定 →「整合」分頁的「指標轉出」卡片（管理員與稽核員看得到「整合」分頁；維運者只有「連線與安全」）：

- Prometheus：是否啟用、端點、最近一次抓取（時間、主機數、序列數、大小、耗時）、抓取次數；可修改啟用、過期門檻、行程排行。
- OTLP：狀態（未啟用／沒有端點／等待第一次推送／正常／推送失敗）、佇列、送出、丟棄（佇列已滿／重試用完／collector 拒絕）、最近一次成功與錯誤（代碼）；可修改啟用、端點、間隔、名稱對應、HTTP 標頭（不更換／更換／清除主控台的值；值只能更換不能檢視），「測試連線」。
- 每個鍵都顯示來源（環境變數／主控台／設定檔／內建預設）；環境變數設定的鍵唯讀並寫出變數名稱；主控台的值可「改用設定檔的值」。
- 儲存為 L1 確認（列出變更，原因選填），寫入稽核（`settings` 類別、動作 `export.change`，標頭只記名稱）。
- 只有 `export:write`（管理員）能修改與測試連線；稽核員唯讀。

### 6.2 端點

| 端點 | 權限 | 說明 |
|---|---|---|
| `GET /api/v1/export/prometheus` | `hosts:read` | 第 2.2 節 |
| `GET /api/v1/export/settings` | `settings:read` | 生效的設定、來源與狀態 |
| `PUT /api/v1/export/settings` | `export:write`（新權限，只有管理員） | 修改主控台可修改的鍵 |
| `POST /api/v1/export/otlp/test` | `export:write` | 以已儲存的設定送出一個空的 `ExportMetricsServiceRequest`（collector 回 2xx 即成功，不寫入資料）；寫入稽核 `export.otlp_test` |

`PUT` 的本文（只送要改的鍵；`null` ＝ 刪除資料庫的值、改用設定檔的值）：

```json
{
  "prometheus": {"enabled": true, "stale_after_secs": 300, "include_processes": false},
  "otlp": {
    "enabled": true,
    "endpoint": "https://otel-collector:4318/v1/metrics",
    "interval_secs": 10,
    "naming": "semconv",
    "headers": [{"name": "Authorization", "value": "Bearer …"}]
  },
  "reason": "接上 Grafana Cloud"
}
```

- `otlp.headers`：陣列為「整組取代」（可為空陣列＝不帶標頭），`null` 為刪除主控台的值（改用 `headers_file`）；最多 16 個，不可自訂 `Host`、`Content-Type`、`Content-Length`、`Transfer-Encoding`、`Connection`、`User-Agent`。
- 錯誤：不合法的值、未知的欄位、啟用 OTLP 卻沒有端點 → `400 invalid_parameter`（`KRS-E4314`，`details.parameter` 為設定鍵）；鍵由環境變數設定 → `409 locked_by_environment`（`details.variable`；標頭看 `KAIROS__EXPORT__OTLP__HEADERS_FILE`）；`alerts-secrets.key` 無法使用時不能保存標頭 → `503 secrets_unavailable`。
- 回應與 `GET` 相同：

```json
{"data": {
  "prometheus": {"enabled": true, "path": "/api/v1/export/prometheus", "stale_after_secs": 300, "include_processes": false, "processes_top_n": 5,
                 "status": {"scrapes": 12, "last_scrape_at": "…", "last_duration_ms": 41, "last_hosts": 1200, "last_series": 61000, "last_bytes": 7340032}},
  "otlp": {"enabled": true, "endpoint": "…", "protocol": "http/protobuf", "interval_secs": 10, "timeout_secs": 10, "naming": "semconv",
           "max_batch_points": 8192, "queue_max_points": 500000, "drop_policy": "oldest", "max_retries": 5,
           "headers": [{"name": "Authorization", "value": "••••"}], "headers_source": "console", "ca_file": "", "client_certificate": false,
           "resource_attributes": [],
           "status": {"state": "ok", "queue_points": 0, "sent_points": 120000, "requests_ok": 40, "requests_failed": 1,
                      "dropped_points": {"queue_full": 0, "failed": 0, "rejected": 0}, "consecutive_failures": 0,
                      "last_attempt_at": "…", "last_success_at": "…", "last_request_ms": 35, "last_request_bytes": 412345,
                      "last_error": {"code": "KRS-E4311", "message": "…", "at": "…"}}},
  "settings": [{"key": "export.otlp.endpoint", "value": "…", "source": "console", "env_var": "KAIROS__EXPORT__OTLP__ENDPOINT",
                "locked": false, "updated_by": "user:admin", "updated_at": "…"}],
  "problems": []
}}
```

`state`：`disabled`、`not_configured`（啟用但沒有端點）、`pending`（還沒有成功過）、`ok`、`failing`（最近一次失敗）。`problems`：標頭檔無法讀取、主控台的標頭無法解開（金鑰檔遺失）等，啟動時也記 `WARN`（`KRS-E4314`）。

`GET /api/v1/settings`（S21 的設定表）與 `kairos config show --effective` 只列出 `export.*` 在設定檔與環境變數的值；主控台的覆寫值以本節的端點查看（指標轉出的鍵不加入 `crate::settings::CONSOLE_KEYS`，與註冊、憑證政策的重設流程分開）。

### 6.3 Kairos 自身的 `/metrics`（新增）

| 名稱 | 種類 | 說明 |
|---|---|---|
| `kairos_export_prometheus_enabled` | gauge | exporter 是否啟用 |
| `kairos_export_prometheus_scrapes_total` | counter | 成功的抓取次數 |
| `kairos_export_otlp_enabled` | gauge | OTLP 是否推送 |
| `kairos_export_otlp_queue_points` | gauge | 佇列中的資料點 |
| `kairos_export_otlp_sent_points_total` | counter | collector 已接受的資料點 |
| `kairos_export_otlp_requests_total{result}` | counter | `ok`、`failed`（含重試） |
| `kairos_export_otlp_dropped_points_total{reason}` | counter | `queue_full`、`failed`、`rejected` |
| `kairos_export_otlp_last_success_timestamp_seconds` | gauge | 最近一次成功（0：還沒有） |

告警建議：`increase(kairos_export_otlp_dropped_points_total[15m]) > 0`、`time() - kairos_export_otlp_last_success_timestamp_seconds > 300`。

## 7. 實作

| 檔案 | 內容 |
|---|---|
| `kairos/src/config/export.rs` | `[export]` 的設定與檢查 |
| `kairos/src/export/mod.rs` | `ExportHub`（最新值、佇列、狀態、計數）、`/metrics` 的部分 |
| `kairos/src/export/settings.rs` | 生效值（環境變數 ＞ 資料庫 ＞ 設定檔 ＞ 預設）、標頭的加密 |
| `kairos/src/export/prom.rs` | Prometheus text／OpenMetrics 的輸出 |
| `kairos/src/export/otlp.rs` | 名稱對應、protobuf 編碼、HTTP 用戶端、推送工作 |
| `kairos/src/export/proto.rs` | protobuf 編碼器（與測試用的解碼器） |
| `kairos/src/api/export.rs` | 主控台的端點 |
| `kairos/web/assets/js/screens/admin-export.js` | S21「整合」的卡片 |
| `kairos/packaging/grafana-kairos-hosts.json`、`kairos/packaging/prometheus-export.example.yml` | 儀表板與抓取範例 |

既有程式的改動只有接線：連線保存 report 後交給 `ExportHub`（`connection.rs`）、狀態表持有它（`registry.rs`）、啟動與關機（`main.rs`）、路由（`api/mod.rs`）、新權限 `export:write`（`rbac.rs`）、`/metrics`（`probe.rs`）。**資料庫 schema 不變**（沿用 `settings` 表，v8）。

## 8. 待決事項與需要設計包配合的項目

1. **錯誤代碼**（暫定，子類別 43「匯出、匯入與備份」的空號）：`KRS-E4311` OTLP 推送失敗（無法連線、逾時、TLS、5xx／429，重試用完時丟棄）、`KRS-E4312` collector 拒絕資料（4xx、`partial_success`）、`KRS-W4313` OTLP 佇列已滿而丟棄、`KRS-E4314` 指標轉出的設定無效、`KRS-E4315` Prometheus exporter 未啟用。需要寫入 `copywriting.md` §3.3 並重新產生 `error-codes.js`（目前主控台以 API 的訊息顯示）。
2. **權限**：新權限 `export:write`（只有管理員）需要加入 `ia.md` §4.2 的權限矩陣與 S16 的權限表。
3. **S21「整合」**：設計包的線框沒有「指標轉出」卡片；本版依 S21 既有的來源標示、L1 確認、機密欄位（S11 的「只能更換不能檢視」）實作，需要設計組補線框與文案審閱。
4. **稽核動作**：`export.change`、`export.otlp_test`（`settings` 類別）需要加入 S18 的動作清單。
