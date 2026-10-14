//! 註冊通道的單元測試。
//!
//! 規格與測試只有一份來源：ENROLL.md 的「錯誤碼」表、「範例」節的每一則請求與回應、「測試向量」節
//! 的輸入與預期結果都在這裡解析並比對，修改規格時這裡的測試會指出不一致的地方。

use super::*;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::collections::HashMap;

/// 規格全文
const SPEC: &str = include_str!("../../ENROLL.md");

/// `## <title>` 這一節的各行（到下一個 `## ` 為止）
fn section(title: &str) -> Vec<&'static str> {
    let heading = format!("## {title}");
    let mut lines = SPEC.lines();
    assert!(lines.any(|l| l == heading), "ENROLL.md 找不到「{title}」節");
    lines.take_while(|l| !l.starts_with("## ")).collect()
}

/// 某一節中 `### <sub>` 小節的第一個程式碼區塊的內容
fn code_block(sec: &str, sub: &str) -> Vec<&'static str> {
    let lines = section(sec);
    let heading = format!("### {sub}");
    let start = lines
        .iter()
        .position(|l| *l == heading)
        .unwrap_or_else(|| panic!("ENROLL.md「{sec}」節找不到「{sub}」小節"));
    let rest = &lines[start + 1..];
    let open = rest
        .iter()
        .position(|l| l.starts_with("```"))
        .expect("小節中沒有程式碼區塊");
    rest[open + 1..]
        .iter()
        .take_while(|l| !l.starts_with("```"))
        .copied()
        .collect()
}

/// 「測試向量」節中 `key = value` 形式的區塊
fn kv(sub: &str) -> HashMap<&'static str, &'static str> {
    code_block("測試向量", sub)
        .into_iter()
        .map(|l| l.split_once(" = ").expect("key = value"))
        .collect()
}

struct Vectors {
    input: HashMap<&'static str, &'static str>,
    expected: HashMap<&'static str, &'static str>,
}

impl Vectors {
    fn load() -> Self {
        Self {
            input: kv("輸入"),
            expected: kv("預期結果"),
        }
    }
    fn i(&self, k: &str) -> &'static str {
        self.input.get(k).unwrap_or_else(|| panic!("缺少輸入 {k}"))
    }
    fn e(&self, k: &str) -> &'static str {
        self.expected
            .get(k)
            .unwrap_or_else(|| panic!("缺少預期結果 {k}"))
    }
    fn bytes(&self, k: &str) -> Vec<u8> {
        b64url_decode(self.i(k)).unwrap()
    }
}

fn hex_decode(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}

// ---------------------------------------------------------------- 錯誤碼表

#[test]
fn error_table_matches_codes() {
    let rows: Vec<Vec<String>> = section("錯誤碼")
        .into_iter()
        .filter(|l| l.starts_with("| `"))
        .map(|l| {
            l.trim_matches('|')
                .split('|')
                .map(|c| c.trim().trim_matches('`').to_string())
                .collect()
        })
        .collect();
    assert_eq!(rows.len(), EnrollErrorCode::ALL.len(), "表格列數");
    let opt = |s: &str| (s != "—").then(|| s.to_string());
    for (row, code) in rows.iter().zip(EnrollErrorCode::ALL) {
        assert_eq!(row[0], code.as_str());
        assert_eq!(row[1], code.http_status().to_string(), "{}", row[0]);
        assert_eq!(
            opt(&row[2]),
            code.kairos_code().map(str::to_string),
            "{}",
            row[0]
        );
        assert_eq!(
            opt(&row[3]),
            code.pulse_code().map(str::to_string),
            "{}",
            row[0]
        );
        assert_eq!(EnrollErrorCode::from_code(code.as_str()), Some(code));
        assert_eq!(
            serde_json::to_value(code).unwrap(),
            Value::String(code.as_str().to_string())
        );
    }
    assert_eq!(EnrollErrorCode::from_code("no_such_code"), None);
}

#[test]
fn mirror_codes_share_numbers() {
    // 鏡像規則：兩端都有代碼時號碼相同（copywriting.md §3.1）；enroll_busy 例外（Pulse 歸到 2202）
    for code in EnrollErrorCode::ALL {
        if let (Some(k), Some(p)) = (code.kairos_code(), code.pulse_code())
            && code != EnrollErrorCode::EnrollBusy
        {
            assert_eq!(k[5..], p[5..], "{}", code.as_str());
        }
    }
}

// ---------------------------------------------------------------- 範例

/// 解析為 T、序列化回去，必須與原文的 JSON 值相同
fn roundtrip<T: Serialize + DeserializeOwned>(json: &str) -> T {
    let original: Value = serde_json::from_str(json).expect("範例必須是合法 JSON");
    let typed: T = serde_json::from_str(json)
        .unwrap_or_else(|e| panic!("無法解析為 {}: {e}\n{json}", std::any::type_name::<T>()));
    assert_eq!(serde_json::to_value(&typed).unwrap(), original, "{json}");
    typed
}

fn check_server_time(t: &str) {
    let ms = parse_rfc3339_ms(t).unwrap();
    assert_eq!(
        format_rfc3339_ms(ms),
        t,
        "server_time 必須是 UTC 毫秒的 Z 格式"
    );
}

#[test]
fn examples_parse_validate_and_roundtrip() {
    let v = Vectors::load();
    let endpoint = v.i("enroll_endpoint");
    let new_spki = b64url_decode(v.e("new_spki")).unwrap();
    let old_spki = b64url_decode(v.e("old_spki")).unwrap();
    let code = RequestCode::parse(v.e("request_code")).unwrap();

    let mut route: Option<Route> = None;
    let (mut requests, mut responses) = (0, 0);
    for line in section("範例") {
        if let Some(req) = line.strip_prefix("> ") {
            requests += 1;
            let mut parts = req.splitn(3, ' ');
            let method = parts.next().unwrap();
            let target = parts.next().unwrap();
            let body = parts.next();
            let (path, query) = match target.split_once('?') {
                Some((p, q)) => (p, Some(q)),
                None => (target, None),
            };
            let r = parse_route(method, path).unwrap_or_else(|e| panic!("{req}: {e:?}"));
            assert_eq!(r.path(), path);
            match (&r, body) {
                (Route::ServerInfo | Route::Poll(_), None) => {
                    assert!(parse_wait_secs(query).is_some());
                }
                (Route::TokenCheck, Some(b)) => {
                    validate_token_check(&roundtrip::<TokenCheckRequest>(b)).unwrap();
                }
                (Route::CreateRequest, Some(b)) => {
                    let checked = validate_enroll_request(&roundtrip::<EnrollRequest>(b)).unwrap();
                    assert_eq!(checked.csr_der, v.bytes("csr"));
                }
                (Route::CreateRenewal, Some(b)) => {
                    let req: GraceRenewRequest = roundtrip(b);
                    let checked = validate_grace_renew_request(&req).unwrap();
                    let input = grace_renew_signing_input(
                        &req.agent_id,
                        &req.host_fingerprint,
                        &checked.csr_der,
                        &req.sent_at,
                        endpoint,
                    )
                    .unwrap();
                    verify_ecdsa_p256_sha256(&old_spki, &input, &req.signature).unwrap();
                }
                (Route::WithdrawChallenge(_), Some(b)) => assert_eq!(b, "{}"),
                (Route::Withdraw(id), Some(b)) => {
                    let req: WithdrawRequest = roundtrip(b);
                    validate_withdraw_request(id, &req).unwrap();
                    let input = withdraw_signing_input(
                        &req.request_id,
                        &req.agent_id,
                        &req.nonce,
                        endpoint,
                    )
                    .unwrap();
                    verify_ecdsa_p256_sha256(&new_spki, &input, &req.signature).unwrap();
                }
                other => panic!("請求與本文不符: {other:?}"),
            }
            route = Some(r);
        } else if let Some(res) = line.strip_prefix("< ") {
            responses += 1;
            let (status, body) = res.split_once(' ').unwrap();
            let status: u16 = status.parse().unwrap();
            let r = route.take().expect("回應前必須有請求");
            if status == 200 {
                let time = match &r {
                    Route::ServerInfo => roundtrip::<ServerInfo>(body).server_time,
                    Route::TokenCheck => roundtrip::<TokenCheckResponse>(body).server_time,
                    Route::CreateRequest | Route::CreateRenewal | Route::Poll(_) => {
                        let res: EnrollResult = roundtrip(body);
                        assert_eq!(res.request_code, code);
                        match res.state {
                            RequestState::PendingApproval => {
                                assert!(res.poll_after_ms.is_some() && res.expires_at.is_some())
                            }
                            RequestState::Issued => {
                                let c = res.certificate.as_ref().unwrap();
                                assert!(is_serial_hex(&c.serial));
                                assert!(res.policy.is_some());
                            }
                            _ => {}
                        }
                        res.server_time
                    }
                    Route::WithdrawChallenge(_) => {
                        let res: WithdrawChallenge = roundtrip(body);
                        let ttl = parse_rfc3339_ms(&res.nonce_expires_at).unwrap()
                            - parse_rfc3339_ms(&res.server_time).unwrap();
                        assert_eq!(ttl, WITHDRAW_NONCE_TTL_MS);
                        res.server_time
                    }
                    Route::Withdraw(_) => {
                        let res: WithdrawResult = roundtrip(body);
                        assert_eq!(res.state, RequestState::Withdrawn);
                        res.server_time
                    }
                    Route::Download(_) => unreachable!(),
                };
                check_server_time(&time);
            } else {
                let err: ErrorResponse = roundtrip(body);
                check_server_time(&err.server_time);
                let code = err
                    .error
                    .known_code()
                    .expect("範例的錯誤碼必須在錯誤碼表中");
                assert_eq!(code.http_status(), status, "{body}");
                if matches!(status, 429 | 503) {
                    assert!(err.error.retry_after_ms.is_some(), "{body}");
                }
                if code == EnrollErrorCode::ClockSkew {
                    let e = &err.error;
                    let offset = clock_offset_ms(
                        parse_rfc3339_ms(e.client_time.as_deref().unwrap()).unwrap(),
                        parse_rfc3339_ms(&err.server_time).unwrap(),
                    );
                    assert_eq!(e.offset_ms, Some(offset));
                    assert!(exceeds_clock_skew(offset));
                    assert_eq!(e.max_skew_ms, Some(MAX_CLOCK_SKEW_MS));
                }
            }
        }
    }
    assert!(route.is_none(), "最後一個請求沒有回應");
    assert_eq!(requests, responses);
    assert!(requests >= 15, "範例太少：{requests}");
}

// ---------------------------------------------------------------- 測試向量

#[test]
fn vector_keys_csr_and_derived_values() {
    let v = Vectors::load();
    let old_spki = p256_spki_of_pkcs8(&v.bytes("old_key_pkcs8")).unwrap();
    let new_spki = p256_spki_of_pkcs8(&v.bytes("new_key_pkcs8")).unwrap();
    assert_eq!(b64url_encode(&old_spki), v.e("old_spki"));
    assert_eq!(b64url_encode(&new_spki), v.e("new_spki"));

    let csr = v.bytes("csr");
    assert_eq!(sha256_hex(&csr), v.e("csr_sha256"));
    assert_eq!(
        request_code_from_spki(&new_spki).to_string(),
        v.e("request_code")
    );
    assert_eq!(spki_fingerprint(&new_spki), v.e("spki_fingerprint"));
    assert_eq!(
        spki_fingerprint_short(&new_spki),
        v.e("spki_fingerprint_short")
    );

    let ca_spki = v.bytes("ca_spki");
    assert_eq!(ca_pin_from_spki(&ca_spki), v.e("ca_pin"));
    let display = ca_pin_display(v.e("ca_pin")).unwrap();
    assert_eq!(display[0], v.e("ca_pin_display_1"));
    assert_eq!(display[1], v.e("ca_pin_display_2"));
    // 從顯示格式複製回來也能還原
    let pasted = format!("{}\n{}", display[0], display[1]);
    assert_eq!(normalize_ca_pin(&pasted).unwrap(), v.e("ca_pin"));
}

#[test]
fn vector_csr_parses_with_selected_crates() {
    // ENROLL.md「CSR 與憑證的套件」：Kairos 以 x509-parser 解析與驗證 CSR
    use x509_parser::extensions::{GeneralName, ParsedExtension};
    use x509_parser::prelude::FromDer;
    use x509_parser::x509::X509Name;
    let v = Vectors::load();
    let der = v.bytes("csr");
    let (rest, csr) =
        x509_parser::certification_request::X509CertificationRequest::from_der(&der).unwrap();
    assert!(rest.is_empty());
    csr.verify_signature().unwrap();
    let info = &csr.certification_request_info;
    assert_eq!(b64url_encode(info.subject_pki.raw), v.e("new_spki"));
    let cn = |name: &X509Name| {
        name.iter_common_name()
            .next()
            .and_then(|c| c.as_str().ok())
            .map(str::to_string)
    };
    assert_eq!(cn(&info.subject).as_deref(), Some(v.i("agent_id")));
    let sans: Vec<String> = csr
        .requested_extensions()
        .into_iter()
        .flatten()
        .filter_map(|e| match e {
            ParsedExtension::SubjectAlternativeName(san) => Some(san.general_names.clone()),
            _ => None,
        })
        .flatten()
        .map(|g| match g {
            GeneralName::DNSName(d) => d.to_string(),
            other => format!("{other:?}"),
        })
        .collect();
    assert_eq!(sans, vec![v.i("agent_id").to_string()]);
}

#[test]
fn vector_csr_generation_with_rcgen() {
    // Pulse 以 rcgen 產生的 CSR：公鑰與私鑰一致，申請編號只取決於公鑰
    use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair};
    let v = Vectors::load();
    let pkcs8 = v.bytes("new_key_pkcs8");
    let key = KeyPair::try_from(pkcs8.as_slice()).unwrap();
    let mut params = CertificateParams::new(vec![v.i("agent_id").to_string()]).unwrap();
    params.distinguished_name = DistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::CommonName, v.i("agent_id"));
    let csr = params.serialize_request(&key).unwrap();
    use x509_parser::prelude::FromDer;
    let (_, parsed) =
        x509_parser::certification_request::X509CertificationRequest::from_der(csr.der()).unwrap();
    let spki = parsed.certification_request_info.subject_pki.raw;
    assert_eq!(b64url_encode(spki), v.e("new_spki"));
    assert_eq!(
        request_code_from_spki(spki).to_string(),
        v.e("request_code")
    );
}

#[test]
fn vector_signing_inputs_and_signatures() {
    let v = Vectors::load();
    let endpoint = v.i("enroll_endpoint");
    let withdraw =
        withdraw_signing_input(v.i("request_id"), v.i("agent_id"), v.i("nonce"), endpoint).unwrap();
    assert_eq!(
        String::from_utf8(withdraw.clone()).unwrap(),
        code_block("測試向量", "撤回的簽章內容").join("\n")
    );
    let grace = grace_renew_signing_input(
        v.i("agent_id"),
        v.i("host_fingerprint"),
        &v.bytes("csr"),
        v.i("sent_at"),
        endpoint,
    )
    .unwrap();
    assert_eq!(
        String::from_utf8(grace.clone()).unwrap(),
        code_block("測試向量", "過期續簽的簽章內容").join("\n")
    );
    assert!(!grace.ends_with(b"\n") && !withdraw.ends_with(b"\n"));
    assert_eq!(hex_lower(&replay_cache_key(&grace)), v.e("replay_key"));

    let new_spki = b64url_decode(v.e("new_spki")).unwrap();
    let old_spki = b64url_decode(v.e("old_spki")).unwrap();
    // 文件中的簽章可以驗證
    verify_ecdsa_p256_sha256(&new_spki, &withdraw, v.e("withdraw_signature")).unwrap();
    verify_ecdsa_p256_sha256(&old_spki, &grace, v.e("grace_signature")).unwrap();
    // 換了金鑰、內容或用途就不通過
    assert_eq!(
        verify_ecdsa_p256_sha256(&old_spki, &withdraw, v.e("withdraw_signature")),
        Err(SignatureError::Invalid)
    );
    assert_eq!(
        verify_ecdsa_p256_sha256(&new_spki, &grace, v.e("grace_signature")),
        Err(SignatureError::Invalid)
    );
    assert_eq!(
        verify_ecdsa_p256_sha256(&new_spki, &grace, v.e("withdraw_signature")),
        Err(SignatureError::Invalid)
    );
    // 實作簽出的簽章（每次不同）也能驗證
    let fresh = sign_ecdsa_p256_sha256(&v.bytes("old_key_pkcs8"), &grace).unwrap();
    assert_ne!(fresh, v.e("grace_signature"));
    verify_ecdsa_p256_sha256(&old_spki, &grace, &fresh).unwrap();

    // 端點的寫法不同但正規化後相同時，簽章內容相同
    let upper = withdraw_signing_input(
        &v.i("request_id").to_ascii_uppercase(),
        &v.i("agent_id").to_ascii_uppercase(),
        v.i("nonce"),
        "KAIROS.Example.gov.tw.:08444",
    )
    .unwrap();
    assert_eq!(upper, withdraw);
    // sent_at 逐字保留（不正規化）：不同寫法得到不同內容，重放快取鍵也不同
    let other = grace_renew_signing_input(
        v.i("agent_id"),
        v.i("host_fingerprint"),
        &v.bytes("csr"),
        "2026-10-11T10:13:05.000+08:00",
        endpoint,
    )
    .unwrap();
    assert_ne!(replay_cache_key(&other), replay_cache_key(&grace));
}

#[test]
fn vector_token_and_install_string() {
    let v = Vectors::load();
    let random: [u8; 10] = hex_decode(v.i("token_random")).try_into().unwrap();
    let token = EnrollToken::from_random(&random);
    assert_eq!(token.canonical(), v.e("token"));
    assert_eq!(token.hint(), v.e("token_hint"));
    assert_eq!(token.prefix4(), v.e("token_prefix4"));
    assert_eq!(hex_lower(&token.lookup_hash()), v.e("token_hash"));
    assert_eq!(EnrollToken::parse(v.e("token")).unwrap(), token);
    // 不分大小寫、可省略連字號與空白；I／L 視為 1、O 視為 0
    let typed = v
        .e("token")
        .to_ascii_lowercase()
        .replace('-', " ")
        .replace('1', "l");
    assert_eq!(EnrollToken::parse(&typed).unwrap(), token);
    assert!(!format!("{token:?}").contains(&v.e("token")[5..]));

    let base = InstallString {
        s: "kairos.example.gov.tw:8443".to_string(),
        e: None,
        t: v.e("token").to_string(),
        p: v.e("ca_pin").to_string(),
        g: None,
    };
    assert_eq!(base.encode().unwrap(), v.e("install_string"));
    assert_eq!(InstallString::decode(v.e("install_string")).unwrap(), base);
    let full = InstallString {
        s: "Kairos.Example.gov.tw:8443".to_string(),
        e: Some("enroll.example.gov.tw:9444".to_string()),
        g: Some(vec!["site:tainan".to_string(), "line:5".to_string()]),
        ..base.clone()
    };
    assert_eq!(full.encode().unwrap(), v.e("install_string_full"));
    let decoded = InstallString::decode(v.e("install_string_full")).unwrap();
    assert_eq!(decoded.s, "kairos.example.gov.tw:8443");
    assert_eq!(decoded.e.as_deref(), Some("enroll.example.gov.tw:9444"));
    // 複製時多了結尾的 = 或前後空白仍可解析
    let padded = format!("  {}==\n", v.e("install_string"));
    assert_eq!(InstallString::decode(&padded).unwrap(), base);
}

// ---------------------------------------------------------------- 單元測試

#[test]
fn b64url_roundtrip_and_strictness() {
    for len in 0..70 {
        let data: Vec<u8> = (0..len).map(|i| (i * 53 + 7) as u8).collect();
        let enc = b64url_encode(&data);
        assert!(!enc.contains('='));
        assert_eq!(b64url_decode(&enc).unwrap(), data);
    }
    assert_eq!(b64url_encode(b"\xfb\xff"), "-_8");
    assert_eq!(b64url_decode("AA=="), Err(B64Error::InvalidChar));
    assert_eq!(b64url_decode("A+/A"), Err(B64Error::InvalidChar));
    assert_eq!(b64url_decode("A A"), Err(B64Error::InvalidChar));
    assert_eq!(b64url_decode("AAAAA"), Err(B64Error::InvalidLength));
    // 未使用的位元不是 0：同一個 byte 的另一種寫法
    assert_eq!(b64url_decode("AA").unwrap(), vec![0]);
    assert_eq!(b64url_decode("AB"), Err(B64Error::NonCanonical));
    assert_eq!(b64url_decode("AAB"), Err(B64Error::NonCanonical));
    assert!(b64url_decode_exact("AAAA", 2).is_err());
}

#[test]
fn token_check_symbol_detects_typos() {
    // α 的階為 31：權重 α^1..α^15 相異且非零
    let mut powers = vec![];
    let mut w = 1u8;
    for _ in 0..31 {
        w = super::token::gf32_mul(w, 2);
        powers.push(w);
    }
    assert_eq!(*powers.last().unwrap(), 1);
    let mut uniq = powers.clone();
    uniq.sort_unstable();
    uniq.dedup();
    assert_eq!(uniq.len(), 31);

    let token =
        EnrollToken::from_random(&[0x83, 0x91, 0x7a, 0x52, 0x1c, 0xe4, 0x0b, 0x66, 0xd9, 0x3f]);
    let compact: Vec<u8> = token.compact().into_bytes();
    // 任一個字元（含檢查碼）換成另一個字元
    for pos in 4..20 {
        for &c in CROCKFORD_ALPHABET.iter() {
            if c == compact[pos] {
                continue;
            }
            let mut t = compact.clone();
            t[pos] = c;
            assert_eq!(
                EnrollToken::parse(std::str::from_utf8(&t).unwrap()),
                Err(TokenFormatError::Check),
                "pos {pos} → {}",
                c as char
            );
        }
    }
    // 任兩個不同的秘密字元互換
    for a in 4..19 {
        for b in a + 1..19 {
            if compact[a] == compact[b] {
                continue;
            }
            let mut t = compact.clone();
            t.swap(a, b);
            assert!(EnrollToken::parse(std::str::from_utf8(&t).unwrap()).is_err());
        }
    }
    assert_eq!(
        EnrollToken::from_random(&[0; 10]).canonical(),
        "KRT1-0000-0000-0000-0000"
    );
    assert!(
        EnrollToken::from_random(&[0xff; 10])
            .canonical()
            .starts_with("KRT1-ZZZZ-ZZZZ-ZZZZ-ZZZ")
    );
}

#[test]
fn token_format_errors() {
    assert_eq!(
        EnrollToken::parse("KRT1-0000"),
        Err(TokenFormatError::Length)
    );
    assert_eq!(
        EnrollToken::parse("KRT2-0000-0000-0000-0000"),
        Err(TokenFormatError::Prefix)
    );
    assert_eq!(
        EnrollToken::parse("KRT1-U000-0000-0000-0000"),
        Err(TokenFormatError::Char)
    );
    assert_eq!(
        EnrollToken::parse("KRT1-0000-0000-0000-0001"),
        Err(TokenFormatError::Check)
    );
    assert!(EnrollToken::parse("krt1 0000 0000 0000 oooo").is_ok());
}

#[test]
fn request_code_parse_and_serde() {
    let code = RequestCode::parse("7kq2 m9xd").unwrap();
    assert_eq!(code.to_string(), "7KQ2-M9XD");
    assert_eq!(
        RequestCode::parse("7KQ2-M9XO").unwrap().to_string(),
        "7KQ2-M9X0"
    );
    assert_eq!(RequestCode::parse("7KQ2-M9X"), None);
    assert_eq!(RequestCode::parse("7KQ2-M9XDD"), None);
    assert_eq!(RequestCode::parse("7KQ2-M9XU"), None);
    assert_eq!(serde_json::to_string(&code).unwrap(), "\"7KQ2-M9XD\"");
    assert!(serde_json::from_str::<RequestCode>("\"7KQ2-M9XD\"").is_ok());
    // 線上只接受正規寫法
    for bad in ["\"7kq2-m9xd\"", "\"7KQ2M9XD\"", "\"7KQ2-M9XO\""] {
        assert!(serde_json::from_str::<RequestCode>(bad).is_err(), "{bad}");
    }
}

#[test]
fn ca_pin_normalization() {
    let hex = "94e46debefb80d4362fe1392a94cdec9bdf6419cd6f004bab17ad43663994658";
    let pin = format!("sha256:{hex}");
    for input in [
        pin.clone(),
        hex.to_ascii_uppercase(),
        format!("SHA256:{}", hex.to_ascii_uppercase()),
        hex.as_bytes()
            .chunks(2)
            .map(|c| std::str::from_utf8(c).unwrap())
            .collect::<Vec<_>>()
            .join(":"),
    ] {
        assert_eq!(normalize_ca_pin(&input).unwrap(), pin, "{input}");
    }
    assert!(normalize_ca_pin("sha256:abc").is_err());
    assert!(normalize_ca_pin(&format!("sha1:{hex}")).is_err());
    assert!(normalize_ca_pin(&format!("{hex}0")).is_err());
}

#[test]
fn endpoint_normalization() {
    let ok = [
        ("kairos.example.gov.tw:8444", "kairos.example.gov.tw:8444"),
        ("Kairos.Example.GOV.tw.:08444", "kairos.example.gov.tw:8444"),
        ("10.0.0.5:8444", "10.0.0.5:8444"),
        ("[2001:DB8:0:0::1]:8444", "[2001:db8::1]:8444"),
        ("localhost:1", "localhost:1"),
        ("a-b.c1:65535", "a-b.c1:65535"),
    ];
    for (input, want) in ok {
        assert_eq!(normalize_endpoint(input).unwrap(), want, "{input}");
    }
    for bad in [
        "kairos.example.gov.tw",
        "kairos.example.gov.tw:",
        "kairos.example.gov.tw:0",
        "kairos.example.gov.tw:65536",
        "kairos.example.gov.tw:+80",
        "2001:db8::1:8444",
        "[2001:db8::1]8444",
        "-bad.example:8444",
        "bad_.example:8444",
        "1.2.3:8444",
        "010.0.0.5:8444",
        ":8444",
        "a..b:8444",
    ] {
        assert!(normalize_endpoint(bad).is_err(), "{bad}");
    }
    assert_eq!(endpoint_host("[2001:db8::1]:8444"), "2001:db8::1");
    assert_eq!(
        endpoint_host("kairos.example.gov.tw:8444"),
        "kairos.example.gov.tw"
    );
}

#[test]
fn sent_at_and_clock_skew() {
    let base = parse_rfc3339_ms("2026-10-11T02:13:05.000Z").unwrap();
    assert_eq!(
        parse_rfc3339_ms("2026-10-11T10:13:05.000+08:00").unwrap(),
        base
    );
    assert_eq!(parse_rfc3339_ms("2026-10-11T02:13:05Z").unwrap(), base);
    for bad in [
        "2026-10-11 02:13:05Z",
        "2026-10-11t02:13:05Z",
        "2026-10-11T02:13:05",
        "1760148785000",
        "",
    ] {
        assert!(parse_rfc3339_ms(bad).is_err(), "{bad}");
    }
    assert!(parse_rfc3339_ms(&format!("2026-10-11T02:13:05.{}Z", "0".repeat(60))).is_err());
    assert_eq!(format_rfc3339_ms(base + 120), "2026-10-11T02:13:05.120Z");
    // 恰好 5 分鐘受理，多 1 毫秒拒絕；兩個方向相同
    assert!(!exceeds_clock_skew(clock_offset_ms(
        base,
        base + MAX_CLOCK_SKEW_MS
    )));
    assert!(!exceeds_clock_skew(clock_offset_ms(
        base + MAX_CLOCK_SKEW_MS,
        base
    )));
    assert!(exceeds_clock_skew(clock_offset_ms(
        base,
        base + MAX_CLOCK_SKEW_MS + 1
    )));
    assert!(exceeds_clock_skew(clock_offset_ms(
        base + MAX_CLOCK_SKEW_MS + 1,
        base
    )));
    assert_eq!(clock_offset_ms(base, base + 462_120), -462_120);
}

#[test]
fn signing_input_rejects_bad_fields() {
    let id = "0d5f7b2e-3c41-4a8e-9b6f-2e7c1d9a4b30";
    let agent = "6f1c0e4a-1b2c-4d3e-9f00-112233445566";
    let nonce = b64url_encode(&[7; 32]);
    let ep = "kairos.example.gov.tw:8444";
    let fp = format!("sha256:{}", "ab".repeat(32));
    let field = |r: Result<Vec<u8>, SigningInputError>| r.unwrap_err().field;
    assert_eq!(
        field(withdraw_signing_input("x", agent, &nonce, ep)),
        "request_id"
    );
    assert_eq!(
        field(withdraw_signing_input(id, "x", &nonce, ep)),
        "agent_id"
    );
    assert_eq!(
        field(withdraw_signing_input(
            id,
            agent,
            &b64url_encode(&[7; 31]),
            ep
        )),
        "nonce"
    );
    assert_eq!(
        field(withdraw_signing_input(
            id,
            agent,
            &nonce,
            "kairos.example.gov.tw"
        )),
        "enroll_endpoint"
    );
    assert_eq!(
        field(grace_renew_signing_input(
            agent,
            "sha256:xyz",
            b"csr",
            "2026-10-11T02:13:05.000Z",
            ep
        )),
        "host_fingerprint"
    );
    assert_eq!(
        field(grace_renew_signing_input(
            agent,
            &fp,
            b"",
            "2026-10-11T02:13:05.000Z",
            ep
        )),
        "csr"
    );
    assert_eq!(
        field(grace_renew_signing_input(
            agent,
            &fp,
            b"csr",
            "2026-10-11T02:13:05.000Z\nx",
            ep
        )),
        "sent_at"
    );
}

#[test]
fn signature_rejects_non_p256_keys() {
    let pkcs8 = {
        let rng = ring::rand::SystemRandom::new();
        ring::signature::EcdsaKeyPair::generate_pkcs8(
            &ring::signature::ECDSA_P256_SHA256_ASN1_SIGNING,
            &rng,
        )
        .unwrap()
    };
    let spki = p256_spki_of_pkcs8(pkcs8.as_ref()).unwrap();
    let sig = sign_ecdsa_p256_sha256(pkcs8.as_ref(), b"msg").unwrap();
    verify_ecdsa_p256_sha256(&spki, b"msg", &sig).unwrap();
    assert_eq!(
        verify_ecdsa_p256_sha256(&spki, b"msg2", &sig),
        Err(SignatureError::Invalid)
    );
    assert_eq!(
        verify_ecdsa_p256_sha256(&spki[1..], b"msg", &sig),
        Err(SignatureError::UnsupportedKey)
    );
    assert_eq!(
        verify_ecdsa_p256_sha256(&spki, b"msg", "not base64!"),
        Err(SignatureError::Invalid)
    );
    assert_eq!(
        sign_ecdsa_p256_sha256(b"garbage", b"msg"),
        Err(SignatureError::BadPrivateKey)
    );
    let point = p256_point_from_spki(&spki).unwrap();
    assert_eq!(p256_spki_from_point(point).unwrap(), spki);
    assert_eq!(p256_spki_from_point(&point[1..]), None);
}

#[test]
fn routes() {
    let id = "0d5f7b2e-3c41-4a8e-9b6f-2e7c1d9a4b30";
    let cases = [
        (
            "GET",
            "/v1/enroll/server-info".to_string(),
            Route::ServerInfo,
        ),
        (
            "HEAD",
            "/v1/enroll/server-info".to_string(),
            Route::ServerInfo,
        ),
        (
            "POST",
            "/v1/enroll/token-check".to_string(),
            Route::TokenCheck,
        ),
        (
            "POST",
            "/v1/enroll/requests".to_string(),
            Route::CreateRequest,
        ),
        (
            "POST",
            "/v1/enroll/renewals".to_string(),
            Route::CreateRenewal,
        ),
        (
            "GET",
            format!("/v1/enroll/requests/{}", id.to_ascii_uppercase()),
            Route::Poll(id.into()),
        ),
        (
            "POST",
            format!("/v1/enroll/requests/{id}/withdraw/challenge"),
            Route::WithdrawChallenge(id.into()),
        ),
        (
            "POST",
            format!("/v1/enroll/requests/{id}/withdraw"),
            Route::Withdraw(id.into()),
        ),
        (
            "GET",
            "/agent/install.sh".to_string(),
            Route::Download("install.sh".into()),
        ),
    ];
    for (method, path, want) in cases {
        let got = parse_route(method, &path).unwrap();
        assert_eq!(got, want, "{method} {path}");
        assert_eq!(got.path().to_ascii_lowercase(), path.to_ascii_lowercase());
    }
    assert_eq!(
        parse_route("POST", "/v1/enroll/server-info"),
        Err(RouteError::MethodNotAllowed("GET, HEAD"))
    );
    assert_eq!(
        parse_route("GET", "/v1/enroll/requests"),
        Err(RouteError::MethodNotAllowed("POST"))
    );
    for path in [
        "/v1/enroll/server-info/",
        "/v1/enroll//requests",
        "/v1/enroll/requests/not-a-uuid",
        "/v1/enroll/requests/0d5f7b2e%2D3c41-4a8e-9b6f-2e7c1d9a4b30",
        "/v2/enroll/server-info",
        "/v1/enroll",
        "/",
        "/agent/../kairos.toml",
        "/agent/.hidden",
        "/agent/sub/dir",
        "/agent/",
    ] {
        assert_eq!(
            parse_route("GET", path),
            Err(RouteError::NotFound),
            "{path}"
        );
    }
}

#[test]
fn mode_whitelist_matches_design() {
    use EndpointClass::*;
    use ModeDecision::*;
    let classes = [
        ServerInfo,
        TokenCheck,
        NewEnrollment,
        GraceRenewal,
        Poll,
        Withdraw,
        Download,
    ];
    // enroll-pause.md §2.3：開放、開放（自動暫停中）、僅續簽
    let expected = [
        (
            EnrollMode::Open,
            false,
            [Accept, Accept, Accept, Accept, Accept, Accept, Accept],
        ),
        (
            EnrollMode::Open,
            true,
            [
                Accept,
                NotAccepted,
                NotAccepted,
                Accept,
                Accept,
                Accept,
                NotAccepted,
            ],
        ),
        (
            EnrollMode::RenewOnly,
            false,
            [
                Accept,
                NotAccepted,
                NotAccepted,
                Accept,
                Accept,
                Accept,
                NotAccepted,
            ],
        ),
        // 自動暫停只在開放時有作用
        (
            EnrollMode::RenewOnly,
            true,
            [
                Accept,
                NotAccepted,
                NotAccepted,
                Accept,
                Accept,
                Accept,
                NotAccepted,
            ],
        ),
    ];
    for (mode, paused, want) in expected {
        for (class, w) in classes.into_iter().zip(want) {
            assert_eq!(
                mode_decision(mode, paused, class),
                w,
                "{mode:?} {paused} {class:?}"
            );
        }
    }
    for class in classes {
        assert_eq!(
            mode_decision(EnrollMode::Closed, false, class),
            NotListening
        );
    }
    assert_eq!(ServerInfo.rate_class(), RateClass::NewEnrollment);
    assert_eq!(TokenCheck.rate_class(), RateClass::NewEnrollment);
    assert_eq!(GraceRenewal.rate_class(), RateClass::Renewal);
    assert_eq!(Withdraw.rate_class(), RateClass::PerRequest);
    assert_eq!(EnrollMode::parse("renew_only"), Some(EnrollMode::RenewOnly));
    assert_eq!(EnrollMode::parse("off"), None);
}

#[test]
fn wait_and_authorization() {
    assert_eq!(parse_wait_secs(None), Some(0));
    assert_eq!(parse_wait_secs(Some("")), Some(0));
    assert_eq!(parse_wait_secs(Some("wait=60")), Some(60));
    assert_eq!(parse_wait_secs(Some("x=1&wait=5")), Some(5));
    for bad in ["wait=61", "wait=", "wait=-1", "wait=1.5", "wait=100"] {
        assert_eq!(parse_wait_secs(Some(bad)), None, "{bad}");
    }
    let secret = b64url_encode(&[9; 32]);
    assert_eq!(
        parse_poll_authorization(&format!("Bearer {secret}")),
        Some(vec![9; 32])
    );
    assert_eq!(
        parse_poll_authorization(&format!("bearer {secret}")),
        Some(vec![9; 32])
    );
    assert_eq!(parse_poll_authorization(&format!("Basic {secret}")), None);
    assert_eq!(
        parse_poll_authorization(&format!("Bearer {}", b64url_encode(&[9; 16]))),
        None
    );
}

#[test]
fn enroll_request_field_errors() {
    let v = Vectors::load();
    let good = EnrollRequest {
        token: v.e("token").to_string(),
        agent_id: v.i("agent_id").to_ascii_uppercase(),
        csr: v.i("csr").to_string(),
        poll_secret: b64url_encode(&[1; 32]),
        sent_at: v.i("sent_at").to_string(),
        hostname: "plc-hmi-07".to_string(),
        pulse_version: "0.5.0".to_string(),
        server_verification: ServerVerification::Tofu,
        display_name: None,
        tags: None,
        host_fingerprint: None,
        os: None,
        previous_serial: Some("5E0B2C77".to_string()),
    };
    let checked = validate_enroll_request(&good).unwrap();
    assert_eq!(checked.agent_id, v.i("agent_id"));
    let field = |f: fn(&mut EnrollRequest)| {
        let mut r = good.clone();
        f(&mut r);
        validate_enroll_request(&r).unwrap_err().field
    };
    assert_eq!(field(|r| r.token = "KRT1-0000".into()), "token");
    assert_eq!(field(|r| r.agent_id = "web-01".into()), "agent_id");
    assert_eq!(field(|r| r.csr = String::new()), "csr");
    assert_eq!(field(|r| r.csr = "A".repeat(5464)), "csr");
    assert_eq!(
        field(|r| r.poll_secret = b64url_encode(&[1; 31])),
        "poll_secret"
    );
    assert_eq!(field(|r| r.sent_at = "yesterday".into()), "sent_at");
    assert_eq!(field(|r| r.hostname = "a\u{202e}b".into()), "hostname");
    assert_eq!(field(|r| r.pulse_version = String::new()), "pulse_version");
    assert_eq!(
        field(|r| r.host_fingerprint = Some("md5:00".into())),
        "host_fingerprint"
    );
    assert_eq!(
        field(|r| r.tags = Some(vec!["a".into(), "a".into()])),
        "tags"
    );
    assert_eq!(
        field(|r| r.previous_serial = Some("5e0b".into())),
        "previous_serial"
    );
    // 列舉值不認得時整則解析失敗
    let mut json = serde_json::to_value(&good).unwrap();
    json["server_verification"] = Value::String("insecure".into());
    assert!(serde_json::from_value::<EnrollRequest>(json).is_err());
}

#[test]
fn withdraw_request_must_match_path() {
    let v = Vectors::load();
    let req = WithdrawRequest {
        request_id: v.i("request_id").to_string(),
        agent_id: v.i("agent_id").to_string(),
        nonce: v.i("nonce").to_string(),
        signature: v.e("withdraw_signature").to_string(),
        signature_alg: SignatureAlg::EcdsaP256Sha256,
        reason: WithdrawReason::EnrollCancelled,
        initiated_by: InitiatedBy::Cli,
        hostname: "plc-hmi-07".to_string(),
        pulse_version: "0.5.0".to_string(),
        sent_at: v.i("sent_at").to_string(),
        host_fingerprint: None,
    };
    let path_id = v.i("request_id").to_ascii_uppercase();
    assert!(validate_withdraw_request(&path_id, &req).is_ok());
    assert_eq!(
        validate_withdraw_request("5b0e9c1a-8d27-4f63-a1e4-7c9d2b6f3e81", &req)
            .unwrap_err()
            .field,
        "request_id"
    );
    let json = serde_json::to_value(&req).unwrap();
    assert_eq!(json["signature_alg"], "ecdsa-p256-sha256");
    assert_eq!(json["reason"], "enroll_cancelled");
    assert_eq!(json["initiated_by"], "cli");
}

#[test]
fn install_string_errors() {
    let v = Vectors::load();
    let payload = |json: &str| format!("pulse-enroll:v1:{}", b64url_encode(json.as_bytes()));
    let t = v.e("token");
    let p = v.e("ca_pin");
    assert_eq!(
        InstallString::decode("pulse:v1:abc"),
        Err(InstallStringError::Scheme)
    );
    assert_eq!(
        InstallString::decode("pulse-enroll:v2:abc"),
        Err(InstallStringError::UnknownVersion("v2".into()))
    );
    assert_eq!(
        InstallString::decode("pulse-enroll:v1:!!"),
        Err(InstallStringError::Encoding)
    );
    assert_eq!(
        InstallString::decode(&payload(&format!(
            r#"{{"s":"k:8443","t":"{t}","p":"{p}","x":1}}"#
        ))),
        Err(InstallStringError::UnknownField("x".into()))
    );
    assert!(matches!(
        InstallString::decode(&payload(&format!(r#"{{"s":"k:8443","p":"{p}"}}"#))),
        Err(InstallStringError::Field("t", _))
    ));
    assert!(matches!(
        InstallString::decode(&payload(&format!(r#"{{"s":"k","t":"{t}","p":"{p}"}}"#))),
        Err(InstallStringError::Field("s", _))
    ));
    assert!(matches!(
        InstallString::decode(&payload(&format!(
            r#"{{"s":"k:8443","e":8444,"t":"{t}","p":"{p}"}}"#
        ))),
        Err(InstallStringError::Field("e", _))
    ));
    assert!(matches!(
        InstallString::decode(&payload(&format!(
            r#"{{"s":"k:8443","t":"KRT1-0000-0000-0000-0001","p":"{p}"}}"#
        ))),
        Err(InstallStringError::Field("t", _))
    ));
    assert!(matches!(
        InstallString::decode(&payload(&format!(
            r#"{{"s":"k:8443","t":"{t}","p":"{p}","g":["a","a"]}}"#
        ))),
        Err(InstallStringError::Field("g", _))
    ));
    // e、g 為 null 時視同省略
    let ok = InstallString::decode(&payload(&format!(
        r#"{{"s":"k:8443","e":null,"t":"{t}","p":"{p}","g":null}}"#
    )))
    .unwrap();
    assert_eq!((ok.e, ok.g), (None, None));
}

#[test]
fn error_body_helpers() {
    let body = ErrorBody::new(EnrollErrorCode::RateLimited, "slow down");
    assert_eq!(body.known_code(), Some(EnrollErrorCode::RateLimited));
    let json = serde_json::to_value(&body).unwrap();
    assert_eq!(
        json,
        serde_json::json!({"code":"rate_limited","message":"slow down"})
    );
    // 不認得的錯誤碼不讓整則解析失敗
    let unknown: ErrorResponse = serde_json::from_str(
        r#"{"server_time":"2026-10-11T02:13:05.000Z","error":{"code":"future_code","message":"x","extra":1}}"#,
    )
    .unwrap();
    assert_eq!(unknown.error.known_code(), None);
    assert!(EnrollErrorCode::RateLimited.is_retryable());
    assert!(!EnrollErrorCode::TokenInvalid.is_retryable());
    assert!(RequestState::Withdrawn.is_final());
    assert!(!RequestState::PendingApproval.is_final());
}

#[test]
#[ignore = "重新產生 ENROLL.md「測試向量」節的金鑰、CSR 與簽章（每次執行結果不同）"]
fn generate_vectors() {
    use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair, PKCS_ECDSA_P256_SHA256};
    let v = Vectors::load();
    let gen_key = || KeyPair::generate_for(&PKCS_ECDSA_P256_SHA256).unwrap();
    let (old, new, ca) = (gen_key(), gen_key(), gen_key());
    println!("old_key_pkcs8 = {}", b64url_encode(&old.serialize_der()));
    println!("new_key_pkcs8 = {}", b64url_encode(&new.serialize_der()));
    println!(
        "ca_spki = {}",
        b64url_encode(&p256_spki_of_pkcs8(&ca.serialize_der()).unwrap())
    );
    let mut params = CertificateParams::new(vec![v.i("agent_id").to_string()]).unwrap();
    params.distinguished_name = DistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::CommonName, v.i("agent_id"));
    let csr = params.serialize_request(&new).unwrap();
    println!("csr = {}", b64url_encode(csr.der()));
    println!("（其餘預期結果由測試失敗訊息取得，或以本 crate 的函式計算）");
}
