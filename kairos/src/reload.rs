//! TLS 熱重載：定期檢查伺服器憑證、私鑰、客戶端 CA 與 CRL 檔的內容，有變化就重建設定並
//! 原子替換；重建失敗時保留舊設定。每個週期也以目前的驗證器重新驗證所有現役連線的
//! 客戶端憑證（撤銷、過期、不再受信任），不通過的連線收到 `certificate_rejected` 並被關閉。
//!
//! 檢查檔案與重新驗證都是同步、可能耗時的工作，一律在 blocking 執行緒執行，不佔用 async worker。
//! 檢查檔案的工作異常結束（panic）時重建 [`Reloader`]，下個週期強制重建設定；
//! 重新驗證的工作異常結束時無法確保已撤銷的憑證會被斷線（fail-open），因此觸發關機。
//!
//! 檢查檔案可能卡住（網路磁碟、FIFO 等；R2-K10）：每次最多等 [`CHECK_WAIT`]，逾時就記 ERROR、
//! 先以目前的設定重新驗證現役連線；卡住的檢查完成前不會再啟動新的檢查（避免佔滿 blocking 執行緒），
//! 但每個週期的重新驗證照常進行，不依賴檔案檢查完成。

use log::{debug, error, info, warn};
use std::io::Error;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::{MissedTickBehavior, interval};
use tokio_rustls::rustls::pki_types::UnixTime;

use crate::registry::{KickReason, Registry};
use crate::shutdown::{self, Shutdown};
use crate::tls::{self, ChainRejection, Fingerprint, TlsFileKind, TlsMaterial, TlsPaths, TlsState};

/// 每個週期等待檔案檢查完成的上限（不超過檢查間隔）
pub const CHECK_WAIT: Duration = Duration::from_secs(10);

/// 一次檢查的結果
#[derive(Debug)]
pub enum ReloadOutcome {
    /// 檔案內容沒有變化
    Unchanged,
    /// 已重建並替換設定；內含內容有變化的檔案種類（依 cert、key、client_ca、crl 排序、不重複）
    Reloaded(Vec<TlsFileKind>),
    /// 檔案有變化但重建失敗，仍使用舊設定
    Failed(Error),
}

pub struct Reloader {
    paths: TlsPaths,
    /// 最近一次「嘗試」重建時的指紋；重建失敗後，檔案沒有再變化就不重試（避免每個週期重複記錄同一個錯誤）
    last: Fingerprint,
    /// 目前生效的設定建立時的指紋（判斷這次重新載入變更了哪些檔案；重建失敗的嘗試不算）
    loaded: Fingerprint,
    state: Arc<TlsState>,
    /// 測試用：檢查檔案前等待（模擬卡住的檔案系統）
    #[cfg(test)]
    stall: Option<Arc<Stall>>,
}

/// 測試用的閘門：關閉時 `wait` 會卡住，開啟後放行
#[cfg(test)]
#[derive(Debug, Default)]
pub struct Stall {
    open: std::sync::Mutex<bool>,
    cv: std::sync::Condvar,
}

#[cfg(test)]
impl Stall {
    fn wait(&self) {
        let mut open = self.open.lock().unwrap();
        while !*open {
            open = self.cv.wait(open).unwrap();
        }
    }

    pub fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.cv.notify_all();
    }
}

impl Reloader {
    /// `initial` 應在建立目前設定**之前**取得，確保之間的變更不會被漏掉
    pub fn new(paths: TlsPaths, initial: Fingerprint, state: Arc<TlsState>) -> Self {
        Self {
            paths,
            loaded: initial.clone(),
            last: initial,
            state,
            #[cfg(test)]
            stall: None,
        }
    }

    /// 重建一個「下次檢查一定重建設定」的 Reloader（用於檢查工作異常結束後）
    fn recreate(paths: TlsPaths, state: Arc<TlsState>) -> Self {
        Self::new(paths, Fingerprint::new(), state)
    }

    /// 檢查檔案是否變化，有變化就重建（同步讀檔，請在 blocking 執行緒呼叫）
    pub fn check(&mut self) -> ReloadOutcome {
        #[cfg(test)]
        if let Some(stall) = &self.stall {
            stall.wait();
        }
        let current = tls::fingerprint(&self.paths);
        if current == self.last {
            return ReloadOutcome::Unchanged;
        }
        self.last = current.clone();
        match tls::build(&self.paths) {
            Ok(material) => {
                self.state.replace(material);
                let changed = changed_kinds(&self.paths, &self.loaded, &current);
                self.loaded = current;
                ReloadOutcome::Reloaded(changed)
            }
            Err(e) => ReloadOutcome::Failed(e),
        }
    }
}

/// 比較前後兩次的指紋，回傳內容有變化（含新出現、消失、無法讀取）的檔案種類（F8）。
/// 指紋的順序與 [`TlsPaths::watched_files_with_kind`] 相同；`old` 為空（強制重建）時所有種類都算變更。
pub fn changed_kinds(paths: &TlsPaths, old: &Fingerprint, new: &Fingerprint) -> Vec<TlsFileKind> {
    let mut kinds: Vec<TlsFileKind> = paths
        .watched_files_with_kind()
        .into_iter()
        .enumerate()
        .filter(|(i, _)| old.get(*i) != new.get(*i))
        .map(|(_, (kind, _))| kind)
        .collect();
    kinds.sort();
    kinds.dedup();
    kinds
}

/// 例如 `cert、key`；沒有可辨識的變更時為 `未知`
fn describe_kinds(kinds: &[TlsFileKind]) -> String {
    if kinds.is_empty() {
        return "未知".to_string();
    }
    kinds
        .iter()
        .map(|k| k.as_str())
        .collect::<Vec<_>>()
        .join("、")
}

/// 以目前設定的驗證器重新驗證所有現役連線；不通過者送出 kick。回傳被踢的 (agent_id, 原因)。
pub fn revalidate(
    registry: &Registry,
    material: &TlsMaterial,
    now: UnixTime,
) -> Vec<(String, ChainRejection)> {
    let Some(verifier) = &material.verifier else {
        return Vec::new();
    };
    let mut kicked = Vec::new();
    // 先取出清單再驗證，驗證期間不持有 registry 的鎖
    for conn in registry.live_connections() {
        if let Err(reason) = tls::verify_chain(verifier.as_ref(), conn.client_chain.as_deref(), now)
            && registry.kick(
                &conn.agent_id,
                conn.conn_id,
                KickReason::CertificateRejected(reason.clone()),
            )
        {
            kicked.push((conn.agent_id, reason));
        }
    }
    kicked
}

/// 重載工作：每 `period` 檢查一次，直到關機
pub async fn run(
    reloader: Reloader,
    registry: Arc<Registry>,
    state: Arc<TlsState>,
    period: Duration,
    shutdown: Arc<Shutdown>,
) {
    run_with_wait(
        reloader,
        registry,
        state,
        period,
        shutdown,
        CHECK_WAIT.min(period),
    )
    .await
}

async fn run_with_wait(
    reloader: Reloader,
    registry: Arc<Registry>,
    state: Arc<TlsState>,
    period: Duration,
    shutdown: Arc<Shutdown>,
    check_wait: Duration,
) {
    let mut ticker = interval(period);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // 第一次 tick 立即完成；啟動時設定剛載入，跳過
    ticker.tick().await;
    let (paths, reloader_state) = (reloader.paths.clone(), reloader.state.clone());
    let mut idle = Some(reloader);
    // 尚未完成的檔案檢查（卡住時跨週期保留）
    let mut pending: Option<tokio::task::JoinHandle<(Reloader, ReloadOutcome)>> = None;
    let mut stuck_logged = false;

    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            _ = ticker.tick() => {}
        }

        // 沒有進行中的檢查時才啟動新的檢查
        if pending.is_none()
            && let Some(mut r) = idle.take()
        {
            pending = Some(tokio::task::spawn_blocking(move || {
                let outcome = r.check();
                (r, outcome)
            }));
        }
        let outcome = match pending.as_mut() {
            None => ReloadOutcome::Unchanged,
            Some(handle) => match tokio::time::timeout(check_wait, handle).await {
                Err(_) => {
                    if !stuck_logged {
                        error!(
                            "檢查 TLS 設定檔超過 {} 秒仍未完成（檔案系統可能卡住），先以目前的設定重新驗證現役連線",
                            check_wait.as_secs_f64()
                        );
                        stuck_logged = true;
                    }
                    ReloadOutcome::Unchanged
                }
                Ok(joined) => {
                    pending = None;
                    if stuck_logged {
                        info!("檢查 TLS 設定檔已恢復");
                        stuck_logged = false;
                    }
                    match joined {
                        Ok((r, outcome)) => {
                            idle = Some(r);
                            outcome
                        }
                        Err(e) => {
                            // 不停止整個工作：重新驗證仍要繼續；下個週期強制重建設定
                            error!(
                                "檢查 TLS 設定檔的工作異常結束，下個週期重新載入，期間沿用目前的設定: {}",
                                e
                            );
                            idle = Some(Reloader::recreate(paths.clone(), reloader_state.clone()));
                            ReloadOutcome::Unchanged
                        }
                    }
                }
            },
        };

        let material = state.current();
        match outcome {
            ReloadOutcome::Unchanged => {}
            ReloadOutcome::Reloaded(changed) => info!(
                "已重新載入 TLS 設定（變更：{}；mTLS：{}，CRL {} 份）",
                describe_kinds(&changed),
                if material.auth_enabled() {
                    "啟用"
                } else {
                    "未啟用"
                },
                material.crl_count
            ),
            ReloadOutcome::Failed(e) => {
                error!("重新載入 TLS 設定失敗，繼續使用目前的設定: {}", e)
            }
        }

        let registry_for_check = registry.clone();
        let joined = tokio::task::spawn_blocking(move || {
            revalidate(&registry_for_check, &material, UnixTime::now())
        })
        .await;
        let kicked = match joined {
            Ok(kicked) => kicked,
            Err(e) => {
                shutdown::fatal(
                    &shutdown,
                    format!(
                        "重新驗證客戶端憑證的工作異常結束（{}），無法確保已撤銷或過期的憑證會被斷線",
                        e
                    ),
                );
                break;
            }
        };
        for (agent_id, reason) in &kicked {
            warn!(
                "agent {} 的客戶端憑證已不再有效（{}），關閉連線",
                agent_id, reason
            );
        }
        if !kicked.is_empty() {
            debug!("本次檢查關閉 {} 條連線", kicked.len());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{ClientChain, HelloInfo};
    use chrono::Utc;
    use std::path::PathBuf;
    use std::time::Instant;
    use tokio::sync::oneshot::error::TryRecvError;
    use tokio_rustls::rustls::pki_types::CertificateDer;
    use tokio_rustls::rustls::pki_types::pem::PemObject;

    const PKI: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/pki/");
    const A: &str = "6f1c0e4a-1b2c-4d3e-9f00-112233445566";
    const B: &str = "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee";

    fn pki(name: &str) -> PathBuf {
        PathBuf::from(format!("{PKI}{name}"))
    }

    /// 在暫存目錄放一份可被改寫的設定檔組
    struct Fixture {
        dir: PathBuf,
        paths: TlsPaths,
    }

    impl Fixture {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "kairos-reload-{}-{}-{}",
                tag,
                std::process::id(),
                crate::registry::next_conn_id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            for f in ["server.crt", "server.key", "test-ca.crt"] {
                std::fs::copy(pki(f), dir.join(f)).unwrap();
            }
            std::fs::copy(pki("crl-empty.crl"), dir.join("crl.crl")).unwrap();
            let s = |n: &str| dir.join(n).to_string_lossy().to_string();
            let paths = TlsPaths {
                cert: s("server.crt"),
                key: s("server.key"),
                client_ca: Some(s("test-ca.crt")),
                crls: vec![s("crl.crl")],
            };
            Self { dir, paths }
        }

        fn reloader(&self) -> (Reloader, Arc<TlsState>) {
            let fp = tls::fingerprint(&self.paths);
            let state = Arc::new(TlsState::new(tls::build(&self.paths).unwrap()));
            (Reloader::new(self.paths.clone(), fp, state.clone()), state)
        }

        fn write(&self, name: &str, from: &str) {
            std::fs::copy(pki(from), self.dir.join(name)).unwrap();
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn chain(name: &str) -> ClientChain {
        Arc::from(vec![CertificateDer::from_pem_file(pki(name)).unwrap()])
    }

    fn hello(agent_id: &str, chain: Option<ClientChain>) -> HelloInfo {
        HelloInfo {
            agent_id: agent_id.into(),
            hostname: "h".into(),
            agent_version: "1".into(),
            peer: "p".into(),
            offline_after: Duration::from_secs(5),
            client_chain: chain,
        }
    }

    #[test]
    fn unchanged_files_do_not_reload() {
        let fx = Fixture::new("same");
        let (mut r, state) = fx.reloader();
        let before = Arc::as_ptr(&state.current());
        assert!(matches!(r.check(), ReloadOutcome::Unchanged));
        assert_eq!(Arc::as_ptr(&state.current()), before);
    }

    #[test]
    fn changed_crl_triggers_reload() {
        let fx = Fixture::new("crl");
        let (mut r, state) = fx.reloader();
        let before = Arc::as_ptr(&state.current());
        fx.write("crl.crl", "crl-revoke-a.crl");
        assert!(matches!(r.check(), ReloadOutcome::Reloaded(k) if k == [TlsFileKind::Crl]));
        assert_ne!(Arc::as_ptr(&state.current()), before);
        // 再檢查一次：沒有新變化
        assert!(matches!(r.check(), ReloadOutcome::Unchanged));
    }

    #[test]
    fn reload_reports_which_kinds_of_files_changed() {
        // F8：日誌附上變更的檔案種類
        let fx = Fixture::new("kinds");
        let (mut r, _state) = fx.reloader();
        // 伺服器憑證與私鑰一起更新（只要內容不同即可：寫入原內容再加一個換行）
        let cert = std::fs::read(pki("server.crt")).unwrap();
        let key = std::fs::read(pki("server.key")).unwrap();
        std::fs::write(fx.dir.join("server.crt"), [cert.as_slice(), b"\n"].concat()).unwrap();
        std::fs::write(fx.dir.join("server.key"), [key.as_slice(), b"\n"].concat()).unwrap();
        match r.check() {
            ReloadOutcome::Reloaded(k) => {
                assert_eq!(k, [TlsFileKind::Cert, TlsFileKind::Key]);
                assert_eq!(describe_kinds(&k), "cert、key");
            }
            other => panic!("應重新載入: {other:?}"),
        }
        // 只換客戶端 CA（內容不同但仍是同一張 CA）
        let ca = std::fs::read(pki("test-ca.crt")).unwrap();
        std::fs::write(fx.dir.join("test-ca.crt"), [ca.as_slice(), b"\n"].concat()).unwrap();
        assert!(matches!(r.check(), ReloadOutcome::Reloaded(k) if k == [TlsFileKind::ClientCa]));
    }

    #[test]
    fn changed_kinds_compares_fingerprints_by_position() {
        let mut paths = TlsPaths {
            cert: "c".into(),
            key: "k".into(),
            client_ca: Some("ca".into()),
            crls: vec!["crl1".into(), "crl2".into()],
        };
        let fp = |paths: &TlsPaths, hashes: &[Option<u64>]| -> Fingerprint {
            paths
                .watched_files()
                .into_iter()
                .zip(hashes)
                .map(|(p, h)| (p.to_string(), *h))
                .collect()
        };
        let old = fp(&paths, &[Some(1), Some(2), Some(3), Some(4), Some(5)]);
        assert!(changed_kinds(&paths, &old, &old).is_empty());
        // 兩份 CRL 都變更：只列一次 crl
        let new = fp(&paths, &[Some(1), Some(2), Some(3), Some(40), None]);
        assert_eq!(changed_kinds(&paths, &old, &new), [TlsFileKind::Crl]);
        let new = fp(&paths, &[Some(10), Some(2), None, Some(4), Some(5)]);
        assert_eq!(
            changed_kinds(&paths, &old, &new),
            [TlsFileKind::Cert, TlsFileKind::ClientCa]
        );
        // 強制重建（沒有上次的指紋）：所有種類都算變更
        assert_eq!(
            changed_kinds(&paths, &Fingerprint::new(), &old),
            [
                TlsFileKind::Cert,
                TlsFileKind::Key,
                TlsFileKind::ClientCa,
                TlsFileKind::Crl
            ]
        );
        assert_eq!(describe_kinds(&[]), "未知");
        // 未啟用 mTLS：只監看 cert 與 key
        paths.client_ca = None;
        let old = fp(&paths, &[Some(1), Some(2)]);
        let new = fp(&paths, &[Some(1), Some(3)]);
        assert_eq!(changed_kinds(&paths, &old, &new), [TlsFileKind::Key]);
    }

    #[test]
    fn failed_reload_keeps_old_material_and_retries_after_fix() {
        let fx = Fixture::new("fail");
        let (mut r, state) = fx.reloader();
        let before = state.current();
        std::fs::write(fx.dir.join("test-ca.crt"), "broken").unwrap();

        match r.check() {
            ReloadOutcome::Failed(e) => assert!(e.to_string().contains("test-ca.crt"), "{e}"),
            other => panic!("應失敗: {other:?}"),
        }
        // 舊設定仍在使用
        assert!(Arc::ptr_eq(&state.current(), &before));
        assert!(state.current().auth_enabled());
        // 檔案沒有再變化：不重試、不重複報錯
        assert!(matches!(r.check(), ReloadOutcome::Unchanged));
        // 修好後重建成功；變更的種類以目前生效的設定為準（CA 內容與原本相同，但 CRL 有變）
        fx.write("crl.crl", "crl-revoke-a.crl");
        fx.write("test-ca.crt", "test-ca.crt");
        assert!(matches!(r.check(), ReloadOutcome::Reloaded(k) if k == [TlsFileKind::Crl]));
        assert!(!Arc::ptr_eq(&state.current(), &before));
    }

    #[test]
    fn recreated_reloader_rebuilds_on_next_check() {
        // K-S11：檢查工作異常結束後重建的 Reloader 下次一定重建設定
        let fx = Fixture::new("recreate");
        let (_r, state) = fx.reloader();
        let before = state.current();
        let mut r = Reloader::recreate(fx.paths.clone(), state.clone());
        assert!(matches!(r.check(), ReloadOutcome::Reloaded(k) if k.len() == 4));
        assert!(!Arc::ptr_eq(&state.current(), &before));
        assert!(matches!(r.check(), ReloadOutcome::Unchanged));
    }

    #[tokio::test]
    async fn run_revalidates_on_blocking_thread_and_stops_on_shutdown() {
        // K-S12：重新驗證在 blocking 執行緒執行；撤銷的連線在一個週期內被踢
        let fx = Fixture::new("run");
        let (r, state) = fx.reloader();
        let registry = Arc::new(Registry::new());
        let mut a = registry
            .register(
                1,
                hello(A, Some(chain("client-a.crt"))),
                Instant::now(),
                Utc::now(),
            )
            .unwrap();
        let stop = Arc::new(Shutdown::new());
        let task = tokio::spawn(run(
            r,
            registry.clone(),
            state.clone(),
            Duration::from_millis(30),
            stop.clone(),
        ));
        fx.write("crl.crl", "crl-revoke-a.crl");
        let kicked = tokio::time::timeout(Duration::from_secs(5), &mut a.kick)
            .await
            .expect("應在數個週期內被踢")
            .unwrap();
        assert!(matches!(kicked, KickReason::CertificateRejected(_)));
        stop.trigger();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stop.fatal_reason(), None);
    }

    #[tokio::test]
    async fn stuck_file_check_does_not_stop_revalidation() {
        // R2-K10：檔案檢查卡住時，重新驗證照常進行（不 fail-open）
        let fx = Fixture::new("stuck");
        let (mut r, state) = fx.reloader();
        let stall = Arc::new(Stall::default());
        r.stall = Some(stall.clone());
        let registry = Arc::new(Registry::new());
        let mut a = registry
            .register(
                1,
                hello(A, Some(chain("client-a.crt"))),
                Instant::now(),
                Utc::now(),
            )
            .unwrap();
        let stop = Arc::new(Shutdown::new());
        let task = tokio::spawn(run_with_wait(
            r,
            registry.clone(),
            state.clone(),
            Duration::from_millis(30),
            stop.clone(),
            Duration::from_millis(20),
        ));
        // 檔案檢查卡住期間，CA/CRL 以其他方式更新（此處直接替換設定）
        let mut revoked = fx.paths.clone();
        revoked.crls = vec![pki("crl-revoke-a.crl").to_string_lossy().to_string()];
        state.replace(tls::build(&revoked).unwrap());
        let kicked = tokio::time::timeout(Duration::from_secs(5), &mut a.kick)
            .await
            .expect("檢查卡住時仍應重新驗證並踢掉已撤銷的連線")
            .unwrap();
        assert!(matches!(kicked, KickReason::CertificateRejected(_)));
        stall.release();
        stop.trigger();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
    }

    #[test]
    fn missing_file_is_a_failed_reload() {
        let fx = Fixture::new("missing");
        let (mut r, state) = fx.reloader();
        let before = state.current();
        std::fs::remove_file(fx.dir.join("server.key")).unwrap();
        assert!(matches!(r.check(), ReloadOutcome::Failed(_)));
        assert!(Arc::ptr_eq(&state.current(), &before));
    }

    #[test]
    fn revalidate_kicks_revoked_and_untrusted_connections() {
        let fx = Fixture::new("revalidate");
        let (mut r, state) = fx.reloader();
        let registry = Registry::new();
        let t0 = Instant::now();
        let mut a = registry
            .register(1, hello(A, Some(chain("client-a.crt"))), t0, Utc::now())
            .unwrap();
        let mut b = registry
            .register(2, hello(B, Some(chain("client-b.crt"))), t0, Utc::now())
            .unwrap();
        let mut no_cert = registry
            .register(
                3,
                hello("cccccccc-0000-4000-8000-000000000000", None),
                t0,
                Utc::now(),
            )
            .unwrap();

        // 空 CRL：只有沒有憑證的連線不通過
        let kicked = revalidate(&registry, &state.current(), UnixTime::now());
        assert_eq!(kicked.len(), 1);
        assert!(matches!(
            no_cert.kick.try_recv(),
            Ok(KickReason::CertificateRejected(_))
        ));

        // 換成撤銷 A 的 CRL → A 被踢，B 不受影響
        fx.write("crl.crl", "crl-revoke-a.der");
        assert!(matches!(r.check(), ReloadOutcome::Reloaded(_)));
        let kicked = revalidate(&registry, &state.current(), UnixTime::now());
        assert_eq!(kicked.iter().map(|k| k.0.as_str()).collect::<Vec<_>>(), [A]);
        match a.kick.try_recv() {
            Ok(KickReason::CertificateRejected(reason)) => {
                assert!(reason.reason.to_lowercase().contains("revoked"), "{reason}")
            }
            other => panic!("A 應被踢: {other:?}"),
        }
        assert_eq!(b.kick.try_recv(), Err(TryRecvError::Empty));

        // 已踢過的不會重複處理
        assert!(revalidate(&registry, &state.current(), UnixTime::now()).is_empty());
    }

    #[test]
    fn revalidate_detects_expired_certificates() {
        let fx = Fixture::new("expired");
        let (_r, state) = fx.reloader();
        let registry = Registry::new();
        let mut e = registry
            .register(
                1,
                hello(
                    "11111111-2222-4333-8444-555555555555",
                    Some(chain("client-expired.crt")),
                ),
                Instant::now(),
                Utc::now(),
            )
            .unwrap();
        let kicked = revalidate(&registry, &state.current(), UnixTime::now());
        assert_eq!(kicked.len(), 1);
        assert!(
            kicked[0].1.reason.to_lowercase().contains("expired"),
            "{}",
            kicked[0].1
        );
        assert!(e.kick.try_recv().is_ok());
    }

    #[test]
    fn revalidate_is_noop_without_mtls() {
        let mut fx = Fixture::new("nomtls");
        fx.paths.client_ca = None;
        let (_r, state) = fx.reloader();
        let registry = Registry::new();
        let _x = registry
            .register(1, hello(A, None), Instant::now(), Utc::now())
            .unwrap();
        assert!(revalidate(&registry, &state.current(), UnixTime::now()).is_empty());
    }
}
