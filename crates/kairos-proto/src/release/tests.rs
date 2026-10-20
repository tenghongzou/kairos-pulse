//! 發佈包信任鏈的測試。簽章測試向量（`testdata/release`）是在建置機上以 OpenSSH 9.6 的
//! `ssh-keygen -Y sign` 產生、並以 `ssh-keygen -Y verify` 確認過的真實簽章（測試金鑰，私鑰未保存）：
//!
//! | 檔案 | 內容 |
//! |---|---|
//! | `SHA256SUMS` | 三個測試檔案（不是真的發佈包）的雜湊，格式同 `sha256sum` |
//! | `SHA256SUMS.sig` | 金鑰 A、namespace `kairos-pulse-release`（RELEASING.md 的參數） |
//! | `SHA256SUMS.wrong-namespace.sig` | 金鑰 A、namespace `file` |
//! | `SHA256SUMS.other-key.sig` | 金鑰 B、namespace `kairos-pulse-release` |
//! | `SHA256SUMS.sha256-hash.sig` | 金鑰 A、`-O hashalg=sha256` |
//! | `allowed_signers` | 以 RELEASING.md 第 3 節的 `printf` 產生，只有金鑰 A |
//! | `release-a.pub`、`other-b.pub`、`rsa.pub` | 公鑰（`rsa.pub` 用來測試非 Ed25519 的行） |
//! | `release-a.fingerprint` | `ssh-keygen -lf release-a.pub` 的輸出 |

use super::*;

const SUMS: &str = include_str!("../../testdata/release/SHA256SUMS");
const SIG: &str = include_str!("../../testdata/release/SHA256SUMS.sig");
const SIG_WRONG_NS: &str = include_str!("../../testdata/release/SHA256SUMS.wrong-namespace.sig");
const SIG_OTHER_KEY: &str = include_str!("../../testdata/release/SHA256SUMS.other-key.sig");
const SIG_SHA256: &str = include_str!("../../testdata/release/SHA256SUMS.sha256-hash.sig");
const SIGNERS: &str = include_str!("../../testdata/release/allowed_signers");
const PUB_A: &str = include_str!("../../testdata/release/release-a.pub");
const PUB_B: &str = include_str!("../../testdata/release/other-b.pub");
const PUB_RSA: &str = include_str!("../../testdata/release/rsa.pub");
const FINGERPRINT_A: &str = include_str!("../../testdata/release/release-a.fingerprint");
const MANIFEST_NAME: &str = "pulse-0.9.1-5e6f7a8-release.json";
const MANIFEST: &str = include_str!("../../testdata/release/pulse-0.9.1-5e6f7a8-release.json");
const TAR: &[u8] = include_bytes!("../../testdata/release/pulse-0.9.1-5e6f7a8-linux-x86_64.tar.gz");
const TAR_NAME: &str = "pulse-0.9.1-5e6f7a8-linux-x86_64.tar.gz";
const DEB_NAME: &str = "pulse-0.9.1-5e6f7a8-linux-x86_64.deb";

/// 2026-11-20 前後（測試向量沒有有效期限，這個值只影響 valid-after／valid-before 的測試）
const NOW: i64 = 1_795_000_000;

/// 公鑰檔的「金鑰類型 base64」兩欄
fn key_fields(pub_line: &str) -> String {
    pub_line
        .split_whitespace()
        .take(2)
        .collect::<Vec<_>>()
        .join(" ")
}

fn signers_line(principal: &str, options: &str, pub_line: &str) -> String {
    format!("{principal} {options} {}\n", key_fields(pub_line))
}

fn verify(signers: &str, sig: &str, message: &str) -> Result<VerifiedSigner, SigError> {
    verify_sshsig(
        &AllowedSigners::parse(signers),
        &[RELEASE_PRINCIPAL],
        RELEASE_NAMESPACE,
        message.as_bytes(),
        sig,
        NOW,
    )
}

// ---- SSHSIG 與 allowed_signers ----

#[test]
fn ssh_keygen_signature_verifies() {
    let signers = AllowedSigners::parse(SIGNERS);
    assert!(signers.rejected.is_empty(), "{:?}", signers.rejected);
    assert_eq!(signers.entries.len(), 1);
    let e = &signers.entries[0];
    assert_eq!(e.principals, [RELEASE_PRINCIPAL]);
    assert_eq!(
        e.namespaces.as_deref(),
        Some(&[RELEASE_NAMESPACE.to_string()][..])
    );
    // 指紋與 ssh-keygen -l 相同
    let expected = FINGERPRINT_A.split_whitespace().nth(1).unwrap();
    assert_eq!(e.fingerprint(), expected);
    assert!(signers.has_usable_entry(RELEASE_NAMESPACE));
    assert!(!signers.has_usable_entry("file"));

    let v = verify(SIGNERS, SIG, SUMS).unwrap();
    assert_eq!(v.principal, RELEASE_PRINCIPAL);
    assert_eq!(v.fingerprint, expected);
    assert_eq!(v.line, 1);
    let sig = SshSignature::parse(SIG).unwrap();
    assert_eq!(sig.namespace, RELEASE_NAMESPACE);
    assert_eq!(sig.hash_algorithm, "sha512");
    assert_eq!(sig.fingerprint(), expected);
    // CRLF 的 armor（Windows 上以文字模式複製的 .sig）也能解析
    assert!(verify(SIGNERS, &SIG.replace('\n', "\r\n"), SUMS).is_ok());
}

#[test]
fn ssh_keygen_negative_vectors() {
    assert_eq!(
        verify(SIGNERS, SIG_WRONG_NS, SUMS),
        Err(SigError::WrongNamespace {
            expected: RELEASE_NAMESPACE.into(),
            found: "file".into()
        })
    );
    assert_eq!(
        verify(SIGNERS, SIG_OTHER_KEY, SUMS),
        Err(SigError::UnknownSigner)
    );
    assert_eq!(
        verify(SIGNERS, SIG_SHA256, SUMS),
        Err(SigError::UnsupportedHashAlgorithm("sha256".into()))
    );
    // 被修改的 SHA256SUMS（改一個字元、多一行、CRLF）
    let tampered = SUMS.replacen("a13150fd", "a13150fe", 1);
    assert_eq!(verify(SIGNERS, SIG, &tampered), Err(SigError::BadSignature));
    let extra = format!("{SUMS}{}  evil.sh\n", "0".repeat(64));
    assert_eq!(verify(SIGNERS, SIG, &extra), Err(SigError::BadSignature));
    assert_eq!(
        verify(SIGNERS, SIG, &SUMS.replace('\n', "\r\n")),
        Err(SigError::BadSignature)
    );
    // 金鑰 B 列在 allowed_signers 中時，B 的簽章就有效（信任完全由本機檔案決定）
    let both = format!(
        "{SIGNERS}{}",
        signers_line(
            RELEASE_PRINCIPAL,
            r#"namespaces="kairos-pulse-release""#,
            PUB_B
        )
    );
    assert_eq!(verify(&both, SIG_OTHER_KEY, SUMS).unwrap().line, 2);
    // 空的信任錨
    assert_eq!(verify("", SIG, SUMS), Err(SigError::UnknownSigner));
}

#[test]
fn principal_namespace_and_validity() {
    let ns = r#"namespaces="kairos-pulse-release""#;
    // principal 不是接受的名稱
    let other = signers_line("ops@customer", ns, PUB_A);
    assert_eq!(verify(&other, SIG, SUMS), Err(SigError::UnknownSigner));
    // 客戶自行重新簽署（D14）：extra_principals 列出時接受
    let v = verify_sshsig(
        &AllowedSigners::parse(&other),
        &[RELEASE_PRINCIPAL, "ops@customer"],
        RELEASE_NAMESPACE,
        SUMS.as_bytes(),
        SIG,
        NOW,
    )
    .unwrap();
    assert_eq!(v.principal, "ops@customer");
    // 多個 principal 的行
    let multi = signers_line(&format!("ops@customer,{RELEASE_PRINCIPAL}"), ns, PUB_A);
    assert!(verify(&multi, SIG, SUMS).is_ok());
    // 沒有 namespaces=、或沒有列出 kairos-pulse-release
    let no_ns = format!("{RELEASE_PRINCIPAL} {}\n", key_fields(PUB_A));
    let parsed = AllowedSigners::parse(&no_ns);
    assert!(parsed.rejected.is_empty() && !parsed.has_usable_entry(RELEASE_NAMESPACE));
    assert_eq!(
        verify(&no_ns, SIG, SUMS),
        Err(SigError::NamespaceNotAllowed)
    );
    let file_ns = signers_line(RELEASE_PRINCIPAL, r#"namespaces="file,git""#, PUB_A);
    assert_eq!(
        verify(&file_ns, SIG, SUMS),
        Err(SigError::NamespaceNotAllowed)
    );
    let two_ns = signers_line(
        RELEASE_PRINCIPAL,
        r#"namespaces="git,kairos-pulse-release""#,
        PUB_A,
    );
    assert!(verify(&two_ns, SIG, SUMS).is_ok());
    // valid-after／valid-before（UTC）
    let at = |opts: &str| signers_line(RELEASE_PRINCIPAL, &format!("{ns},{opts}"), PUB_A);
    assert!(verify(&at(r#"valid-before="20991231Z""#), SIG, SUMS).is_ok());
    assert_eq!(
        verify(&at(r#"valid-before="20260101Z""#), SIG, SUMS),
        Err(SigError::Expired)
    );
    assert_eq!(
        verify(&at(r#"valid-after="20991231Z""#), SIG, SUMS),
        Err(SigError::NotYetValid)
    );
    assert!(verify(&at(r#"valid-after="20260101000000Z""#), SIG, SUMS).is_ok());
    // 舊金鑰過期、新金鑰有效的輪替期間：任一行有效即可
    let rotation = format!(
        "{}{}",
        at(r#"valid-before="20260101Z""#),
        at(r#"valid-after="20260101Z""#)
    );
    assert_eq!(verify(&rotation, SIG, SUMS).unwrap().line, 2);
}

#[test]
fn local_time_validity_is_interpreted_conservatively() {
    let line = |opt: &str| {
        signers_line(
            RELEASE_PRINCIPAL,
            &format!(r#"namespaces="kairos-pulse-release",{opt}"#),
            PUB_A,
        )
    };
    let utc = |s: &str| {
        chrono::NaiveDateTime::parse_from_str(s, "%Y%m%d%H%M%S")
            .unwrap()
            .and_utc()
            .timestamp()
    };
    let parsed = AllowedSigners::parse(&line(r#"valid-after="20261120000000""#));
    assert_eq!(
        parsed.entries[0].valid_after,
        Some(utc("20261120000000") + 12 * 3600)
    );
    let parsed = AllowedSigners::parse(&line(r#"valid-before="202611200000""#));
    assert_eq!(
        parsed.entries[0].valid_before,
        Some(utc("20261120000000") - 14 * 3600)
    );
    let parsed = AllowedSigners::parse(&line(r#"valid-before="20261120Z""#));
    assert_eq!(parsed.entries[0].valid_before, Some(utc("20261120000000")));
}

#[test]
fn unsupported_lines_are_rejected_fail_closed() {
    let ns = r#"namespaces="kairos-pulse-release""#;
    let text = [
        "# 註解與空行略過\n".to_string(),
        "\n".to_string(),
        signers_line(RELEASE_PRINCIPAL, &format!("cert-authority,{ns}"), PUB_A),
        signers_line(RELEASE_PRINCIPAL, ns, PUB_RSA),
        signers_line(
            RELEASE_PRINCIPAL,
            &format!(r#"{ns},verify-required"#),
            PUB_A,
        ),
        signers_line("*@kairos-pulse", ns, PUB_A),
        signers_line(RELEASE_PRINCIPAL, r#"namespaces="*""#, PUB_A),
        signers_line(RELEASE_PRINCIPAL, "namespaces=kairos-pulse-release", PUB_A),
        signers_line(
            RELEASE_PRINCIPAL,
            &format!(r#"{ns},valid-after="2026""#),
            PUB_A,
        ),
        signers_line(RELEASE_PRINCIPAL, &format!(r#"{ns},{ns}"#), PUB_A),
        format!("{RELEASE_PRINCIPAL} {ns} ssh-ed25519 AAAA\n"),
        format!("{RELEASE_PRINCIPAL} {ns} ssh-ed25519\n"),
        format!(
            "{RELEASE_PRINCIPAL} {ns} ssh-ed25519 {}\n",
            key_fields(PUB_RSA).split(' ').nth(1).unwrap()
        ),
    ]
    .concat();
    let parsed = AllowedSigners::parse(&text);
    assert!(parsed.entries.is_empty(), "{:?}", parsed.entries);
    assert_eq!(parsed.rejected.len(), 11, "{:?}", parsed.rejected);
    let reasons: Vec<&str> = parsed.rejected.iter().map(|r| r.reason.as_str()).collect();
    assert!(reasons[0].contains("cert-authority"), "{reasons:?}");
    assert!(reasons[1].contains("ssh-rsa"), "{reasons:?}");
    assert!(reasons[2].contains("verify-required"), "{reasons:?}");
    assert!(reasons[3].contains("principal"), "{reasons:?}");
    assert!(reasons[4].contains("namespaces"), "{reasons:?}");
    assert!(reasons[5].contains("quotes"), "{reasons:?}");
    assert!(reasons[6].contains("time"), "{reasons:?}");
    assert!(reasons[7].contains("duplicate"), "{reasons:?}");
    // 被拒絕的行不影響其他行
    let mixed = format!("{text}{SIGNERS}");
    assert_eq!(verify(&mixed, SIG, SUMS).unwrap().line, 14);
}

#[test]
fn malformed_signatures() {
    let body: Vec<&str> = SIG.lines().collect();
    for bad in [
        String::new(),
        "not a signature".to_string(),
        SIG.replace("BEGIN SSH SIGNATURE", "BEGIN PGP SIGNATURE"),
        SIG.replace("-----END SSH SIGNATURE-----", ""),
        format!("{SIG}trailing"),
        SIG.replacen("U1NIU0lH", "U1NIU0lI", 1),
        // 截斷的 base64（少一行）
        [body[0], body[1], body[2], body[5]].join("\n"),
    ] {
        assert!(
            matches!(SshSignature::parse(&bad), Err(SigError::Malformed(_))),
            "{bad:?}"
        );
    }
    // 版本不是 1
    let mut blob = crate::b64::decode(&body[1..body.len() - 1].concat()).unwrap();
    blob[9] = 2;
    let armored = format!(
        "-----BEGIN SSH SIGNATURE-----\n{}\n-----END SSH SIGNATURE-----\n",
        crate::b64::encode(&blob)
    );
    assert_eq!(
        SshSignature::parse(&armored),
        Err(SigError::UnsupportedVersion(2))
    );
}

// ---- SHA256SUMS ----

#[test]
fn sha256sums_parse_and_check() {
    let sums = Sha256Sums::parse(SUMS).unwrap();
    assert_eq!(sums.entries().count(), 3);
    assert_eq!(sums.get(TAR_NAME), Some(sha256_hex(TAR).as_str()));
    assert!(sums.check(TAR_NAME, &sha256_hex(TAR)).is_ok());
    assert!(
        sums.check(TAR_NAME, &"0".repeat(64))
            .unwrap_err()
            .contains("does not match")
    );
    assert!(
        sums.check("pulse-install.sh", &"0".repeat(64))
            .unwrap_err()
            .contains("not listed")
    );
    assert_eq!(
        sums.get(MANIFEST_NAME),
        Some(sha256_hex(MANIFEST.as_bytes()).as_str())
    );
    // 最後一行沒有換行也可以
    assert!(Sha256Sums::parse(SUMS.trim_end()).is_ok());
    let h = "a".repeat(64);
    for bad in [
        String::new(),
        "\n".to_string(),
        format!("{h} file\n"),
        format!("{h}  file\r\n"),
        format!("{}  file\n", "A".repeat(64)),
        format!("{h}  dir/file\n"),
        format!("{h}  *file\n"),
        format!("{h}  file\n{h}  file\n"),
        format!("{h}  file\n\n{h}  other\n"),
        format!("{}  file\n", "a".repeat(63)),
    ] {
        assert!(Sha256Sums::parse(&bad).is_err(), "{bad:?}");
    }
}

// ---- 檔名 ----

#[test]
fn release_file_names() {
    let f = ReleaseFile::parse(TAR_NAME).unwrap();
    assert_eq!(
        (f.product, f.os, f.arch, f.format, f.dirty),
        (
            Product::Pulse,
            Os::Linux,
            Arch::X86_64,
            Format::TarGz,
            false
        )
    );
    assert_eq!(
        (f.version.to_string(), f.commit.as_str()),
        ("0.9.1".into(), "5e6f7a8")
    );
    assert_eq!(f.file_name(), TAR_NAME);
    for name in [
        "pulse-0.9.0-test.1-1a2b3c4-linux-x86_64.tar.gz",
        "pulse-0.9.0-1a2b3c4-dirty-linux-aarch64.rpm",
        "pulse-0.9.0-1a2b3c4-windows-x86_64.msi",
        "pulse-0.9.0-1a2b3c4-windows-x86_64.zip",
        "kairos-0.9.0-1a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8f9a0b-linux-aarch64.cdx.json",
        DEB_NAME,
    ] {
        let f = ReleaseFile::parse(name).unwrap();
        assert_eq!(f.file_name(), name);
    }
    let f = ReleaseFile::parse("pulse-0.9.0-test.1-1a2b3c4-linux-x86_64.tar.gz").unwrap();
    assert_eq!(f.version.pre, ["test", "1"]);
    assert!(
        ReleaseFile::parse("pulse-0.9.0-1a2b3c4-dirty-linux-aarch64.rpm")
            .unwrap()
            .dirty
    );
    for bad in [
        "pulse-install.sh",
        "SHA256SUMS",
        "pulse-0.9.0-1a2b3c4-linux-x86_64.exe",
        "pulse-0.9.0-1a2b3c4-macos-x86_64.tar.gz",
        "pulse-0.9.0-1a2b3c4-linux-riscv64.tar.gz",
        "pulse-0.9-1a2b3c4-linux-x86_64.tar.gz",
        "pulse-0.9.0-XYZ-linux-x86_64.tar.gz",
        "pulse-0.9.0-1a2b3c-linux-x86_64.tar.gz",
        "agent-0.9.0-1a2b3c4-linux-x86_64.tar.gz",
        "../pulse-0.9.0-1a2b3c4-linux-x86_64.tar.gz",
        "dir\\pulse-0.9.0-1a2b3c4-linux-x86_64.tar.gz",
        "pulse-0.9.0-1a2b3c4-linux-x86_64.tar.gz.sig",
    ] {
        assert!(ReleaseFile::parse(bad).is_err(), "{bad}");
    }
    let (p, v, c) = ReleaseFile::parse_manifest_name(MANIFEST_NAME).unwrap();
    assert_eq!(
        (p, v.to_string(), c.as_str()),
        (Product::Pulse, "0.9.1".into(), "5e6f7a8")
    );
    assert_eq!(ReleaseFile::manifest_name(p, &v, &c), MANIFEST_NAME);
    for bad in [
        "pulse-0.9.1-5e6f7a8.json",
        "pulse-0.9.1-5e6f7a8-dirty-release.json",
        "x/pulse-0.9.1-5e6f7a8-release.json",
    ] {
        assert!(ReleaseFile::parse_manifest_name(bad).is_err(), "{bad}");
    }
}

#[test]
fn artifact_kind_per_install_method() {
    assert_eq!(
        artifact_kind("tar", "aarch64"),
        Some((Os::Linux, Arch::Aarch64, Format::TarGz))
    );
    assert_eq!(
        artifact_kind("deb", "x86_64"),
        Some((Os::Linux, Arch::X86_64, Format::Deb))
    );
    assert_eq!(
        artifact_kind("rpm", "x86_64"),
        Some((Os::Linux, Arch::X86_64, Format::Rpm))
    );
    assert_eq!(
        artifact_kind("msi", "x86_64"),
        Some((Os::Windows, Arch::X86_64, Format::Msi))
    );
    assert_eq!(
        artifact_kind("zip", "x86_64"),
        Some((Os::Windows, Arch::X86_64, Format::Zip))
    );
    assert_eq!(artifact_kind("msi", "aarch64"), None);
    assert_eq!(artifact_kind("unknown", "x86_64"), None);
    assert_eq!(artifact_kind("tar", "armv7"), None);
}

// ---- 版本 ----

#[test]
fn semver_parsing_and_precedence() {
    // SemVer 2.0 第 11 節的例子，由小到大
    let ordered = [
        "1.0.0-alpha",
        "1.0.0-alpha.1",
        "1.0.0-alpha.beta",
        "1.0.0-beta",
        "1.0.0-beta.2",
        "1.0.0-beta.11",
        "1.0.0-rc.1",
        "1.0.0",
        "1.0.1",
        "1.1.0",
        "2.0.0",
        "10.0.0",
    ];
    let parsed: Vec<Version> = ordered.iter().map(|s| Version::parse(s).unwrap()).collect();
    for w in parsed.windows(2) {
        assert!(w[0] < w[1], "{} < {}", w[0], w[1]);
    }
    for (s, v) in ordered.iter().zip(&parsed) {
        assert_eq!(v.to_string(), *s);
    }
    assert!(Version::parse("0.9.0-test.1").unwrap() < Version::parse("0.9.0").unwrap());
    assert!(Version::parse("0.9.0-test.2").unwrap() > Version::parse("0.9.0-test.1").unwrap());
    // build metadata 不影響比較
    assert_eq!(
        Version::parse("1.0.0+build.5").unwrap(),
        Version::parse("1.0.0").unwrap()
    );
    assert_eq!(
        Version::parse("1.0.0+build.5").unwrap().to_string(),
        "1.0.0+build.5"
    );
    assert_eq!(Version::parse("0.9.0-test.1").unwrap().core(), (0, 9, 0));
    for bad in [
        "",
        "1",
        "1.0",
        "1.0.0.0",
        "01.0.0",
        "1.0.0-",
        "1.0.0-01",
        "1.0.0-a..b",
        "1.0.0+",
        "1.0.0-a_b",
        "v1.0.0",
        " 1.0.0",
        "1.0.0 ",
        &format!("1.0.0-{}", "a".repeat(60)),
        "99999999999999999999.0.0",
    ] {
        assert!(Version::parse(bad).is_err(), "{bad:?}");
    }
}

// ---- 清單與整體流程 ----

fn trust(signers: &AllowedSigners) -> TrustPolicy<'_> {
    TrustPolicy {
        signers,
        extra_principals: &[],
        now_unix: NOW,
    }
}

#[test]
fn manifest_parsing() {
    let m = ReleaseManifest::parse(MANIFEST).unwrap();
    assert_eq!(m.file_name(), MANIFEST_NAME);
    assert_eq!(m.product(), Product::Pulse);
    assert!(m.supports_remote_upgrade());
    assert!(!m.breaking);
    assert!(m.kairos_compatible(&Version::parse("0.9.0").unwrap()));
    assert!(m.kairos_compatible(&Version::parse("0.5.0").unwrap()));
    assert!(!m.kairos_compatible(&Version::parse("0.4.9").unwrap()));
    // 必要欄位與格式
    let v: serde_json::Value = serde_json::from_str(MANIFEST).unwrap();
    for (field, value) in [
        ("schema", serde_json::json!("kairos-pulse.release/v2")),
        ("product", serde_json::json!("agent")),
        ("version", serde_json::json!("0.9")),
        ("commit", serde_json::json!("XYZ")),
        ("released_at", serde_json::json!("2026-11-31")),
        ("min_kairos", serde_json::json!("latest")),
        ("min_upgrade_from", serde_json::json!(5)),
        ("remote_features", serde_json::json!(["Config"])),
        ("unit", serde_json::json!({"sha256": "abc"})),
    ] {
        let mut bad = v.clone();
        bad[field] = value;
        assert!(ReleaseManifest::parse(&bad.to_string()).is_err(), "{field}");
    }
    let mut missing = v.clone();
    missing.as_object_mut().unwrap().remove("min_upgrade_from");
    assert!(ReleaseManifest::parse(&missing.to_string()).is_err());
    // 選用欄位
    let mut minimal = v;
    let o = minimal.as_object_mut().unwrap();
    o.remove("unit");
    o.remove("breaking");
    o.remove("remote_features");
    let m = ReleaseManifest::parse(&minimal.to_string()).unwrap();
    assert!(!m.supports_remote_upgrade() && m.unit.is_none());
    assert!(ReleaseManifest::parse(&" ".repeat(9000)).is_err());
}

#[test]
fn bundle_verification_end_to_end() {
    let signers = AllowedSigners::parse(SIGNERS);
    let bundle = ReleaseBundle {
        sha256sums: SUMS,
        signature: SIG,
        manifest: Some((MANIFEST_NAME, MANIFEST)),
    };
    let ok = verify_bundle(&bundle, &trust(&signers)).unwrap();
    assert_eq!(ok.manifest.as_ref().unwrap().version, "0.9.1");
    let expect = ArtifactExpectation {
        product: Some(Product::Pulse),
        version: Some("0.9.1"),
        commit: Some("5e6f7a8"),
        os: Some(Os::Linux),
        arch: Some(Arch::X86_64),
        format: Some(Format::TarGz),
    };
    assert!(
        ok.check_artifact(TAR_NAME, &sha256_hex(TAR), &expect)
            .is_ok()
    );
    // 雜湊、格式、版本、commit、平台不符：8402
    let deb_hash = ok.sums.get(DEB_NAME).unwrap().to_string();
    for (name, hash, e) in [
        (TAR_NAME, "0".repeat(64), expect),
        (DEB_NAME, deb_hash.clone(), expect),
        (
            TAR_NAME,
            sha256_hex(TAR),
            ArtifactExpectation {
                version: Some("0.9.2"),
                ..expect
            },
        ),
        (
            TAR_NAME,
            sha256_hex(TAR),
            ArtifactExpectation {
                version: Some("0.9.1+x"),
                ..expect
            },
        ),
        (
            TAR_NAME,
            sha256_hex(TAR),
            ArtifactExpectation {
                commit: Some("5e6f7a9"),
                ..expect
            },
        ),
        (
            TAR_NAME,
            sha256_hex(TAR),
            ArtifactExpectation {
                arch: Some(Arch::Aarch64),
                ..expect
            },
        ),
        (
            TAR_NAME,
            sha256_hex(TAR),
            ArtifactExpectation {
                os: Some(Os::Windows),
                ..expect
            },
        ),
        (
            TAR_NAME,
            sha256_hex(TAR),
            ArtifactExpectation {
                product: Some(Product::Kairos),
                ..expect
            },
        ),
        (
            MANIFEST_NAME,
            sha256_hex(MANIFEST.as_bytes()),
            ArtifactExpectation::default(),
        ),
    ] {
        let err = ok.check_artifact(name, &hash, &e).unwrap_err();
        assert_eq!(err.code_number(), 8402, "{name}: {err}");
    }
    assert!(
        ok.check_artifact(
            DEB_NAME,
            &deb_hash,
            &ArtifactExpectation {
                format: Some(Format::Deb),
                ..expect
            }
        )
        .is_ok()
    );

    // 簽章失敗：8401
    for sig in [SIG_WRONG_NS, SIG_OTHER_KEY, SIG_SHA256] {
        let err = verify_bundle(
            &ReleaseBundle {
                signature: sig,
                ..bundle
            },
            &trust(&signers),
        )
        .unwrap_err();
        assert_eq!(err.code_number(), 8401, "{err}");
    }
    // 清單被修改（不在 SHA256SUMS 中的內容）、檔名與內容不符：8402
    let tampered = MANIFEST.replace("\"breaking\":false", "\"breaking\":true ");
    for manifest in [
        Some((MANIFEST_NAME, tampered.as_str())),
        Some(("pulse-0.9.2-5e6f7a8-release.json", MANIFEST)),
        Some((TAR_NAME, MANIFEST)),
    ] {
        let err =
            verify_bundle(&ReleaseBundle { manifest, ..bundle }, &trust(&signers)).unwrap_err();
        assert_eq!(err.code_number(), 8402, "{err}");
    }
    // 沒有清單（0.9.0 以前的發佈）
    let no_manifest = verify_bundle(
        &ReleaseBundle {
            manifest: None,
            ..bundle
        },
        &trust(&signers),
    )
    .unwrap();
    assert!(no_manifest.manifest.is_none());
    // 客戶重新簽署：principal 不是官方名稱，extra_principals 列出時才接受
    let customer = signers_line(
        "ops@customer",
        r#"namespaces="kairos-pulse-release""#,
        PUB_A,
    );
    let customer = AllowedSigners::parse(&customer);
    assert_eq!(
        verify_bundle(&bundle, &trust(&customer))
            .unwrap_err()
            .code_number(),
        8401
    );
    let extra = ["ops@customer".to_string()];
    let ok = verify_bundle(
        &bundle,
        &TrustPolicy {
            signers: &customer,
            extra_principals: &extra,
            now_unix: NOW,
        },
    )
    .unwrap();
    assert_eq!(ok.signer.principal, "ops@customer");
}

#[test]
fn version_policy() {
    let v = |s: &str| Version::parse(s).unwrap();
    let m = ReleaseManifest::parse(MANIFEST).unwrap();
    use PolicyDecline::*;
    use VersionAction::*;
    // 升級：需要清單與 min_upgrade_from
    assert_eq!(
        check_version_policy(&v("0.9.0"), &v("0.9.1"), Some(&m), false, false),
        Ok(Upgrade)
    );
    assert_eq!(
        check_version_policy(&v("0.9.0-test.1"), &v("0.9.1"), Some(&m), false, false),
        Err(BelowMinUpgradeFrom {
            min_upgrade_from: "0.9.0".into()
        })
    );
    assert_eq!(
        check_version_policy(&v("0.8.0"), &v("0.9.1"), Some(&m), false, false)
            .unwrap_err()
            .detail(),
        "min_upgrade_from"
    );
    assert_eq!(
        check_version_policy(&v("0.9.0"), &v("0.9.1"), None, false, false),
        Err(MissingManifest)
    );
    // 相同版本：重新安裝
    assert_eq!(
        check_version_policy(&v("0.9.1"), &v("0.9.1"), None, false, false),
        Ok(Reinstall)
    );
    // 降版：推送與本機都允許才接受；min_upgrade_from 不適用
    for (offer, local) in [(false, false), (true, false), (false, true)] {
        assert_eq!(
            check_version_policy(&v("0.9.2"), &v("0.9.1"), Some(&m), offer, local),
            Err(DowngradeNotAllowed)
        );
    }
    assert_eq!(
        check_version_policy(&v("0.9.2"), &v("0.8.0"), None, true, true),
        Ok(Downgrade)
    );
    assert_eq!(DowngradeNotAllowed.detail(), "downgrade_not_allowed");
}

#[test]
fn pulse_release_meta_produces_a_valid_manifest() {
    // packaging/release-manifest.sh 讀取的手動欄位（每行「鍵 = 值」）：組成清單後必須通過 ReleaseManifest 的檢查
    const META: &str = include_str!("../../../../pulse/packaging/release-meta.toml");
    let value = |key: &str| -> String {
        META.lines()
            .map(str::trim)
            .filter(|l| !l.starts_with('#'))
            .find_map(|l| {
                let (k, v) = l.split_once('=')?;
                (k.trim() == key).then(|| v.split('#').next().unwrap().trim().to_string())
            })
            .unwrap_or_else(|| panic!("release-meta.toml 缺少 {key}"))
    };
    let manifest = format!(
        r#"{{"schema":"{MANIFEST_SCHEMA}","product":"pulse","version":"0.9.0","commit":"1a2b3c4","released_at":"2026-11-18","min_kairos":{},"min_upgrade_from":{},"remote_features":{},"unit":{{"sha256":"{}","previous_sha256":[]}},"breaking":{}}}"#,
        value("min_kairos"),
        value("min_upgrade_from"),
        value("remote_features").replace(' ', ""),
        "0".repeat(64),
        value("breaking"),
    );
    let m = ReleaseManifest::parse(&manifest).unwrap_or_else(|e| panic!("{e}: {manifest}"));
    assert!(Version::parse(&m.min_upgrade_from).unwrap() >= Version::parse("0.9.0").unwrap());
}
