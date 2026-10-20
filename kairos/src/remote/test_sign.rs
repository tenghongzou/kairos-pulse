//! 測試用的發佈簽章：以 ring 的 Ed25519 產生與 `ssh-keygen -Y sign -n kairos-pulse-release` 相同格式的
//! SSHSIG（`SHA256SUMS.sig`），以及對應的 `allowed_signers`。只在測試中使用；正式發佈的私鑰離線保存
//! （RELEASING.md 第 3 節），Kairos 從不簽章。

use ring::digest;
use ring::signature::{Ed25519KeyPair, KeyPair};

/// 標準 base64（含補位）
pub fn b64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (u32::from(c[0]) << 16)
            | (u32::from(*c.get(1).unwrap_or(&0)) << 8)
            | u32::from(*c.get(2).unwrap_or(&0));
        for i in 0..4 {
            if i <= c.len() {
                out.push(T[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn put_string(out: &mut Vec<u8>, data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(data);
}

fn key_blob(public: &[u8]) -> Vec<u8> {
    let mut b = Vec::new();
    put_string(&mut b, b"ssh-ed25519");
    put_string(&mut b, public);
    b
}

/// 測試用的發佈金鑰（以固定的種子產生）
pub struct TestSigner {
    key: Ed25519KeyPair,
}

impl TestSigner {
    pub fn new(seed: u8) -> Self {
        let key = Ed25519KeyPair::from_seed_unchecked(&[seed; 32]).expect("ed25519 seed");
        Self { key }
    }

    /// `allowed_signers` 的一行（principal `release@kairos-pulse`、namespace `kairos-pulse-release`）
    pub fn allowed_signers(&self) -> String {
        format!(
            "release@kairos-pulse namespaces=\"kairos-pulse-release\" ssh-ed25519 {} test\n",
            b64(&key_blob(self.key.public_key().as_ref()))
        )
    }

    /// `message` 的 SSHSIG（armor 格式）
    pub fn sign(&self, message: &[u8]) -> String {
        let namespace = b"kairos-pulse-release";
        let mut signed = Vec::new();
        signed.extend_from_slice(b"SSHSIG");
        put_string(&mut signed, namespace);
        put_string(&mut signed, b"");
        put_string(&mut signed, b"sha512");
        put_string(
            &mut signed,
            digest::digest(&digest::SHA512, message).as_ref(),
        );
        let sig = self.key.sign(&signed);
        let mut sig_blob = Vec::new();
        put_string(&mut sig_blob, b"ssh-ed25519");
        put_string(&mut sig_blob, sig.as_ref());
        let mut blob = Vec::new();
        blob.extend_from_slice(b"SSHSIG");
        blob.extend_from_slice(&1u32.to_be_bytes());
        put_string(&mut blob, &key_blob(self.key.public_key().as_ref()));
        put_string(&mut blob, namespace);
        put_string(&mut blob, b"");
        put_string(&mut blob, b"sha512");
        put_string(&mut blob, &sig_blob);
        let b64 = b64(&blob);
        let mut out = String::from("-----BEGIN SSH SIGNATURE-----\n");
        for chunk in b64.as_bytes().chunks(70) {
            out.push_str(std::str::from_utf8(chunk).unwrap_or_default());
            out.push('\n');
        }
        out.push_str("-----END SSH SIGNATURE-----\n");
        out
    }
}

/// 一次測試發佈：發佈檔的內容、`SHA256SUMS`、簽章與清單
pub struct TestRelease {
    pub version: String,
    pub files: Vec<(String, Vec<u8>)>,
    pub manifest: String,
    pub sha256sums: String,
    pub signature: String,
}

/// 產生一次測試發佈（tar.gz 與 deb，x86_64），`min_upgrade_from` 為 0.9.0
pub fn release(signer: &TestSigner, version: &str, commit: &str) -> TestRelease {
    let manifest = format!(
        "{{\"schema\":\"kairos-pulse.release/v1\",\"product\":\"pulse\",\"version\":\"{version}\",\"commit\":\"{commit}\",\"released_at\":\"2026-11-18\",\"min_kairos\":\"0.5.0\",\"min_upgrade_from\":\"0.9.0\",\"remote_features\":[\"config\",\"upgrade\"],\"breaking\":false}}\n"
    );
    let files: Vec<(String, Vec<u8>)> = [
        "linux-x86_64.tar.gz",
        "linux-x86_64.deb",
        "linux-x86_64.rpm",
    ]
    .iter()
    .map(|suffix| {
        let name = format!("pulse-{version}-{commit}-{suffix}");
        // 內容大於一段（32 KiB），測試分段下載
        let mut data = format!("{name}\n").into_bytes();
        data.resize(40_000, b'x');
        (name, data)
    })
    .collect();
    let mut sums = String::new();
    for (name, data) in &files {
        sums.push_str(&format!(
            "{}  {name}\n",
            kairos_proto::release::sha256_hex(data)
        ));
    }
    sums.push_str(&format!(
        "{}  pulse-{version}-{commit}-release.json\n",
        kairos_proto::release::sha256_hex(manifest.as_bytes())
    ));
    let signature = signer.sign(sums.as_bytes());
    TestRelease {
        version: version.to_string(),
        files,
        manifest,
        sha256sums: sums,
        signature,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kairos_proto::release::{AllowedSigners, ReleaseBundle, TrustPolicy, verify_bundle};

    #[test]
    fn test_signatures_verify_with_the_shared_implementation() {
        let s = TestSigner::new(7);
        let r = release(&s, "0.9.2", "abcdef1");
        let signers = AllowedSigners::parse(&s.allowed_signers());
        let v = verify_bundle(
            &ReleaseBundle {
                sha256sums: &r.sha256sums,
                signature: &r.signature,
                manifest: Some(("pulse-0.9.2-abcdef1-release.json", &r.manifest)),
            },
            &TrustPolicy {
                signers: &signers,
                extra_principals: &[],
                now_unix: 1_800_000_000,
            },
        )
        .unwrap();
        assert_eq!(v.signer.principal, "release@kairos-pulse");
        // 另一把金鑰簽的不通過
        let other = TestSigner::new(8);
        let bad = other.sign(r.sha256sums.as_bytes());
        assert!(
            verify_bundle(
                &ReleaseBundle {
                    sha256sums: &r.sha256sums,
                    signature: &bad,
                    manifest: None,
                },
                &TrustPolicy {
                    signers: &signers,
                    extra_principals: &[],
                    now_unix: 1_800_000_000,
                },
            )
            .is_err()
        );
    }
}
