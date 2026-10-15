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
| `nat-ca.crt` | NAT 情境用的測試 CA（`Kairos NAT Test CA`，EC P-256，效期至 2126 年；私鑰產生後即刪除） |
| `nat-clients.bundle` | 由 `nat-ca.crt` 簽發的 40 個客戶端身分，依序為「憑證 PEM、私鑰 PEM」；第 i 個（1 起算）的 SAN 為 `DNS:4e470000-0000-4000-8000-<i 的 12 位十六進位>`、EKU clientAuth。**私鑰為公開的測試金鑰**。整合測試以它模擬同一個 NAT 後超過 32 台 agent（審計 A7） |

PEM 格式的 CRL 使用 `.crl` 副檔名、客戶端身分合併檔使用 `.bundle`，因為 `.gitignore` 忽略 `*.pem`。

`nat-*` 以下列方式產生（`openssl req -x509` 與 `openssl x509 -req`，不是 `openssl ca`）：

```sh
openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -out ca.key
openssl req -new -x509 -key ca.key -subj "/CN=Kairos NAT Test CA" -days 36500 -sha256 \
  -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign,cRLSign" -out nat-ca.crt
for i in $(seq 1 40); do
  id=$(printf "4e470000-0000-4000-8000-%012x" $i)
  openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -out c.key
  openssl req -new -key c.key -subj "/CN=$id" -out c.csr
  printf "basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=clientAuth\nsubjectAltName=DNS:%s\n" "$id" > c.ext
  openssl x509 -req -in c.csr -CA nat-ca.crt -CAkey ca.key -set_serial $((0x2000+i)) -days 36500 -sha256 -extfile c.ext -out c.crt
  cat c.crt c.key >> nat-clients.bundle
done
rm -f c.* ca.key
```

`rsa-root.key`：`kairos pki sign`／`gencrl` 以既有 RSA 根 CA 簽發的測試（`kairos/src/pki/ca.rs`）使用的 RSA 2048 私鑰（PKCS#8，**公開的測試金鑰**），以 `openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048` 產生；測試在執行時以它自簽 CA 憑證。
