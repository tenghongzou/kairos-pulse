# Pulse ↔ Kairos 通訊協定

協定規格只有一份，位於 monorepo 的 [`crates/kairos-proto/PROTOCOL.md`](../crates/kairos-proto/PROTOCOL.md)。Kairos 與 Pulse 共用的協定型別、序列化與欄位驗證在同一目錄的 `kairos-proto` crate，其單元測試直接從該文件解析測試向量。

發佈包（`packaging/build-release.sh` 產出的 tar.gz）內的 `PROTOCOL.md` 是規格的完整複本，不是本說明。
