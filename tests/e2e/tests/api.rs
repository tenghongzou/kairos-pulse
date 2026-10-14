//! 端對端：實際的 Kairos 與 Pulse，經主控台 API（`kairos/API.md`）查到主機與其指標，SSE 收到
//! 上線、離線事件；撤銷憑證後主機詳情顯示「憑證被拒」（設計包 §24 #3）。token 以實際的
//! `kairos api-token create` 建立，Kairos 在 10 秒內把建立寫入日誌。

use kp_e2e::{
    ApiStream, GOOD, Kairos, PULSE_REGISTERED, Pulse, REVOKED, TestDir, WAIT, poll_until, test_pki,
};

#[test]
fn api_lists_hosts_and_metrics_and_streams_online_offline_events() {
    let root = TestDir::new("api");
    let kairos = Kairos::new(&root, test_pki::CRL_EMPTY);
    let mut k = kairos.start();
    k.wait_for("主控台 API 已啟動", WAIT);
    let token = kairos.create_token("e2e");
    // 稽核：執行中的 Kairos 把 token 的建立寫入日誌（不含秘密）
    k.wait_for("API token 已建立", WAIT);
    let secret = token.splitn(3, '_').nth(2).unwrap();
    assert!(!k.stdout().contains(secret), "日誌不應出現 token 的秘密");
    let api = kairos.api(&token);

    // 沒有 token：401
    let anonymous = kp_e2e::Api {
        port: kairos.console_port,
        token: "nope".into(),
    };
    let (status, v) = anonymous.get("/api/v1/hosts");
    assert_eq!(status, 401, "{v}");
    assert_eq!(v["error"]["code"], "unauthorized");
    assert_eq!(v["error"]["product_code"], "KRS-E6010");

    // 先開 SSE，再啟動 Pulse：新主機上線 → host.added
    let mut events = ApiStream::open(&api, None);
    let ready = events.next_event();
    assert_eq!(ready.event, "ready");
    let pulse = Pulse::new(&root, "pulse", kairos.port, GOOD);
    let mut p = pulse.start(&[]);
    p.wait_for(PULSE_REGISTERED, WAIT);
    let added = events.wait_for("host.added", GOOD.agent_id);
    assert_eq!(added.data["status"], "online");
    assert_eq!(added.data["ip"], "127.0.0.1");
    assert!(added.id.is_some());

    // 主機清單：線上、IP／CIDR 搜尋、狀態計數
    assert!(
        poll_until(WAIT, || {
            api.data("/api/v1/hosts?status=online")[0]["agent_id"] == GOOD.agent_id
        }),
        "主機清單應列出線上的主機\n{}",
        k.dump()
    );
    let (status, list) = api.get("/api/v1/hosts?q=127.0.0.0%2F8");
    assert_eq!(status, 200, "{list}");
    assert_eq!(list["page"]["total"], 1, "{list}");
    assert_eq!(list["meta"]["status_counts"]["online"], 1, "{list}");
    let host = &list["data"][0];
    assert_eq!(host["hostname"].as_str().map(str::is_empty), Some(false));
    assert_eq!(host["identity_verified"], true, "{host}");

    // 詳情：補送（durable）與最近的 report
    let detail = api.data(&format!("/api/v1/hosts/{}", GOOD.agent_id));
    assert_eq!(detail["status"], "online", "{detail}");
    assert_eq!(detail["connection"]["durable"], true, "{detail}");
    assert!(detail["host_fingerprint"].as_str().is_some(), "{detail}");

    // 指標：Pulse 每秒一筆，等到 CPU 的趨勢有點
    assert!(
        poll_until(WAIT, || {
            let (status, v) = api.get(&format!(
                "/api/v1/hosts/{}/metrics/system.cpu.usage?range=1h",
                GOOD.agent_id
            ));
            let t = &v["data"];
            status == 200
                && t["resolution"] == "raw"
                && t["series"][0]["points"]
                    .as_array()
                    .is_some_and(|p| p.len() >= 2)
        }),
        "趨勢應有原始資料的點\n{}",
        k.dump()
    );
    let metrics = api.data(&format!("/api/v1/hosts/{}/metrics", GOOD.agent_id));
    assert!(
        metrics["metrics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["name"] == "system.memory.used"),
        "{metrics}"
    );
    let overview = api.data("/api/v1/overview");
    assert_eq!(overview["status_counts"]["online"], 1, "{overview}");
    let health = api.data("/api/v1/system/health");
    let writes = health["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == "report_writes")
        .unwrap()
        .clone();
    assert!(
        writes["values"]["since_start"]["written"].as_u64().unwrap() >= 2,
        "{writes}"
    );
    assert_eq!(health["items"][3]["id"], "delivery");
    assert_eq!(health["items"][3]["values"]["durable"], 1, "{health}");

    // Pulse 停止 → host.offline（連線關閉）
    let (code, _) = p.stop();
    assert_eq!(code, 0, "{}", p.dump());
    let offline = events.wait_for("host.offline", GOOD.agent_id);
    assert_eq!(offline.data["reason"], "connection_closed", "{:?}", offline);
    assert!(
        poll_until(WAIT, || {
            let v = api.data("/api/v1/hosts?status=offline");
            v[0]["agent_id"] == GOOD.agent_id
                && v[0]["status_detail"]["offline_reason"] == "connection_closed"
        }),
        "主機清單應顯示離線與原因"
    );
    // 以 Last-Event-ID 重連：補送之後的事件
    let mut resumed = ApiStream::open(&api, added.id.as_deref());
    let ready = resumed.next_event();
    assert_eq!(ready.event, "ready");
    assert_eq!(ready.data["resumed"], true, "{:?}", ready);
    assert_eq!(
        resumed.wait_for("host.offline", GOOD.agent_id).id,
        offline.id
    );

    let (code, _) = k.stop();
    assert_eq!(code, 0, "{}", k.dump());
}

#[test]
fn revoked_certificate_is_attributed_to_the_host() {
    // 設計包 §24 #3：握手時被拒的憑證以 SAN 歸屬到 agent_id，主機詳情顯示「憑證被拒」
    let root = TestDir::new("api-revoke");
    let kairos = Kairos::new(&root, test_pki::CRL_EMPTY);
    let mut k = kairos.start();
    let api = kairos.api(&kairos.create_token("e2e"));
    let pulse = Pulse::new(&root, "pulse", kairos.port, REVOKED);
    let mut p = pulse.start(&[]);
    p.wait_for(PULSE_REGISTERED, WAIT);
    let mut events = ApiStream::open(&api, None);
    assert_eq!(events.next_event().event, "ready");
    // 主機已寫入清冊（第一筆 report）
    assert!(
        poll_until(WAIT, || api
            .get(&format!("/api/v1/hosts/{}", REVOKED.agent_id))
            .0
            == 200),
        "主機應出現在清冊\n{}",
        k.dump()
    );

    // 撤銷：連線被踢（離線原因 certificate_rejected），之後的重連在握手時被拒
    kairos.replace_crl(test_pki::CRL_REVOKE);
    let offline = events.wait_for("host.offline", REVOKED.agent_id);
    assert_eq!(
        offline.data["reason"], "certificate_rejected",
        "{offline:?}"
    );
    k.wait_for("出示的客戶端憑證已被撤銷，TLS 握手失敗", WAIT);
    assert!(
        poll_until(WAIT, || {
            let d = api.data(&format!("/api/v1/hosts/{}", REVOKED.agent_id));
            d["certificate_rejection"]["reason"] == "revoked"
                && d["certificate_rejection"]["product_code"] == "KRS-W1105"
                && d["certificate_rejection"]["peer"] == "127.0.0.1"
                && d["status_detail"]["offline_reason"] == "certificate_rejected"
        }),
        "主機詳情應顯示憑證被拒\n{}",
        k.dump()
    );
    let v = api.data("/api/v1/hosts?offline_reason=certificate_rejected");
    assert_eq!(v[0]["agent_id"], REVOKED.agent_id, "{v}");

    let (code, _) = p.stop();
    assert_eq!(code, 0, "{}", p.dump());
    let (code, _) = k.stop();
    assert_eq!(code, 0, "{}", k.dump());
}

#[test]
fn repeated_authentication_failures_are_rate_limited_with_product_codes() {
    // 設計包 §26：401 為 KRS-E6010、429 為 KRS-W6011；第一次限制這個來源時的 WARN 日誌帶 KRS-W6011
    let root = TestDir::new("api-limit");
    let kairos = Kairos::new(&root, test_pki::CRL_EMPTY);
    let mut k = kairos.start();
    k.wait_for("主控台 API 已啟動", WAIT);
    let anonymous = kp_e2e::Api {
        port: kairos.console_port,
        token: "kat_wrong".into(),
    };
    let mut limited = None;
    for _ in 0..12 {
        let (status, v) = anonymous.get("/api/v1/me");
        if status == 429 {
            limited = Some(v);
            break;
        }
        assert_eq!(status, 401, "{v}");
        assert_eq!(v["error"]["product_code"], "KRS-E6010", "{v}");
    }
    let v = limited.unwrap_or_else(|| panic!("驗證失敗 10 次後應回 429\n{}", k.dump()));
    assert_eq!(v["error"]["code"], "rate_limited", "{v}");
    assert_eq!(v["error"]["product_code"], "KRS-W6011", "{v}");
    k.wait_for("警告 KRS-W6011：主控台 API", WAIT);
    // 限制期間的請求不再記 WARN
    assert_eq!(anonymous.get("/api/v1/me").0, 429);
    assert_eq!(k.count("KRS-W6011"), 1, "{}", k.dump());
    let (code, _) = k.stop();
    assert_eq!(code, 0, "{}", k.dump());
}
