# 測試用 PKI

僅供 `cargo test` 使用，**不得用於任何實際部署**（`server.key` 為公開的測試金鑰）。

以 `openssl ca` 產生（EC P-256，效期至 2126 年）：

| 檔案 | 內容 |
|---|---|
| `test-ca.crt` | 測試用代理 CA |
| `client-a.crt` | SAN `DNS:6f1c0e4a-1b2c-4d3e-9f00-112233445566`、EKU clientAuth；已於 `crl-revoke-a.*` 撤銷 |
| `client-b.crt` | SAN `DNS:aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee`、EKU clientAuth；未撤銷 |
| `client-expired.crt` | 效期 2020-01-01～2021-01-01，已過期 |
| `crl-revoke-a.crl` / `crl-revoke-a.der` | 同一份 CRL 的 PEM 與 DER 格式，撤銷 `client-a.crt` |
| `crl-empty.crl` | 沒有撤銷任何憑證的 CRL（PEM） |
| `server.crt` / `server.key` | 自簽伺服器憑證（SAN `localhost`、`127.0.0.1`）與其測試金鑰 |

PEM 格式的 CRL 使用 `.crl` 副檔名，因為 `.gitignore` 忽略 `*.pem`。
