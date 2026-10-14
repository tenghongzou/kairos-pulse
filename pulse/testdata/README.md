# 測試用憑證

**僅供單元測試使用**，不得用於任何實際部署。CA 的私鑰在產生後已刪除，因此無法再用這些 CA 簽發新憑證。

| 檔案 | 內容 |
|---|---|
| `root.pem` | 測試根 CA（自簽） |
| `inter.pem` | 由 `root.pem` 簽發的中繼 CA |
| `ca-bundle.pem` | `root.pem` + `inter.pem`（測試 `ca_cert` 可含多張） |
| `client-rsa.pem` | RSA 2048 客戶端憑證，由根 CA 直接簽發；SAN `DNS:6f1c0e4a-1b2c-4d3e-9f00-112233445566`，EKU `clientAuth` |
| `client-rsa.key` / `client-rsa.pkcs1.key` | 上述憑證的私鑰（PKCS#8 / PKCS#1） |
| `client-ec-chain.pem` | EC P-256 客戶端憑證（由中繼 CA 簽發）+ 中繼 CA；SAN 同上 |
| `client-ec.key` / `client-ec.sec1.key` | 上述憑證的私鑰（PKCS#8 / SEC1） |
| `not-pem.txt` | 非 PEM 內容（測試錯誤訊息） |
| `server-ca.pem` | 測試**伺服器** CA（自簽，EC P-256），與上面的客戶端 CA 不同；只信任 `root.pem` 時伺服器憑證不受信任 |
| `server.pem` / `server.key` | 由 `server-ca.pem` 簽發的伺服器憑證與私鑰（PKCS#8）：SAN `DNS:localhost`、`IP:127.0.0.1`，EKU `serverAuth`，`CA:FALSE`；供實際 TLS 握手測試使用 |
| `enroll-rsa-root.pem` | 測試 **RSA 2048** 根 CA（自簽，`CA:TRUE`；私鑰已刪除）：註冊通道測試「既有部署的 RSA 根 CA」 |
| `enroll-rsa-server.pem` / `enroll-rsa-server.key` | 由 `enroll-rsa-root.pem` 簽發的 EC P-256 伺服器憑證與私鑰（PKCS#8）：SAN `DNS:localhost`、`IP:127.0.0.1`，EKU `serverAuth`，`CA:FALSE`；假 8444 伺服器（`src/enroll/fake.rs`）使用 |
| `multi-san.pem` | 自簽客戶端憑證（只有憑證、沒有私鑰）：SAN `DNS:6f1c0e4a-1b2c-4d3e-9f00-112233445566`、`DNS:11111111-2222-4333-8444-555555555555`，EKU `clientAuth`；測試啟動時對多個 DNS SAN 發出警告 |

重新產生方式（OpenSSL 3.0+；Windows Git Bash 需先 `export MSYS_NO_PATHCONV=1`）：

```bash
ID=6f1c0e4a-1b2c-4d3e-9f00-112233445566
openssl req -x509 -newkey rsa:2048 -nodes -keyout root.key -out root.pem -days 36500 -subj "/CN=Pulse Test Root CA"
openssl req -new -newkey rsa:2048 -nodes -keyout inter.key -out inter.csr -subj "/CN=Pulse Test Intermediate CA" \
  -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign,cRLSign"
openssl x509 -req -in inter.csr -CA root.pem -CAkey root.key -CAcreateserial -out inter.pem -days 36500 -copy_extensions copyall
openssl req -new -newkey rsa:2048 -nodes -keyout client-rsa.key -out c1.csr -subj "/CN=$ID" \
  -addext "subjectAltName=DNS:$ID" -addext "extendedKeyUsage=clientAuth"
openssl x509 -req -in c1.csr -CA root.pem -CAkey root.key -CAcreateserial -out client-rsa.pem -days 36500 -copy_extensions copyall
openssl pkey -in client-rsa.key -traditional -out client-rsa.pkcs1.key
openssl req -new -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -keyout client-ec.key -out c2.csr -subj "/CN=$ID" \
  -addext "subjectAltName=DNS:$ID" -addext "extendedKeyUsage=clientAuth"
openssl x509 -req -in c2.csr -CA inter.pem -CAkey inter.key -CAcreateserial -out client-ec-leaf.pem -days 36500 -copy_extensions copyall
cat client-ec-leaf.pem inter.pem > client-ec-chain.pem
openssl ec -in client-ec.key -out client-ec.sec1.key
cat root.pem inter.pem > ca-bundle.pem
printf 'not a pem file\n' > not-pem.txt
rm -f *.csr *.srl root.key inter.key client-ec-leaf.pem
```

伺服器憑證（`server-ca.pem`、`server.pem`、`server.key`）：

```bash
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -keyout server-ca.key -out server-ca.pem \
  -days 36500 -subj "/CN=Pulse Test Server CA" \
  -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign,cRLSign"
openssl req -new -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -keyout server.key -out s.csr -subj "/CN=localhost" \
  -addext "subjectAltName=DNS:localhost,IP:127.0.0.1" -addext "extendedKeyUsage=serverAuth" \
  -addext "basicConstraints=critical,CA:FALSE"
openssl x509 -req -in s.csr -CA server-ca.pem -CAkey server-ca.key -CAcreateserial -out server.pem -days 36500 -copy_extensions copyall
rm -f s.csr *.srl server-ca.key
```

註冊通道的 RSA 根 CA（`enroll-rsa-*`）：

```bash
openssl req -x509 -newkey rsa:2048 -nodes -keyout root.key -out enroll-rsa-root.pem -days 36500   -subj "/CN=Pulse Test RSA Root CA" -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign,cRLSign"
openssl req -new -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -keyout enroll-rsa-server.key -out s.csr -subj "/CN=localhost"   -addext "subjectAltName=DNS:localhost,IP:127.0.0.1" -addext "extendedKeyUsage=serverAuth" -addext "basicConstraints=critical,CA:FALSE"
openssl x509 -req -in s.csr -CA enroll-rsa-root.pem -CAkey root.key -CAcreateserial -out enroll-rsa-server.pem -days 36500 -copy_extensions copyall
rm -f root.key s.csr *.srl
```

P-256、P-384 根 CA 的情境由測試以 rcgen 在執行時產生，不需要檔案。
