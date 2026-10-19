//! 測試用：以臨時的 Ed25519 金鑰產生 `ssh-keygen -Y sign -n kairos-pulse-release` 格式的簽章與
//! `allowed_signers`、`SHA256SUMS`、發佈清單（不呼叫系統的 `ssh-keygen`；驗證端是 `kairos-proto` 的實作）。

use ring::signature::{Ed25519KeyPair, KeyPair};

use super::files;

fn put_string(out: &mut Vec<u8>, data: &[u8]) {
    out.extend_from_slice(&u32::try_from(data.len()).unwrap().to_be_bytes());
    out.extend_from_slice(data);
}

/// 測試用的發佈金鑰
pub struct TestKey {
    kp: Ed25519KeyPair,
}

impl TestKey {
    pub fn generate() -> Self {
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        Self {
            kp: Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap(),
        }
    }

    fn blob(&self) -> Vec<u8> {
        let mut b = Vec::new();
        put_string(&mut b, b"ssh-ed25519");
        put_string(&mut b, self.kp.public_key().as_ref());
        b
    }

    /// `allowed_signers` 的一行（principal `release@kairos-pulse`）
    pub fn allowed_signers(&self) -> String {
        format!(
            "release@kairos-pulse namespaces=\"kairos-pulse-release\" ssh-ed25519 {} test\n",
            crate::enroll::x509::base64_std(&self.blob())
        )
    }

    /// SSHSIG（namespace `kairos-pulse-release`、`sha512`）的 armor
    pub fn sign(&self, message: &[u8]) -> String {
        let ns = b"kairos-pulse-release";
        let digest = ring::digest::digest(&ring::digest::SHA512, message);
        let mut signed = b"SSHSIG".to_vec();
        put_string(&mut signed, ns);
        put_string(&mut signed, b"");
        put_string(&mut signed, b"sha512");
        put_string(&mut signed, digest.as_ref());
        let sig = self.kp.sign(&signed);
        let mut sigblob = Vec::new();
        put_string(&mut sigblob, b"ssh-ed25519");
        put_string(&mut sigblob, sig.as_ref());
        let mut blob = b"SSHSIG".to_vec();
        blob.extend_from_slice(&1u32.to_be_bytes());
        put_string(&mut blob, &self.blob());
        put_string(&mut blob, ns);
        put_string(&mut blob, b"");
        put_string(&mut blob, b"sha512");
        put_string(&mut blob, &sigblob);
        let b64 = crate::enroll::x509::base64_std(&blob);
        let mut out = String::from("-----BEGIN SSH SIGNATURE-----\n");
        for line in b64.as_bytes().chunks(70) {
            out.push_str(std::str::from_utf8(line).unwrap());
            out.push('\n');
        }
        out.push_str("-----END SSH SIGNATURE-----\n");
        out
    }
}

/// 一次測試用的發佈
pub struct TestRelease {
    pub sums: String,
    pub sig: String,
    pub manifest_name: String,
    pub manifest: String,
}

/// 發佈清單、`SHA256SUMS`（含清單與 `files`）與簽章
pub fn release(
    key: &TestKey,
    version: &str,
    commit: &str,
    files_in: &[(&str, &[u8])],
    unit: Option<(&str, &[&str])>,
) -> TestRelease {
    let unit = unit.map_or_else(String::new, |(sha, prev)| {
        format!(
            ",\"unit\":{{\"sha256\":\"{sha}\",\"previous_sha256\":[{}]}}",
            prev.iter()
                .map(|p| format!("\"{p}\""))
                .collect::<Vec<_>>()
                .join(",")
        )
    });
    let manifest = format!(
        "{{\"schema\":\"kairos-pulse.release/v1\",\"product\":\"pulse\",\"version\":\"{version}\",\"commit\":\"{commit}\",\"released_at\":\"2026-11-18\",\"min_kairos\":\"0.5.0\",\"min_upgrade_from\":\"0.9.0\",\"remote_features\":[\"config\",\"upgrade\"]{unit},\"breaking\":false}}\n"
    );
    let manifest_name = format!("pulse-{version}-{commit}-release.json");
    let mut entries: Vec<(String, String)> = files_in
        .iter()
        .map(|(n, b)| {
            (
                n.to_string(),
                files::hex(ring::digest::digest(&ring::digest::SHA256, b).as_ref()),
            )
        })
        .collect();
    entries.push((
        manifest_name.clone(),
        files::hex(ring::digest::digest(&ring::digest::SHA256, manifest.as_bytes()).as_ref()),
    ));
    entries.sort();
    let sums: String = entries.iter().map(|(n, h)| format!("{h}  {n}\n")).collect();
    let sig = key.sign(sums.as_bytes());
    TestRelease {
        sums,
        sig,
        manifest_name,
        manifest,
    }
}
