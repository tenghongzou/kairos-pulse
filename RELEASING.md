# 發佈流程

本文件給發佈負責人使用：版本規則、打 tag、建置、簽章與發佈。客戶端的驗證步驟見 `kairos/packaging/INSTALL.md`、`pulse/packaging/INSTALL.md` 第 2 節；版本相容矩陣與支援期限見 `CHANGELOG.md`。

## 1. 版本規則

- Kairos 與 Pulse 各自採用 [SemVer](https://semver.org/lang/zh-TW/) `MAJOR.MINOR.PATCH`，版本號唯一的來源是產品 `Cargo.toml` 的 `[package] version`。
  - `--version` 與啟動日誌第一行印出 `<名稱> <版本> (<commit>)`，例如 `kairos 0.8.0 (1a2b3c4)`；Pulse 在 hello 的 `agent_version` 只送版本號。
  - 發佈檔名為 `<名稱>-<版本>-<commit>-<os>-<arch>.<格式>`（例如 `kairos-0.8.0-1a2b3c4-linux-x86_64.tar.gz`、`pulse-0.8.0-1a2b3c4-windows-x86_64.msi`），打包腳本會確認 `--version` 的輸出與檔名完全一致（Linux aarch64 以 qemu-aarch64 執行；Windows 執行檔無法在容器內執行，改為確認內嵌的版本字串）。deb／rpm 的套件版本為 `<版本>-1`。
- **1.0 之前（0.x）**：破壞性變更升 MINOR（0.2.x → 0.3.0），其餘新增與修正升 PATCH。**1.0 之後**依標準 SemVer：破壞性變更升 MAJOR。
- **破壞性變更**指客戶升級時必須做事、否則會出問題的變更，例如：設定欄位的預設值改變或移除、原本接受的設定被拒絕、協定不相容、結束代碼改變、資料檔或日誌的格式與權限改變、systemd unit 需要更新、需要調整監控門檻。每一項都要寫進 `CHANGELOG.md` 的「破壞性變更與升級步驟」表。
- 兩個產品通常同時發佈相同的版本號，但可以只發佈其中一個（例如只修 Pulse 時發佈 Pulse 0.4.1，Kairos 維持 0.4.0）；相容性以 `CHANGELOG.md` 的相容矩陣為準，不以版本號是否相同判斷。
- **共用 crate**（`crates/kairos-proto`、`crates/date-guard`）是 `publish = false` 的內部 crate，不單獨發佈、不打 tag，只出現在 SBOM 中。版本依其 Rust API 的 SemVer 變動（目前為 0.1.0，第一次發佈前不再調整；之後 API 有破壞性變更時升 MINOR）。**協定版本**（hello 的 `proto` 欄位）由 `crates/kairos-proto/PROTOCOL.md` 定義，與任何 crate 的版本無關。
- `0.2.0` 之前的建置一律是 `0.1.0 (<commit>)`，只能以 commit 區分（對照表見 README「歷史來源」與各產品 README 的「升級注意事項」）。

## 2. 發佈步驟

以下以 Kairos 0.8.0、Pulse 0.8.0 為例。

### 2.1 準備發佈 commit

1. 從 `main` 開分支（衝刺的整合分支 `sprintN/integration` 須先合併回 `main`），例如 `release/0.8.0`。
2. 確認兩個產品 `Cargo.toml` 的 `version`，執行 `cargo metadata --format-version 1 >/dev/null` 更新 `Cargo.lock`；`git diff Cargo.lock` 只能有 `kairos`、`pulse` 等 workspace 成員的版本變動。
3. 更新 `CHANGELOG.md`：
   - 草稿段落 `## [0.8.0] - 〔草稿：發佈日期待定〕` 改為 `## [0.8.0] - YYYY-MM-DD`，移除段落中的〔草稿〕標記；`[Unreleased]` 中若有新的項目先移入。之後的版本同樣把 `[Unreleased]` 的內容移到新的 `## [<版本>] - YYYY-MM-DD`，上方保留空的 `## [Unreleased]`。兩個產品版本不同時（例如只發佈 Pulse 0.4.1），標題寫成 `## [Pulse 0.4.1] - YYYY-MM-DD`。
   - 更新「版本相容矩陣」，移除已確定項目的〔草稿〕標記。
4. 提交、開 PR，等 CI 全綠（`check`、`deny`、`package`）。`package` job 的 artifact（`release-<commit>`）就是這個 commit 的未簽章發佈包。
5. 合併到整合分支後，記下合併後的 commit（以下稱 `<release-commit>`）；之後的步驟都在這個 commit 上進行。

### 2.2 打 tag

每個產品一個 annotated tag，名稱 `<名稱>-v<版本>`；打包腳本會檢查 HEAD 上的 `<名稱>-v*` tag 與 `Cargo.toml` 的版本一致。

```bash
git switch --detach <release-commit>
git tag -a kairos-v0.8.0 -m "Kairos 0.8.0"
git tag -a pulse-v0.8.0  -m "Pulse 0.8.0"
git push origin kairos-v0.8.0 pulse-v0.8.0
```

建議以個人的 SSH 金鑰簽署 tag（`git tag -s`，事先 `git config gpg.format ssh`、`git config user.signingkey ~/.ssh/id_ed25519.pub`），**不要**使用下一節的發佈金鑰：發佈金鑰只用來簽 `SHA256SUMS`，保存在離線的位置。tag 一旦推送就不再移動；發現問題時發佈新的 PATCH 版本。

### 2.3 建置與簽章

在乾淨的 checkout 上執行（Windows 請用 Git Bash；需要 Docker、cargo、cargo-deny）：

```bash
git switch --detach kairos-v0.8.0          # 與 pulse-v0.8.0 是同一個 commit
SIGNING_KEY=/離線媒體/kairos-pulse-release sh packaging/release.sh
```

`packaging/release.sh` 依序：

1. 確認工作目錄乾淨（有未提交的變更時拒絕；發佈包帶 `-dirty` 時也拒絕簽章）。
2. 執行 `cargo fmt --check`、`cargo clippy -D warnings`、`cargo test`、`cargo deny check`，任一失敗即停止。`cargo test` 的步驟與 CI 相同：先 `cargo test --workspace --exclude kp-e2e`，再 `cargo build -p kairos -p pulse --bins` 建置執行檔，最後單獨執行端對端測試 `cargo test -p kp-e2e`（失敗時可從輸出分辨是單元／整合測試還是端對端測試）。
3. 刪除 `dist/` 中舊的 `kairos-*`、`pulse-*`、`SHA256SUMS*`，打包兩個產品（`kairos/packaging/build-release.sh`、`pulse/packaging/build-release.sh`，共用實作 `packaging/build-release-common.sh`）。
4. 以 `packaging/sign-release.sh` 簽署合併版 `SHA256SUMS`，並以 `<私鑰>.pub` 驗證剛產生的簽章。

產出（`dist/`；0.8.0 起）：

| 檔案 | 說明 |
|---|---|
| `kairos-<版本>-<commit>-linux-{x86_64,aarch64}.tar.gz`、`pulse-…-linux-{x86_64,aarch64}.tar.gz` | 發佈包（含 `THIRD-PARTY-LICENSES` 與 `<名稱>.cdx.json`） |
| `kairos-…-linux-{x86_64,aarch64}.{deb,rpm}`、`pulse-…-linux-{x86_64,aarch64}.{deb,rpm}` | 套件（nfpm；`<產品>/packaging/pkg/`） |
| `pulse-<版本>-<commit>-windows-x86_64.{zip,msi}` | Pulse 的 Windows 發佈檔（MSI：`pulse/packaging/windows/pulse.wxs`，wixl） |
| `<名稱>-<版本>-<commit>-<os>-<arch>.cdx.json` | 每個平台的 SBOM（與發佈包內相同），供客戶匯入掃描工具 |
| `pulse-install.sh` | Pulse 的一行安裝腳本（`pulse/packaging/install.sh`） |
| `SHA256SUMS` | 以上所有檔案的 SHA-256（共 20 個：Kairos 8、Pulse 12） |
| `SHA256SUMS.sig` | `SHA256SUMS` 的簽章 |

只建置部分目標（例如開發時快速確認）：`TARGETS=x86_64-unknown-linux-musl PACKAGES= sh kairos/packaging/build-release.sh`；正式發佈一律用預設值。容器內 cargo 的平行數預設 2（`BUILD_JOBS`，只影響時間，不影響產物）；完整建置兩個產品、五個目標在 4 vCPU 的建置機上約需〔待量測〕。

### 2.4 與 CI 的建置比對

建置在 digest 固定的容器中進行、來源取自 `git archive`，同一個 commit 在本機與 CI 的產物應完全相同。發佈前下載 `package` job 的 artifact，比對兩份 `SHA256SUMS`：

```bash
diff dist/SHA256SUMS <下載的 artifact>/SHA256SUMS && echo 與 CI 一致
```

不一致時不要發佈，先以 `sh packaging/check-reproducible.sh` 找出差異（見第 5 節）。

### 2.5 發佈

把 `dist/` 的所有檔案上傳到發佈處，公告中附上版本、`CHANGELOG.md` 對應的段落與發佈公鑰的指紋。`allowed_signers` 不要和發佈包放在同一個地方（見第 3 節）。

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
| GNU tar | `apk add tar=1.35-r5` | `APK_BASE` |
| aarch64 交叉編譯：zig、qemu-aarch64 | `apk add zig=0.16.0-r1 qemu-aarch64=11.0.3-r0` | `APK_AARCH64` |
| Windows：mingw-w64-gcc、msitools（wixl、msibuild）、libfaketime、zip | `apk add mingw-w64-gcc=15.2.0-r0 msitools=0.106-r0 libfaketime=0.9.13-r0 zip=3.0-r13` | `APK_WINDOWS` |
| cargo-about、cargo-cyclonedx、cargo-zigbuild | `cargo install --locked` 固定版本 | `CARGO_ABOUT_VERSION`、`CARGO_CYCLONEDX_VERSION`、`CARGO_ZIGBUILD_VERSION` |
| nfpm（deb／rpm） | GitHub 發佈檔，固定版本並核對 SHA-256 | `NFPM_VERSION`、`NFPM_SHA256` |
| Rust 的交叉編譯目標（`aarch64-unknown-linux-musl`、`x86_64-pc-windows-gnu`） | `rustup target add`（隨 `RUST_TOOLCHAIN` 固定；rustup 以 SHA-256 驗證下載內容） | 容器內 |
| 依賴 crate | `Cargo.lock`（`--locked`；cargo 以 checksum 驗證下載內容） | repo 根目錄 |

**限制**：Alpine 套件庫只保留每個套件的最新修訂版。任一固定的套件被新的修訂版取代後，`apk add` 會失敗（腳本會提示更新），此時需要同時更新 image digest 與套件版本：

```bash
docker pull rust:1.99-alpine
docker buildx imagetools inspect rust:1.99-alpine        # 取 Digest: 的值（index digest）
docker run --rm rust:1.99-alpine@sha256:<新 digest> sh -c \
  'cat /etc/alpine-release; apk update >/dev/null; apk policy tar zig qemu-aarch64 mingw-w64-gcc msitools libfaketime zip'
```

更新 `IMAGE`、`APK_BASE`／`APK_AARCH64`／`APK_WINDOWS` 後執行 `sh packaging/check-reproducible.sh`，並在 CHANGELOG 記錄建置環境的變更。tar、zip、msitools、libfaketime、nfpm 只用來打包，版本改變不影響執行檔（但可能改變套件的位元組，需重新確認可重現）；zig 與 mingw-w64-gcc 是 aarch64、Windows 的 C 編譯器與連結器，版本改變會改變執行檔；image digest 改變則可能改變 x86_64 的執行檔（gcc、musl 版本）。同一版本號的重新建置不應更換這些版本。

更新 nfpm：從 `https://github.com/goreleaser/nfpm/releases` 取新版的 `nfpm_<版本>_Linux_x86_64.tar.gz` 與 `checksums.txt`，核對後更新 `NFPM_VERSION`、`NFPM_SHA256`（版本至少發佈兩週以上）。

更新 Rust 版本時，`rust-toolchain.toml`、`IMAGE` 與 `RUST_TOOLCHAIN` 要一起改。

## 5. 可重現性

`sh packaging/check-reproducible.sh` 以兩個全新的 docker volume 各從頭建置一次兩個產品，比對所有產物的 SHA-256；不一致時列出 tar 內不同的檔案。CI 在 push 到 `main` 與手動觸發時執行同樣的檢查（`reproducible` job）。

可重現的前提與做法：

- 來源以 `git -c core.autocrlf=false archive` 匯出（LF），與主機的行尾設定無關；容器內路徑固定（`/src`、`/build/target`、`/usr/local/cargo/registry`），執行檔中的路徑字串因此一致。
- `BUILD_COMMIT` 取自 git；tar 的檔案順序、擁有者、mtime（`SOURCE_DATE_EPOCH` = commit 時間）固定，gzip 不寫入檔名與時間（`-n`）。
- SBOM 的 timestamp 取自 `SOURCE_DATE_EPOCH`，且沒有隨機的 serialNumber。
- deb／rpm：nfpm 的 `mtime` 設為 commit 時間（套件內檔案與控制資料的時間）。
- zip：檔案依名稱排序、mtime 為 commit 時間、`TZ=UTC`（DOS 時間是本地時間）、`-X` 不寫額外的時間欄位。
- MSI：wixl 與 msibuild 在 libfaketime 下以 commit 時間執行（摘要資訊的建立／儲存時間、OLE 目錄的時間），`NO_FAKE_STAT=1` 讓 CAB 內的檔案時間取自（已固定的）mtime；PackageCode 由「版本＋commit」、ProductCode 由版本推導（不使用 wixl 產生的隨機值）。
- Windows 執行檔以 `-Wl,--no-insert-timestamp` 連結（PE 標頭的時間戳記為 0），並以 `-C strip=symbols` 去掉符號表（rustc 為 raw-dylib 產生的匯入程式庫在隨機命名的暫存目錄中，目錄名稱會留在 COFF 符號表）。

## 6. SBOM 與第三方授權

- `THIRD-PARTY-LICENSES`：cargo-about 依 `packaging/about.toml` 與 `packaging/about.hbs` 產生，**每個目標各自產生**（建置時把 `targets` 換成該目標；Windows 版另含 `windows-service`、`windows-sys` 等），只列該平台實際建置的依賴，保守地包含建置期依賴（例如 ring 的 `cc`），不含 dev 依賴。允許的授權與 `deny.toml` 相同；新依賴帶入其他授權時打包失敗，請經法務審查後同時更新 `deny.toml` 與 `about.toml`。
- SBOM：cargo-cyclonedx 產生 CycloneDX 1.5 JSON，不含建置期依賴。比較過 cargo-auditable（把依賴清單嵌入執行檔），兩者都以 `cargo metadata` 取得依賴，準確度相同；選擇 cargo-cyclonedx 是因為它輸出獨立的標準檔案，可直接交給客戶的掃描工具，也不需要改變執行檔的建置方式。
- **已知限制**：`cargo metadata` 會列入只由弱依賴特性（`dep?/feature`）提到、實際未啟用的可選依賴，SBOM 因此多列 `indexmap`、`hashbrown`、`equivalent`、`toml_writer`（沒有編譯進執行檔）。THIRD-PARTY-LICENSES 不受影響。
