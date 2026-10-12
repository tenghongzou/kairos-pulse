# 發佈流程

本文件給發佈負責人使用：版本規則、打 tag、建置、簽章與發佈。客戶端的驗證步驟見 `kairos/packaging/INSTALL.md`、`pulse/packaging/INSTALL.md` 第 2 節；版本相容矩陣與支援期限見 `CHANGELOG.md`。

## 1. 版本規則

- Kairos 與 Pulse 各自採用 [SemVer](https://semver.org/lang/zh-TW/) `MAJOR.MINOR.PATCH`，版本號唯一的來源是產品 `Cargo.toml` 的 `[package] version`。
  - `--version` 與啟動日誌第一行印出 `<名稱> <版本> (<commit>)`，例如 `kairos 0.2.0 (1a2b3c4)`；Pulse 在 hello 的 `agent_version` 只送版本號。
  - 發佈包檔名為 `<名稱>-<版本>-<commit>-linux-x86_64.tar.gz`，打包腳本會確認 `--version` 的輸出與檔名完全一致。
- **1.0 之前（0.x）**：破壞性變更升 MINOR（0.2.x → 0.3.0），其餘新增與修正升 PATCH。**1.0 之後**依標準 SemVer：破壞性變更升 MAJOR。
- **破壞性變更**指客戶升級時必須做事、否則會出問題的變更，例如：設定欄位的預設值改變或移除、原本接受的設定被拒絕、協定不相容、結束代碼改變、資料檔或日誌的格式與權限改變、systemd unit 需要更新、需要調整監控門檻。每一項都要寫進 `CHANGELOG.md` 的「破壞性變更與升級步驟」表。
- 兩個產品通常同時發佈相同的版本號，但可以只發佈其中一個（例如只修 Pulse 時發佈 Pulse 0.2.1，Kairos 維持 0.2.0）；相容性以 `CHANGELOG.md` 的相容矩陣為準，不以版本號是否相同判斷。
- **共用 crate**（`crates/kairos-proto`、`crates/date-guard`）是 `publish = false` 的內部 crate，不單獨發佈、不打 tag，只出現在 SBOM 中。版本依其 Rust API 的 SemVer 變動（目前為 0.1.0，第一次發佈前不再調整；之後 API 有破壞性變更時升 MINOR）。**協定版本**（hello 的 `proto` 欄位）由 `crates/kairos-proto/PROTOCOL.md` 定義，與任何 crate 的版本無關。
- `0.2.0` 之前的建置一律是 `0.1.0 (<commit>)`，只能以 commit 區分（對照表見 README「歷史來源」與各產品 README 的「升級注意事項」）。

## 2. 發佈步驟

以下以 Kairos 0.2.0、Pulse 0.2.0 為例。

### 2.1 準備發佈 commit

1. 從整合分支（目前為 `sprint1/integration`，之後為 `main`）開分支，例如 `release/0.2.0`。
2. 確認兩個產品 `Cargo.toml` 的 `version`，執行 `cargo metadata --format-version 1 >/dev/null` 更新 `Cargo.lock`；`git diff Cargo.lock` 只能有 `kairos`、`pulse` 等 workspace 成員的版本變動。
3. 更新 `CHANGELOG.md`：
   - `## [Unreleased]` 改為 `## [Kairos 0.2.0、Pulse 0.2.0] - YYYY-MM-DD`，上方留一個新的空 `## [Unreleased]`。
   - 更新「版本相容矩陣」，移除已確定項目的〔草稿〕標記。
4. 提交、開 PR，等 CI 全綠（`check`、`deny`、`package`）。`package` job 的 artifact（`release-<commit>`）就是這個 commit 的未簽章發佈包。
5. 合併到整合分支後，記下合併後的 commit（以下稱 `<release-commit>`）；之後的步驟都在這個 commit 上進行。

### 2.2 打 tag

每個產品一個 annotated tag，名稱 `<名稱>-v<版本>`；打包腳本會檢查 HEAD 上的 `<名稱>-v*` tag 與 `Cargo.toml` 的版本一致。

```bash
git switch --detach <release-commit>
git tag -a kairos-v0.2.0 -m "Kairos 0.2.0"
git tag -a pulse-v0.2.0  -m "Pulse 0.2.0"
git push origin kairos-v0.2.0 pulse-v0.2.0
```

建議以個人的 SSH 金鑰簽署 tag（`git tag -s`，事先 `git config gpg.format ssh`、`git config user.signingkey ~/.ssh/id_ed25519.pub`），**不要**使用下一節的發佈金鑰：發佈金鑰只用來簽 `SHA256SUMS`，保存在離線的位置。tag 一旦推送就不再移動；發現問題時發佈新的 PATCH 版本。

### 2.3 建置與簽章

在乾淨的 checkout 上執行（Windows 請用 Git Bash；需要 Docker、cargo、cargo-deny）：

```bash
git switch --detach kairos-v0.2.0          # 與 pulse-v0.2.0 是同一個 commit
SIGNING_KEY=/離線媒體/kairos-pulse-release sh packaging/release.sh
```

`packaging/release.sh` 依序：

1. 確認工作目錄乾淨（有未提交的變更時拒絕；發佈包帶 `-dirty` 時也拒絕簽章）。
2. 執行 `cargo fmt --check`、`cargo clippy -D warnings`、`cargo test`、`cargo deny check`，任一失敗即停止。`cargo test` 的步驟與 CI 相同：先 `cargo test --workspace --exclude kp-e2e`，再 `cargo build -p kairos -p pulse --bins` 建置執行檔，最後單獨執行端對端測試 `cargo test -p kp-e2e`（失敗時可從輸出分辨是單元／整合測試還是端對端測試）。
3. 刪除 `dist/` 中舊的 `kairos-*`、`pulse-*`、`SHA256SUMS*`，打包兩個產品（`kairos/packaging/build-release.sh`、`pulse/packaging/build-release.sh`，共用實作 `packaging/build-release-common.sh`）。
4. 以 `packaging/sign-release.sh` 簽署合併版 `SHA256SUMS`，並以 `<私鑰>.pub` 驗證剛產生的簽章。

產出（`dist/`）：

| 檔案 | 說明 |
|---|---|
| `kairos-0.2.0-<commit>-linux-x86_64.tar.gz`、`pulse-0.2.0-<commit>-linux-x86_64.tar.gz` | 發佈包（含 `THIRD-PARTY-LICENSES` 與 `<名稱>.cdx.json`） |
| `kairos-0.2.0-<commit>-linux-x86_64.cdx.json`、`pulse-…cdx.json` | SBOM（與 tar 內相同），供客戶匯入掃描工具 |
| `SHA256SUMS` | 以上四個檔案的 SHA-256 |
| `SHA256SUMS.sig` | `SHA256SUMS` 的簽章 |

### 2.4 與 CI 的建置比對

建置在 digest 固定的容器中進行、來源取自 `git archive`，同一個 commit 在本機與 CI 的產物應完全相同。發佈前下載 `package` job 的 artifact，比對兩份 `SHA256SUMS`：

```bash
diff dist/SHA256SUMS <下載的 artifact>/SHA256SUMS && echo 與 CI 一致
```

不一致時不要發佈，先以 `sh packaging/check-reproducible.sh` 找出差異（見第 5 節）。

### 2.5 發佈

把 `dist/` 的六個檔案上傳到發佈處，公告中附上版本、`CHANGELOG.md` 對應的段落與發佈公鑰的指紋。`allowed_signers` 不要和發佈包放在同一個地方（見第 3 節）。

## 3. 發佈金鑰

簽章工具為 `ssh-keygen -Y sign`（OpenSSH 8.1 以上）：Git for Windows、Windows 11、macOS 與主流 Linux 都已內建，發佈者與客戶都不需要另外安裝工具，可完全離線驗證，公鑰只是一行 `allowed_signers`。其他候選工具的比較：

| 工具 | 不採用的原因 |
|---|---|
| minisign | 發佈端與每個客戶端都要另外安裝（RHEL 需要 EPEL，Windows 需要 scoop／winget） |
| cosign | 需要安裝；keyless 模式依賴 Sigstore 線上服務，不適合離線的地端環境 |
| GPG | Linux 上大多已有，但 keyring、信任模型與「未經認證的簽章」警告對客戶不友善，金鑰管理較複雜 |

ssh-keygen 的限制是 OpenSSH 8.0 以下（RHEL／Rocky 8、Debian 10）沒有 `-Y verify`，INSTALL 已說明改在管理主機上驗證。

簽章參數（改變時要同步更新兩份 INSTALL.md 與 `packaging/sign-release.sh`）：

| 項目 | 值 |
|---|---|
| namespace | `kairos-pulse-release` |
| principal | `release@kairos-pulse`（`allowed_signers` 的第一欄） |
| 演算法 | Ed25519 |

**產生正式金鑰**（只做一次，在離線或受保護的主機上，**不要**放在 repo 內；`sign-release.sh` 會拒絕 repo 內的私鑰）：

```bash
ssh-keygen -t ed25519 -C "kairos-pulse release key" -f /離線媒體/kairos-pulse-release   # 設定密碼
printf 'release@kairos-pulse namespaces="kairos-pulse-release" %s\n' \
  "$(cat /離線媒體/kairos-pulse-release.pub)" > allowed_signers
ssh-keygen -lf /離線媒體/kairos-pulse-release.pub     # 指紋，公告與電話核對時使用
```

- 私鑰與密碼分開保存，至少兩份離線備份；只有發佈負責人能取得。
- `allowed_signers` 透過與發佈包**不同**的管道交給客戶（合約或交付文件、客戶入口網站的 HTTPS 頁面），並公布指紋。repo 中不放正式公鑰，避免讓人以為從 repo 取得即可。
- **輪替**：產生新金鑰後，以舊金鑰簽署一份含新公鑰與生效版本的聲明，經相同的另一管道通知客戶；過渡期間 `allowed_signers` 可同時列出新舊兩行。
- **外洩**：立即停止使用，經另一管道通知客戶移除舊公鑰，以新金鑰重新簽署仍在支援期內的發佈。

## 4. 建置環境與更新方式

| 項目 | 固定方式 | 位置 |
|---|---|---|
| Rust 工具鏈與 C 編譯器 | `rust:1.99-alpine@sha256:…`（multi-arch index digest） | `packaging/build-release-common.sh` 的 `IMAGE` |
| musl-dev、gcc 等 | 隨 image digest 固定（image 內已有） | 同上 |
| GNU tar | `apk add tar=1.35-r5` | `APK_PACKAGES` |
| cargo-about、cargo-cyclonedx | `cargo install --locked` 固定版本 | `CARGO_ABOUT_VERSION`、`CARGO_CYCLONEDX_VERSION` |
| 依賴 crate | `Cargo.lock`（`--locked`；cargo 以 checksum 驗證下載內容） | repo 根目錄 |

**限制**：Alpine 套件庫只保留每個套件的最新修訂版。`tar=1.35-r5` 被新的修訂版取代後，`apk add` 會失敗（腳本會提示更新），此時需要同時更新 image digest 與套件版本：

```bash
docker pull rust:1.99-alpine
docker buildx imagetools inspect rust:1.99-alpine        # 取 Digest: 的值（index digest）
docker run --rm rust:1.99-alpine@sha256:<新 digest> sh -c \
  'cat /etc/alpine-release; apk update >/dev/null; apk policy tar'
```

更新 `IMAGE`、`APK_PACKAGES` 後執行 `sh packaging/check-reproducible.sh`，並在 CHANGELOG 記錄建置環境的變更。tar 只用來打包，版本改變不影響執行檔；image digest 改變則可能改變執行檔（gcc、musl 版本），同一版本號的重新建置不應更換 image。

更新 Rust 版本時，`rust-toolchain.toml`、`IMAGE` 與 `RUST_TOOLCHAIN` 要一起改。

## 5. 可重現性

`sh packaging/check-reproducible.sh` 以兩個全新的 docker volume 各從頭建置一次兩個產品，比對所有產物的 SHA-256；不一致時列出 tar 內不同的檔案。CI 在 push 到 `main` 與手動觸發時執行同樣的檢查（`reproducible` job）。

可重現的前提與做法：

- 來源以 `git -c core.autocrlf=false archive` 匯出（LF），與主機的行尾設定無關；容器內路徑固定（`/src`、`/build/target`、`/usr/local/cargo/registry`），執行檔中的路徑字串因此一致。
- `BUILD_COMMIT` 取自 git；tar 的檔案順序、擁有者、mtime（`SOURCE_DATE_EPOCH` = commit 時間）固定，gzip 不寫入檔名與時間（`-n`）。
- SBOM 的 timestamp 取自 `SOURCE_DATE_EPOCH`，且沒有隨機的 serialNumber。

## 6. SBOM 與第三方授權

- `THIRD-PARTY-LICENSES`：cargo-about 依 `packaging/about.toml` 與 `packaging/about.hbs` 產生，只列目標平台（x86_64-unknown-linux-musl）實際建置的依賴，保守地包含建置期依賴（例如 ring 的 `cc`），不含 dev 依賴。允許的授權與 `deny.toml` 相同；新依賴帶入其他授權時打包失敗，請經法務審查後同時更新 `deny.toml` 與 `about.toml`。
- SBOM：cargo-cyclonedx 產生 CycloneDX 1.5 JSON，不含建置期依賴。比較過 cargo-auditable（把依賴清單嵌入執行檔），兩者都以 `cargo metadata` 取得依賴，準確度相同；選擇 cargo-cyclonedx 是因為它輸出獨立的標準檔案，可直接交給客戶的掃描工具，也不需要改變執行檔的建置方式。
- **已知限制**：`cargo metadata` 會列入只由弱依賴特性（`dep?/feature`）提到、實際未啟用的可選依賴，SBOM 因此多列 `indexmap`、`hashbrown`、`equivalent`、`toml_writer`（沒有編譯進執行檔）。THIRD-PARTY-LICENSES 不受影響。
