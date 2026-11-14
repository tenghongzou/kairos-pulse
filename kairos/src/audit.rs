//! 稽核事件的雜湊鏈（設計包 `kairos/screens.md` S18、§24 #11；審計 F6、S7）。
//!
//! 每一筆 `audit_events` 有 `prev_hash`（前一筆的 `hash`）與 `hash`：
//!
//! ```text
//! hash = SHA-256( prev_hash ‖ "\n" ‖ JSON([id, at, actor, actor_ip, category, action, object, result, code, details]) )
//! ```
//!
//! - JSON 是 serde_json 的精簡格式（沒有空白；非 ASCII 字元原樣以 UTF-8 輸出）；`details` 是資料庫中保存的
//!   字串本身（不是解析後的物件），沒有值時為 `null`。雜湊以小寫十六進位保存。
//! - 第一筆的 `prev_hash` 是 64 個 `0`（[`GENESIS`]）。保留期限清除最舊的紀錄時，被清除的最後一筆的
//!   `hash` 記在 `meta.audit_chain_anchor`，剩下的第一筆的 `prev_hash` 必須等於它。
//! - 最新一筆的 id 與 hash 另記在 `meta.audit_chain_head`：只刪除最後幾筆的竄改也會被發現（整條鏈被重新
//!   計算時無法由資料庫本身發現，需要外送或匯出的副本比對，見 `kairos/API.md` 第 12 節）。
//! - 寫入一律經由 [`AuditLog::audit_append`]：先 INSERT（取得寫入鎖）再計算雜湊，同一個寫入交易中不會有其他行程插入，
//!   CLI 與 Kairos 同時寫入也不會讓鏈分岔。不在交易中呼叫時自己開 `BEGIN IMMEDIATE`。
//! - schema v6 之前寫入的紀錄（沒有雜湊）在升級後**第一次**寫入或驗證時依 id 順序補上，之後在
//!   `meta.audit_legacy_sealed` 記下已補上（[`AuditLog::audit_seal_legacy`]）。之後只有 [`AuditLog::audit_append`] 本身寫入的那一筆會被計算
//!   雜湊：繞過程式直接插入資料庫的紀錄（沒有雜湊）不會被「合法化」，驗證時以 `missing_hash` 指出。
//! - 任何角色都不能經由 API 或 CLI 修改或刪除紀錄（只有保留期限的清除）。
//!
//! ## HMAC（0.7〔草稿〕，schema v7 的 `hmac` 欄位）
//!
//! 雜湊鏈只靠公開的演算法：能寫入資料庫的人可以修改一筆後把之後的鏈全部重新計算。0.7 起每一筆另有
//!
//! ```text
//! hmac = HMAC-SHA256( K, "kairos-audit-record/v1\n" ‖ hash )    K = HMAC-SHA256(金鑰檔, "kairos-audit-hmac/v1")
//! ```
//!
//! 金鑰在資料目錄的 [`KEY_FILE`]（32 bytes 隨機值，0600），**與 `console.key` 分開**：外部稽核者或
//! SIEM 需要獨立驗證 HMAC 時可以只交付這把金鑰，不會同時拿到 CSRF 與 TOTP 的金鑰；輪替或遺失
//! `console.key`（所有人重設兩步驟驗證）也不會讓整條稽核鏈無法驗證。代價是多一個要備份的檔案
//! （`kairos db backup --include-keys` 一併加密備份）。
//!
//! - 金鑰檔只由 Kairos 服務在啟動時建立（[`ensure_key`]）；CLI 只讀取（以 root 執行的 CLI 不會建立
//!   一個服務帳號讀不到的金鑰檔）。寫入時依連線的資料庫路徑找到同一個資料目錄的金鑰（[`key_for_db_path`]）。
//! - **升級時的邊界**：有金鑰之後寫入的第一筆記在 `meta.audit_hmac`（`since_id`、`key_id`）。這一筆
//!   之前的紀錄（0.6 以前、或服務第一次以 0.7 啟動前由 CLI 寫入的）只有雜湊鏈，驗證時明確列出邊界；
//!   之後的每一筆都必須有正確的 HMAC（`missing_hmac`、`hmac_mismatch`），邊界之前出現 HMAC 表示邊界
//!   被移動（`hmac_boundary_mismatch`）。
//! - `meta.audit_chain_head` 另帶 `mac`：沒有金鑰無法把 head 改成較早的一筆來掩蓋刪除最後幾筆
//!   （重播先前抄下的 head 仍需靠定期外送的鏈尾雜湊發現，[`crate::audit_monitor`]）。
//! - **head 與錨點的保護**（安全稽核 E1；[`Head`]、[`check_head`]、[`ChainBefore`]）：第 1 版 head 的 `mac` 只
//!   涵蓋鏈尾，驗證也不要求 head 存在，錨點完全沒有保護——刪掉最後幾筆並刪掉 head、或刪掉最前面幾筆並
//!   改寫錨點，驗證都回報完整。第 2 版 head 的 `mac` 同時涵蓋鏈尾與錨點；HMAC 開始之後 head 必須存在；
//!   寫入端在 head 與寫入前的鏈尾、錨點不一致時不更新 head（不讓下一次寫入把痕跡「修好」）。升級前的
//!   第 1 版 head 照常接受，升級後第一次寫入時換成第 2 版。
//! - 金鑰遺失或與 `meta.audit_hmac.key_id` 不符時驗證失敗（`hmac_key_unavailable`／`hmac_key_mismatch`，
//!   `KRS-E6201`）；雜湊鏈本身仍會先完整檢查。金鑰不符時不以新的金鑰繼續簽（不「接續」鏈）。
//! - 已知限制：能同時寫入資料庫並刪除 `meta.audit_hmac` 與所有 `hmac` 的人可以把鏈「降級」成 0.7
//!   之前的樣子；由外送的鏈尾雜湊（含 HMAC 起點）在外部發現。
// 查詢結果以 tuple 回傳（沿用原本查詢的形狀），方法參數與原本的函式相同
#![allow(clippy::type_complexity, clippy::too_many_arguments)]

use ring::hmac;
use ring::rand::{SecureRandom, SystemRandom};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{Error, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use crate::store::db::StoreResult;

/// 第一筆的 `prev_hash`
pub const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";
/// `meta` 的鍵：被保留期限清除的最後一筆（`{"id","hash","purged_at","purged"}`）
pub const META_ANCHOR: &str = "audit_chain_anchor";
/// `meta` 的鍵：最新一筆（`{"id","hash"}`）
pub const META_HEAD: &str = "audit_chain_head";
/// `meta` 的鍵：升級前（沒有雜湊）的紀錄已補上雜湊（`{"through_id","sealed_at"}`）
pub const META_LEGACY_SEALED: &str = "audit_legacy_sealed";
/// `meta` 的鍵：HMAC 的起點（`{"since_id","key_id","started_at"}`）
pub const META_HMAC: &str = "audit_hmac";
/// 完整性驗證失敗的產品代碼（`copywriting.md` §3.3）
pub const CODE_CHAIN_BROKEN: &str = "KRS-E6201";
/// HMAC 的金鑰檔（資料目錄下）
pub const KEY_FILE: &str = "audit-hmac.key";

/// 稽核 HMAC 的金鑰（由 [`KEY_FILE`] 衍生；原始金鑰不留在記憶體中）
pub struct AuditKey {
    mac: hmac::Key,
    key_id: String,
}

impl std::fmt::Debug for AuditKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "AuditKey({})", self.key_id)
    }
}

pub(crate) fn ct_eq_str(a: &str, b: &str) -> bool {
    a.len() == b.len() && bool::from(subtle::ConstantTimeEq::ct_eq(a.as_bytes(), b.as_bytes()))
}

impl AuditKey {
    pub fn from_bytes(key: &[u8; 32]) -> Self {
        let base = hmac::Key::new(hmac::HMAC_SHA256, key);
        let derived = hmac::sign(&base, b"kairos-audit-hmac/v1");
        let id = hmac::sign(&base, b"kairos-audit-key-id/v1");
        Self {
            mac: hmac::Key::new(hmac::HMAC_SHA256, derived.as_ref()),
            key_id: hex(&id.as_ref()[..8]),
        }
    }

    /// 金鑰的識別（不是秘密；`meta.audit_hmac.key_id`，用來區分「金鑰不符」與「紀錄被修改」）
    pub fn key_id(&self) -> &str {
        &self.key_id
    }

    /// 一筆紀錄的 HMAC（對它的雜湊）
    pub fn record_mac(&self, hash: &str) -> String {
        let mut ctx = hmac::Context::with_key(&self.mac);
        ctx.update(b"kairos-audit-record/v1\n");
        ctx.update(hash.as_bytes());
        hex(ctx.sign().as_ref())
    }

    /// `meta.audit_chain_head` 的 HMAC
    pub fn head_mac(&self, id: i64, hash: &str) -> String {
        let mut ctx = hmac::Context::with_key(&self.mac);
        ctx.update(b"kairos-audit-head/v1\n");
        ctx.update(id.to_string().as_bytes());
        ctx.update(b"\n");
        ctx.update(hash.as_bytes());
        hex(ctx.sign().as_ref())
    }

    /// 第 2 版 `meta.audit_chain_head` 的 HMAC：除了鏈尾，也涵蓋保留期限清除的錨點（沒有清除過時為
    /// `None`）。沒有金鑰的人因此不能改寫錨點來掩蓋「刪除最前面幾筆」
    pub fn head_mac_v2(
        &self,
        tail_id: i64,
        tail_hash: &str,
        anchor: Option<(i64, &str)>,
    ) -> String {
        let mut ctx = hmac::Context::with_key(&self.mac);
        ctx.update(b"kairos-audit-head/v2\n");
        ctx.update(tail_id.to_string().as_bytes());
        ctx.update(b"\n");
        ctx.update(tail_hash.as_bytes());
        ctx.update(b"\n");
        match anchor {
            Some((id, hash)) => {
                ctx.update(id.to_string().as_bytes());
                ctx.update(b"\n");
                ctx.update(hash.as_bytes());
            }
            None => ctx.update(b"-"),
        }
        hex(ctx.sign().as_ref())
    }

    /// 讀取資料目錄的金鑰檔；不存在時為 `Ok(None)`（不建立）
    pub fn load(data_dir: &Path) -> Result<Option<Self>, Error> {
        let path = data_dir.join(KEY_FILE);
        let mut buf = zeroize::Zeroizing::new(Vec::new());
        match std::fs::File::open(&path) {
            Ok(mut f) => {
                f.read_to_end(&mut buf)?;
            }
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(Error::new(
                    e.kind(),
                    format!("無法讀取 {}: {e}", path.display()),
                ));
            }
        }
        let key: zeroize::Zeroizing<[u8; 32]> =
            zeroize::Zeroizing::new(buf.as_slice().try_into().map_err(|_| {
                Error::new(
                    ErrorKind::InvalidData,
                    format!("{} 的長度不是 32 bytes（檔案可能已損毀）", path.display()),
                )
            })?);
        Ok(Some(Self::from_bytes(&key)))
    }

    /// 讀取金鑰檔；不存在時建立（0600，只寫一次，不覆寫）。只由 Kairos 服務在啟動時呼叫
    pub fn load_or_create(data_dir: &Path) -> Result<Self, Error> {
        if let Some(k) = Self::load(data_dir)? {
            crate::users::secrets::restrict_0600(&data_dir.join(KEY_FILE));
            return Ok(k);
        }
        // 叢集模式下不自動建立（kairos/HA.md 第 11.1 節）
        crate::cluster::keys::check_autocreate(&data_dir.join(KEY_FILE))?;
        let mut key = zeroize::Zeroizing::new([0u8; 32]);
        SystemRandom::new()
            .fill(&mut key[..])
            .map_err(|_| Error::other("無法取得隨機數（作業系統的 CSPRNG）"))?;
        let path = data_dir.join(KEY_FILE);
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        match opts.open(&path) {
            Ok(mut f) => {
                f.write_all(&key[..])?;
                f.sync_all()?;
                Ok(Self::from_bytes(&key))
            }
            // 另一個行程剛建立：改讀它的
            Err(e) if e.kind() == ErrorKind::AlreadyExists => Self::load(data_dir)?
                .ok_or_else(|| Error::other(format!("{} 剛建立卻無法讀取", path.display()))),
            Err(e) => Err(Error::new(
                e.kind(),
                format!("無法建立 {}: {e}", path.display()),
            )),
        }
    }
}

/// `meta.audit_chain_head` 的內容（兩個後端共用的解析與判斷）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Head {
    /// 第 1 版（0.11.0 以前，以及沒有金鑰時的寫入）：`{"id","hash"[,"mac"]}`。`mac` 只涵蓋鏈尾
    V1 {
        id: i64,
        hash: String,
        mac: Option<String>,
    },
    /// 第 2 版：`{"v":2,"tail_id","tail_hash","anchor_id","anchor_hash","mac"}`。`mac` 涵蓋鏈尾與錨點
    /// （[`AuditKey::head_mac_v2`]）。欄位名稱與第 1 版不同：舊版的驗證讀不到 `id`／`hash`，會略過 head
    /// 的檢查而不是誤報
    V2 {
        tail_id: i64,
        tail_hash: String,
        anchor: Option<(i64, String)>,
        mac: String,
    },
}

impl Head {
    /// 解析 `meta` 中的值；兩種格式都不是時為 `None`（與沒有 head 相同）
    pub fn parse(v: &Value) -> Option<Self> {
        if v.get("v").and_then(Value::as_i64) == Some(2) {
            let anchor = match (
                v.get("anchor_id").and_then(Value::as_i64),
                v.get("anchor_hash").and_then(Value::as_str),
            ) {
                (Some(id), Some(hash)) => Some((id, hash.to_string())),
                _ => None,
            };
            return Some(Head::V2 {
                tail_id: v.get("tail_id")?.as_i64()?,
                tail_hash: v.get("tail_hash")?.as_str()?.to_string(),
                anchor,
                mac: v.get("mac")?.as_str()?.to_string(),
            });
        }
        Some(Head::V1 {
            id: v.get("id")?.as_i64()?,
            hash: v.get("hash")?.as_str()?.to_string(),
            mac: v.get("mac").and_then(Value::as_str).map(str::to_string),
        })
    }

    /// 第 2 版 head 的 JSON。`tail`：最新一筆；全部被保留期限清除時為錨點本身
    pub fn v2_json(key: &AuditKey, tail: (i64, &str), anchor: Option<&(i64, String)>) -> Value {
        let a = anchor.map(|(id, h)| (*id, h.as_str()));
        json!({
            "v": 2,
            "tail_id": tail.0,
            "tail_hash": tail.1,
            "anchor_id": a.map(|a| a.0),
            "anchor_hash": a.map(|a| a.1),
            "mac": key.head_mac_v2(tail.0, tail.1, a),
        })
    }

    /// head 記錄的鏈尾 id（斷點的位置）
    pub fn tail_id(&self) -> i64 {
        match self {
            Head::V1 { id, .. } => *id,
            Head::V2 { tail_id, .. } => *tail_id,
        }
    }
}

/// head 與資料庫的現況是否一致（寫入端：能不能把 head 往前推；驗證端：回報斷點的原因）。
///
/// - `tail`：最新一筆的 (id, hash)；沒有紀錄時為 `None`
/// - `anchor`：`meta.audit_chain_anchor`
/// - `since_id`：`meta.audit_hmac.since_id`（HMAC 的起點）
/// - `key`：**可以用來驗證**的金鑰（與起點記錄的 `key_id` 相同）；`None` 時不比對 mac
///
/// 規則：
/// - HMAC 開始之後 head 一定存在（有金鑰的每一次寫入與清除都會更新它）；不存在表示被刪除。
/// - 第 2 版：鏈尾相符（沒有紀錄時鏈尾＝錨點）、head 內的錨點與 `meta` 的錨點相同、mac 正確。
/// - 第 1 版（舊版寫入的）：鏈尾相符；鏈尾在 HMAC 起點之後時 mac 正確。錨點不在保護範圍內，
///   新版第一次寫入時換成第 2 版。
pub fn check_head(
    head: Option<&Head>,
    tail: Option<(i64, &str)>,
    anchor: Option<&(i64, String)>,
    since_id: Option<i64>,
    key: Option<&AuditKey>,
) -> Result<(), &'static str> {
    match head {
        None if since_id.is_some() => Err("head_mismatch"),
        None => Ok(()),
        Some(Head::V1 { id, hash, mac }) => {
            if tail != Some((*id, hash.as_str())) {
                return Err("head_mismatch");
            }
            if let (Some(sid), Some(k)) = (since_id, key)
                && *id >= sid
                && !mac
                    .as_deref()
                    .is_some_and(|m| ct_eq_str(m, &k.head_mac(*id, hash)))
            {
                return Err("head_mismatch");
            }
            Ok(())
        }
        Some(Head::V2 {
            tail_id,
            tail_hash,
            anchor: signed_anchor,
            mac,
        }) => {
            let signed = signed_anchor.as_ref().map(|(id, h)| (*id, h.as_str()));
            if let Some(k) = key
                && !ct_eq_str(mac, &k.head_mac_v2(*tail_id, tail_hash, signed))
            {
                return Err("head_mismatch");
            }
            if signed_anchor.as_ref() != anchor {
                return Err("anchor_mismatch");
            }
            // 沒有紀錄：全部被保留期限清除，鏈尾就是錨點
            let expected_tail = tail.or(signed);
            if expected_tail != Some((*tail_id, tail_hash.as_str())) {
                return Err("head_mismatch");
            }
            Ok(())
        }
    }
}

/// 寫入或清除之前鏈的狀態：有金鑰的寫入者據此決定能不能把 head 往前推
#[derive(Debug, Clone, Default)]
pub struct ChainBefore {
    /// `meta.audit_chain_head` 原本的值
    pub head: Option<Value>,
    /// 最新一筆的 (id, hash)
    pub tail: Option<(i64, Option<String>)>,
    pub anchor: Option<(i64, String)>,
    /// `meta.audit_hmac`：(since_id, key_id)
    pub since: Option<(i64, String)>,
}

impl ChainBefore {
    /// 這把金鑰是不是 HMAC 起點記錄的那一把（還沒開始時不是）
    pub fn signs_with(&self, key: &AuditKey) -> bool {
        self.since
            .as_ref()
            .is_some_and(|(_, kid)| kid == key.key_id())
    }

    /// 能不能把 head 更新成新的狀態。HMAC 還沒開始時一律可以（之前的 head 本來就沒有保護）；開始之後
    /// 原本的 head 必須與原本的鏈尾、錨點一致且 mac 正確。**不一致時不更新**：否則刪掉最後幾筆（或改寫
    /// 錨點）之後，只要 Kairos 再寫一筆，head 就被「修好」、痕跡消失
    pub fn can_advance(&self, key: &AuditKey) -> bool {
        let Some((sid, kid)) = &self.since else {
            return true;
        };
        if kid != key.key_id() {
            return false;
        }
        let tail = match &self.tail {
            Some((id, Some(h))) => Some((*id, h.as_str())),
            Some((_, None)) => return false,
            None => None,
        };
        let head = self.head.as_ref().and_then(Head::parse);
        check_head(
            head.as_ref(),
            tail,
            self.anchor.as_ref(),
            Some(*sid),
            Some(key),
        )
        .is_ok()
    }
}

/// 已讀取的金鑰（以資料目錄為鍵；只快取讀到的金鑰，不存在時每次重新檢查）
fn key_cache() -> &'static Mutex<HashMap<PathBuf, Arc<AuditKey>>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, Arc<AuditKey>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 資料庫檔（`db_path`，後端回報的路徑）所在資料目錄的金鑰（記憶體中的資料庫、沒有金鑰檔或無法讀取時
/// 為 `None`）
pub(crate) fn key_for_db_path(db_path: Option<&str>) -> Option<Arc<AuditKey>> {
    let path = db_path.filter(|p| !p.is_empty())?;
    let dir = Path::new(path).parent()?.to_path_buf();
    if let Some(k) = key_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&dir)
    {
        return Some(k.clone());
    }
    match AuditKey::load(&dir) {
        Ok(Some(k)) => {
            let k = Arc::new(k);
            key_cache()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(dir, k.clone());
            Some(k)
        }
        Ok(None) => None,
        Err(e) => {
            log::debug!("無法讀取稽核 HMAC 的金鑰檔：{e}");
            None
        }
    }
}

/// 服務啟動時：建立或讀取金鑰並放進快取（之後的寫入都帶 HMAC）
pub fn ensure_key(data_dir: &Path) -> Result<Arc<AuditKey>, Error> {
    let k = Arc::new(AuditKey::load_or_create(data_dir)?);
    // SQLite 回報的資料庫路徑是絕對路徑：兩種寫法都放進快取（找不到時 key_for 會重新讀檔）
    let mut cache = key_cache().lock().unwrap_or_else(|e| e.into_inner());
    if let Ok(abs) = std::fs::canonicalize(data_dir) {
        cache.insert(abs, k.clone());
    }
    cache.insert(data_dir.to_path_buf(), k.clone());
    Ok(k)
}

/// 一筆稽核事件（寫入用）
#[derive(Debug, Clone, Default)]
pub struct Audit<'a> {
    /// 執行者：`user:<帳號>`、`token:<token_id>`、作業系統帳號（CLI）、`system`、`anonymous`
    pub actor: &'a str,
    pub actor_ip: Option<&'a str>,
    /// 類別：`login`、`user`、`api_token`、`settings`、`enroll`、`pki`、`host`、`audit`、`system`、`access`、`alerts`
    pub category: &'a str,
    /// 動作（`<對象>.<動詞>`，例如 `login.success`、`user.disable`、`token.create`）
    pub action: &'a str,
    /// 對象（帳號、token_id、序號、主機、設定鍵…）
    pub object: Option<&'a str>,
    /// 結果：`ok`（成功）、`failed`（失敗）、`denied`（拒絕；權限不足、帳號鎖定）、`rejected`
    pub result: &'a str,
    /// 產品代碼（`KRS-…`）
    pub code: Option<&'a str>,
    /// 其他欄位（原因、變更前後的差異…；不含密碼與 token）
    pub details: Option<Value>,
}

/// [`Audit`] 的擁有所有權版本（跨 `await`、送進 blocking 執行緒時使用）
#[derive(Debug, Clone, Default)]
pub struct Event {
    pub actor: String,
    pub actor_ip: Option<String>,
    pub category: &'static str,
    pub action: &'static str,
    pub object: Option<String>,
    pub result: &'static str,
    pub code: Option<&'static str>,
    pub details: Option<Value>,
}

impl Event {
    pub fn append(&self, log: &(impl AuditLog + ?Sized), at_ms: i64) -> StoreResult<i64> {
        log.audit_append(
            at_ms,
            &Audit {
                actor: &self.actor,
                actor_ip: self.actor_ip.as_deref(),
                category: self.category,
                action: self.action,
                object: self.object.as_deref(),
                result: self.result,
                code: self.code,
                details: self.details.clone(),
            },
        )
    }
}

/// 資料庫中的一筆
#[derive(Debug, Clone, PartialEq)]
pub struct AuditRow {
    pub id: i64,
    pub at: i64,
    pub actor: String,
    pub actor_ip: Option<String>,
    pub category: String,
    pub action: String,
    pub object: Option<String>,
    pub result: String,
    pub code: Option<String>,
    pub details: Option<String>,
    pub prev_hash: Option<String>,
    pub hash: Option<String>,
    /// 0.7 起（schema v7）：雜湊的 HMAC（[`AuditKey::record_mac`]）；邊界之前的紀錄為 `None`
    pub hmac: Option<String>,
}

/// 一筆紀錄的雜湊（模組說明的算式）
pub fn compute_hash(prev_hash: &str, r: &AuditRow) -> String {
    let record = json!([
        r.id, r.at, r.actor, r.actor_ip, r.category, r.action, r.object, r.result, r.code,
        r.details
    ]);
    let mut input = Vec::with_capacity(256);
    input.extend_from_slice(prev_hash.as_bytes());
    input.push(b'\n');
    input.extend_from_slice(record.to_string().as_bytes());
    hex(ring::digest::digest(&ring::digest::SHA256, &input).as_ref())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// 超過上限的字串截短（以字元為單位；避免以超長的帳號名稱塞滿紀錄）
pub(crate) fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect::<String>() + "…"
    }
}

/// 驗證結果
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyReport {
    /// 檢查的筆數
    pub checked: u64,
    pub first_id: Option<i64>,
    pub last_id: Option<i64>,
    /// 最後一筆的雜湊（鏈的終點）
    pub last_hash: Option<String>,
    /// 鏈的起點（第一筆的 `prev_hash`）
    pub first_prev_hash: Option<String>,
    /// 第一個斷點
    pub broken: Option<ChainBreak>,
    /// HMAC 的邊界與結果
    pub hmac: HmacReport,
}

/// HMAC 的驗證結果
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HmacReport {
    /// 第一筆有 HMAC 的紀錄（`meta.audit_hmac.since_id`；還沒開始時為 `None`）
    pub since_id: Option<i64>,
    pub key_id: Option<String>,
    /// `not_started`（還沒有金鑰或還沒寫入）、`verified`、`key_unavailable`、`key_mismatch`、`failed`
    pub status: &'static str,
    /// 驗證過 HMAC 的筆數
    pub verified: u64,
    /// 邊界之前、只有雜湊鏈的筆數
    pub chain_only: u64,
}

/// 斷點
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainBreak {
    /// 斷在哪一筆（被刪除的最後幾筆時為 head 記錄的 id）
    pub id: i64,
    /// `hash_mismatch`（內容被修改）、`prev_mismatch`（前一筆被刪除或修改、順序被改變）、
    /// `missing_hash`（沒有雜湊，例如被人工插入）、`anchor_mismatch`（第一筆與清除時的錨點不符）、
    /// `head_mismatch`（最後幾筆被刪除、或最新一筆被修改）
    pub reason: &'static str,
    pub at: Option<i64>,
}

impl VerifyReport {
    pub fn ok(&self) -> bool {
        self.broken.is_none()
    }

    pub fn json(&self) -> Value {
        json!({
            "ok": self.ok(),
            "checked": self.checked,
            "first_id": self.first_id,
            "last_id": self.last_id,
            "first_prev_hash": self.first_prev_hash,
            "last_hash": self.last_hash,
            "broken": self.broken.as_ref().map(|b| json!({
                "id": b.id,
                "reason": b.reason,
                "at": b.at.map(crate::api::http::ts_ms),
                "product_code": CODE_CHAIN_BROKEN,
            })),
            "hmac": {
                "status": self.hmac.status,
                "since_id": self.hmac.since_id,
                "key_id": self.hmac.key_id,
                "verified": self.hmac.verified,
                "chain_only": self.hmac.chain_only,
            },
        })
    }
}

/// 查詢條件（S18 的 FilterBar）
#[derive(Debug, Clone, Default)]
pub struct AuditFilter {
    /// 執行者（完全相同）
    pub actor: Option<String>,
    pub category: Option<String>,
    /// 動作（完全相同，或以 `.` 結尾時為前綴：`user.`）
    pub action: Option<String>,
    /// 對象（片段，不分大小寫）
    pub target: Option<String>,
    pub result: Option<String>,
    pub from_ms: Option<i64>,
    pub to_ms: Option<i64>,
    /// 游標：只列出 id 小於它的（新到舊）
    pub before_id: Option<i64>,
    /// 匯出：只列出 id 大於它的（舊到新）
    pub after_id: Option<i64>,
}

impl AuditRow {
    /// API 與匯出的 JSON（`details` 解析成物件；時間為 RFC 3339 UTC）
    pub fn json(&self) -> Value {
        json!({
            "id": self.id,
            "at": crate::api::http::ts_ms(self.at),
            "actor": self.actor,
            "actor_ip": self.actor_ip,
            "category": self.category,
            "action": self.action,
            "object": self.object,
            "result": self.result,
            "code": self.code,
            "details": self.details.as_deref().and_then(|d| serde_json::from_str::<Value>(d).ok()),
            "prev_hash": self.prev_hash,
            "hash": self.hash,
            "hmac": self.hmac,
        })
    }

    /// 匯出的一行：[`AuditRow::json`] 另帶 `at_ms` 與 `details_raw`（重新計算雜湊所需的原始值）
    pub fn export_json(&self) -> Value {
        let mut v = self.json();
        v["type"] = json!("event");
        v["at_ms"] = json!(self.at);
        v["details_raw"] = json!(self.details);
        v
    }
}

/// 稽核事件的儲存（`audit_events` 與 `meta` 中的鏈頭、錨點、HMAC 起點）。SQLite 的實作在
/// `store/sqlite/audit.rs`
///
/// 寫入（`kairos/HA.md` 第 5.4 節 D1）：[`AuditLog::audit_append`] 先 INSERT 再以前一筆計算雜湊，必須與其他
/// 寫入者序列化。已在交易中呼叫時沿用該交易（呼叫端的交易必須是 [`TxMode::Immediate`]
/// (crate::store::db::TxMode::Immediate)）；不在交易中時自己開一個序列化的寫入交易
pub trait AuditLog {
    /// 寫入一筆稽核事件並接上雜湊鏈（資料目錄有金鑰檔時另加 HMAC）；回傳 id
    fn audit_append(&self, at_ms: i64, a: &Audit<'_>) -> StoreResult<i64>;

    /// [`AuditLog::audit_append`]，不需要 id
    fn audit(&self, at_ms: i64, a: &Audit<'_>) -> StoreResult<()> {
        self.audit_append(at_ms, a).map(|_| ())
    }

    /// `meta.audit_hmac`：(since_id, key_id)
    fn audit_hmac_since(&self) -> StoreResult<Option<(i64, String)>>;

    /// 鏈尾（最新一筆）：(id, hash, hmac)；沒有紀錄時為 `None`
    #[allow(clippy::type_complexity)]
    fn audit_tail(&self) -> StoreResult<Option<(i64, Option<String>, Option<String>)>>;

    /// 升級前（schema v6 之前）寫入、沒有雜湊的紀錄依 id 順序補上雜湊（只做一次；之後是 no-op）；
    /// 回傳補上的筆數
    fn audit_seal_legacy(&self) -> StoreResult<usize>;

    /// 依保留期限清除 `before_ms` 之前的紀錄，並把被清除的最後一筆記為新的錨點；回傳清除的筆數
    fn audit_purge(&self, before_ms: i64, now_ms: i64) -> StoreResult<usize>;

    /// 從頭到尾重新計算雜湊鏈，回報第一個斷點（不修改任何資料；在同一個讀取快照中進行）。HMAC 的金鑰
    /// 依資料庫所在的資料目錄取得
    fn audit_verify(&self) -> StoreResult<VerifyReport>;

    /// 新到舊的 `limit` 筆，以及符合條件的總數
    fn audit_query(&self, f: &AuditFilter, limit: usize) -> StoreResult<(Vec<AuditRow>, u64)>;

    /// 匯出：舊到新，每批最多 `limit` 筆（以 `after_id` 接續）
    fn audit_export_batch(&self, f: &AuditFilter, limit: usize) -> StoreResult<Vec<AuditRow>>;
}
