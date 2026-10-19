//! 端對端：集中設定（feature `config`；crates/kairos-proto/REMOTE.md 第 3 節、PROTOCOL.md「遠端管理」）的
//! Pulse 端。Kairos 端（K1）由另一條開發線實作，這裡以測試內的假 Kairos（rustls、mTLS 埠的行為只做到
//! 集中設定需要的部分）驗證**實際的 pulse 執行檔**：
//!
//! - hello 帶 `install`、宣告 `config`；`pending.config_rev` → `config_get` → `config` → `config_status`
//!   （`applied`、本機覆蓋的鍵列在 `overridden`、`PLS-I0503`）
//! - 即時套用：沒有本機值的 `report.interval` 下發後，report 的間隔立即改變，連線不中斷
//! - 快取：`<狀態目錄>/remote/config.json`；Kairos 停機時重新啟動 Pulse 仍以快取運作，Kairos 恢復後
//!   同一版不再 `config_get`，只回報一次 `config_status`
//!
//! 遠端升級（P2）需要 root 與 /opt/pulse 的安裝，由 Pulse 的單元與整合測試（假 Kairos、假 systemd）涵蓋。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use kp_e2e::{
    GOOD, PULSE_REGISTERED, Pulse, TestDir, WAIT, free_port, poll_until, pulse_testdata,
    release_port,
};
use serde_json::{Value, json};

/// 假 Kairos 的行為
#[derive(Clone)]
struct Behavior {
    features: Vec<&'static str>,
    rev: u64,
    settings: Value,
}

struct Shared {
    behavior: Behavior,
    /// 收到的訊息（收到的時間、內容）
    log: Mutex<Vec<(Instant, Value)>>,
    streams: Mutex<Vec<TcpStream>>,
    stop: AtomicBool,
}

/// 測試內的假 Kairos（只回應 Pulse 會送的訊息）
struct FakeKairos {
    shared: Arc<Shared>,
    accept: Option<std::thread::JoinHandle<()>>,
}

fn server_config() -> Arc<rustls::ServerConfig> {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    let certs: Vec<CertificateDer<'static>> =
        CertificateDer::pem_file_iter(pulse_testdata("server.pem"))
            .unwrap()
            .map(Result::unwrap)
            .collect();
    let key = PrivateKeyDer::from_pem_file(pulse_testdata("server.key")).unwrap();
    Arc::new(
        rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .unwrap(),
    )
}

fn now_ts() -> String {
    // RFC 3339 UTC 毫秒（不依賴 chrono：e2e crate 沒有這個依賴）
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
    let secs = d.as_secs();
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // 1970-01-01 起的日數 → 年月日（Howard Hinnant 的 civil_from_days）
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { y + 1 } else { y };
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60,
        d.subsec_millis()
    )
}

fn read_line<S: Read>(s: &mut S, buf: &mut Vec<u8>) -> Option<String> {
    loop {
        if let Some(i) = buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = buf.drain(..=i).collect();
            return Some(
                String::from_utf8_lossy(&line[..line.len() - 1])
                    .trim_end_matches('\r')
                    .to_string(),
            );
        }
        let mut chunk = [0u8; 16 * 1024];
        match s.read(&mut chunk) {
            Ok(0) | Err(_) => return None,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
}

fn serve(stream: TcpStream, cfg: Arc<rustls::ServerConfig>, shared: Arc<Shared>) {
    let conn = rustls::ServerConnection::new(cfg).unwrap();
    let mut tls = rustls::StreamOwned::new(conn, stream);
    let mut buf = Vec::new();
    let config = shared.behavior.features.contains(&"config");
    let pending = json!({"config_rev": shared.behavior.rev});
    while let Some(line) = read_line(&mut tls, &mut buf) {
        let Ok(v) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        // Kairos 端的驗證：Pulse 送的每一則都必須合規
        let msg = kairos_proto::parse_client_msg(&line).unwrap_or_else(|e| panic!("{line}: {e}"));
        kairos_proto::validate_client_msg(&msg).unwrap_or_else(|e| panic!("{line}: {e}"));
        shared.log.lock().unwrap().push((Instant::now(), v.clone()));
        let seq = v["seq"].clone();
        let reply = match v["type"].as_str().unwrap_or("") {
            "hello" => {
                let mut ack = json!({"type":"hello_ack","seq":seq,"proto":2,"offline_after_ms":5000,
                    "features":shared.behavior.features,"server_time":now_ts()});
                if config {
                    ack["pending"] = pending.clone();
                }
                ack
            }
            "heartbeat" if config => json!({"type":"ack","seq":seq,"pending":pending}),
            "config_get" => serde_json::to_value(kairos_proto::ServerMsg::Config(Box::new(
                kairos_proto::RemoteConfig::new(
                    seq.as_u64().unwrap(),
                    shared.behavior.rev,
                    shared.behavior.settings.as_object().unwrap().clone(),
                    None,
                    now_ts(),
                ),
            )))
            .unwrap(),
            _ => json!({"type":"ack","seq":seq}),
        };
        let mut out = reply.to_string();
        out.push('\n');
        if tls
            .write_all(out.as_bytes())
            .and_then(|()| tls.flush())
            .is_err()
        {
            return;
        }
    }
}

impl FakeKairos {
    fn start(port: u16, behavior: Behavior) -> Self {
        release_port(port);
        let listener = TcpListener::bind(("127.0.0.1", port)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let shared = Arc::new(Shared {
            behavior,
            log: Mutex::new(Vec::new()),
            streams: Mutex::new(Vec::new()),
            stop: AtomicBool::new(false),
        });
        let cfg = server_config();
        let s = shared.clone();
        let accept = std::thread::spawn(move || {
            while !s.stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        s.streams.lock().unwrap().push(stream.try_clone().unwrap());
                        let (cfg, s) = (cfg.clone(), s.clone());
                        std::thread::spawn(move || serve(stream, cfg, s));
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(20)),
                }
            }
        });
        Self {
            shared,
            accept: Some(accept),
        }
    }

    fn of(&self, kind: &str) -> Vec<(Instant, Value)> {
        self.shared
            .log
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, v)| v["type"] == kind)
            .cloned()
            .collect()
    }

    /// 停機：不再接受連線，已建立的連線一併關閉
    fn stop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.accept.take() {
            t.join().unwrap();
        }
        for s in self.shared.streams.lock().unwrap().drain(..) {
            let _ = s.shutdown(std::net::Shutdown::Both);
        }
    }
}

impl Drop for FakeKairos {
    fn drop(&mut self) {
        self.stop();
    }
}

fn behavior() -> Behavior {
    Behavior {
        features: vec!["metrics", "durable", "config"],
        rev: 7,
        settings: json!({
            "report": {"interval": 3},
            "heartbeat": {"interval": 5},
            "log": {"level": "debug"},
            "server": {"address": "evil.example.com"}
        }),
    }
}

#[test]
fn remote_config_is_applied_live_cached_and_reported() {
    let root = TestDir::new("remote-config");
    let port = free_port();
    let mut kairos = FakeKairos::start(port, behavior());
    let pulse = Pulse::new(&root, "pulse", port, GOOD);
    // 本機沒有 [report]（讓下發的 report.interval 生效）；heartbeat.interval = 1 與 log.level = trace 是本機值
    let toml_path = pulse.dir.join("pulse.toml");
    let toml = std::fs::read_to_string(&toml_path)
        .unwrap()
        .replace("[report]\ninterval = 1\n", "");
    std::fs::write(&toml_path, toml).unwrap();
    let state = pulse.dir.join("state");
    let envs = [("STATE_DIRECTORY", state.to_str().unwrap())];
    let mut p = pulse.start(&envs);
    p.wait_for(PULSE_REGISTERED, WAIT);

    // hello：install 與 config
    let hello = kairos.of("hello")[0].1.clone();
    assert_eq!(hello["install"]["config"], "enabled", "{hello}");
    assert!(hello["install"]["method"].is_string(), "{hello}");
    let features: Vec<&str> = hello["features"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f.as_str().unwrap())
        .collect();
    assert!(features.contains(&"config"), "{features:?}");

    // config_get → config → config_status（applied）
    assert!(
        poll_until(WAIT, || kairos
            .of("config_status")
            .iter()
            .any(|(_, s)| s["rev"] == 7)),
        "{}",
        p.dump()
    );
    assert_eq!(kairos.of("config_get")[0].1["have_rev"], 0);
    let (applied_at, st) = kairos
        .of("config_status")
        .into_iter()
        .find(|(_, s)| s["rev"] == 7)
        .unwrap();
    assert_eq!(st["state"], "applied", "{st}");
    assert_eq!(st["code"], "PLS-I0503", "{st}");
    assert_eq!(st["effective"]["report"]["interval"], 3);
    assert_eq!(st["effective"]["heartbeat"]["interval"], 1, "本機優先");
    let overridden: Vec<&str> = st["overridden"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["key"].as_str().unwrap())
        .collect();
    assert_eq!(overridden, ["heartbeat.interval", "log.level"], "{st}");
    let ignored: Vec<&str> = st["ignored"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["key"].as_str().unwrap())
        .collect();
    assert_eq!(ignored, ["server.address"], "連線設定只能在本機改：{st}");
    p.wait_for("config.applied", WAIT);

    // 即時套用：之後的 report 每 3 秒一筆（連線沒有中斷）
    assert!(
        poll_until(WAIT, || kairos
            .of("report")
            .iter()
            .filter(|(t, _)| *t > applied_at)
            .count()
            >= 4),
        "{}",
        p.dump()
    );
    let times: Vec<Instant> = kairos
        .of("report")
        .into_iter()
        .filter(|(t, _)| *t > applied_at)
        .map(|(t, _)| t)
        .collect();
    let gaps: Vec<Duration> = times.windows(2).skip(1).map(|w| w[1] - w[0]).collect();
    assert!(
        gaps.iter().all(|g| *g >= Duration::from_millis(2500)),
        "下發 report.interval = 3 後的間隔：{gaps:?}"
    );
    assert_eq!(kairos.of("hello").len(), 1, "即時的鍵不需要重新連線");
    // 快取
    let cache = std::fs::read_to_string(state.join("remote/config.json")).unwrap();
    assert!(cache.contains("\"rev\": 7"), "{cache}");

    // Kairos 停機時重新啟動 Pulse：以快取的設定運作
    p.stop();
    kairos.stop();
    let mut p = pulse.start(&envs);
    p.wait_for("以快取的集中設定第 7 版啟動", WAIT);

    // Kairos 恢復：同一版不再 config_get，只回報 config_status
    let kairos = FakeKairos::start(port, behavior());
    assert!(
        poll_until(WAIT, || !kairos.of("config_status").is_empty()),
        "{}",
        p.dump()
    );
    std::thread::sleep(Duration::from_secs(2));
    let st = &kairos.of("config_status")[0].1;
    assert_eq!(
        (st["rev"].as_u64(), st["state"].as_str()),
        (Some(7), Some("applied")),
        "{st}"
    );
    assert!(
        kairos.of("config_get").is_empty(),
        "{:?}",
        kairos.of("config_get")
    );
    p.stop();
}

#[test]
fn kairos_without_config_does_not_use_the_cache() {
    let root = TestDir::new("remote-config-old");
    let port = free_port();
    let pulse = Pulse::new(&root, "pulse", port, GOOD);
    let toml_path = pulse.dir.join("pulse.toml");
    let toml = std::fs::read_to_string(&toml_path)
        .unwrap()
        .replace("[report]\ninterval = 1\n", "");
    std::fs::write(&toml_path, toml).unwrap();
    let state = pulse.dir.join("state");
    let envs = [("STATE_DIRECTORY", state.to_str().unwrap())];
    // 先在支援 config 的 Kairos 下套用 report.interval = 3
    let mut kairos = FakeKairos::start(port, behavior());
    let mut p = pulse.start(&envs);
    assert!(
        poll_until(WAIT, || kairos
            .of("config_status")
            .iter()
            .any(|(_, s)| s["rev"] == 7)),
        "{}",
        p.dump()
    );
    p.stop();
    kairos.stop();
    // 換成不支援 config 的 Kairos（舊版或 JSONL）：快取保留但不使用，report 回到內建的 5 秒
    let old = FakeKairos::start(
        port,
        Behavior {
            features: vec!["metrics", "durable"],
            ..behavior()
        },
    );
    let mut p = pulse.start(&envs);
    p.wait_for("不支援集中設定", WAIT);
    assert!(
        old.of("config_status").is_empty() && old.of("config_get").is_empty(),
        "沒有協商 config 時不送遠端管理的訊息"
    );
    assert!(state.join("remote/config.json").is_file(), "快取保留");
    p.stop();
}
