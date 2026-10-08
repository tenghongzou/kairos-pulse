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
