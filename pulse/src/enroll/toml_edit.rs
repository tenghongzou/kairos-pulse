//! `pulse.toml` 的最小改寫（設計包 `enrollment-ux.md` §7）：只設定指定的鍵，其他內容（含註解）原樣保留。
//!
//! 規則：在 `[table]` 區段內找到 `key = …` 的行就整行取代；區段內沒有這個鍵時加在區段的最後一個
//! 非空白行之後；沒有這個區段時附加在檔案結尾。寫入前把原檔備份為 `pulse.toml.bak-<時間>`，
//! 以暫存檔＋rename 原子取代（權限沿用原檔）。不支援 inline table 與多行字串中的同名鍵（Pulse 的設定沒有）。

use std::path::{Path, PathBuf};

/// TOML 的值
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Str(String),
    Int(i64),
    Bool(bool),
}

impl Value {
    fn render(&self) -> String {
        match self {
            Self::Str(s) => {
                let mut out = String::from("\"");
                for c in s.chars() {
                    match c {
                        '"' => out.push_str("\\\""),
                        '\\' => out.push_str("\\\\"),
                        '\n' => out.push_str("\\n"),
                        '\t' => out.push_str("\\t"),
                        c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
                        c => out.push(c),
                    }
                }
                out.push('"');
                out
            }
            Self::Int(i) => i.to_string(),
            Self::Bool(b) => b.to_string(),
        }
    }
}

fn section_of(line: &str) -> Option<String> {
    let t = line.trim();
    let inner = t.strip_prefix('[')?.split(']').next()?;
    (!t.starts_with("[[")).then(|| inner.trim().to_string())
}

fn key_of(line: &str) -> Option<&str> {
    let t = line.trim_start();
    if t.starts_with('#') || t.starts_with('[') {
        return None;
    }
    let (k, _) = t.split_once('=')?;
    Some(k.trim().trim_matches('"'))
}

/// 在文字中設定 `[table] key = value`
pub fn set(text: &str, table: &str, key: &str, value: &Value) -> String {
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let new_line = format!("{key} = {}", value.render());
    let mut current: Option<String> = None;
    let mut section_end: Option<usize> = None;
    let mut found_section = false;
    for i in 0..lines.len() {
        if let Some(sec) = section_of(&lines[i]) {
            current = Some(sec.clone());
            if sec == table {
                found_section = true;
                section_end = Some(i);
            }
            continue;
        }
        if current.as_deref() == Some(table) {
            if key_of(&lines[i]) == Some(key) {
                lines[i] = new_line;
                return join(lines);
            }
            if !lines[i].trim().is_empty() {
                section_end = Some(i);
            }
        }
    }
    if found_section {
        let at = section_end.map_or(lines.len(), |i| i + 1);
        lines.insert(at, new_line);
    } else {
        if lines.last().is_some_and(|l| !l.trim().is_empty()) {
            lines.push(String::new());
        }
        lines.push(format!("[{table}]"));
        lines.push(new_line);
    }
    join(lines)
}

fn join(lines: Vec<String>) -> String {
    let mut s = lines.join("\n");
    s.push('\n');
    s
}

/// 套用多個設定、備份原檔並原子寫入；回傳備份的路徑（原檔不存在時 `None`）
pub fn apply(path: &Path, changes: &[(&str, &str, Value)]) -> std::io::Result<Option<PathBuf>> {
    let original = match std::fs::read_to_string(path) {
        Ok(t) => Some(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    let mut text = original.clone().unwrap_or_default();
    for (table, key, value) in changes {
        text = set(&text, table, key, value);
    }
    if original.as_deref() == Some(text.as_str()) {
        return Ok(None);
    }
    let backup = match &original {
        Some(o) => {
            let name = format!(
                "{}.bak-{}",
                path.file_name()
                    .map_or_else(|| "pulse.toml".into(), |n| n.to_string_lossy().into_owned()),
                chrono::Local::now().format("%Y%m%d-%H%M%S")
            );
            let b = path.with_file_name(name);
            std::fs::write(&b, o)?;
            Some(b)
        }
        None => None,
    };
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let tmp = dir.join(format!(".pulse.toml.{}.tmp", uuid::Uuid::new_v4().simple()));
    std::fs::write(&tmp, text.as_bytes())?;
    if let Ok(meta) = std::fs::metadata(path) {
        let _ = std::fs::set_permissions(&tmp, meta.permissions());
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(backup)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_inserts_and_appends_keeping_comments() {
        let text = "# 註解\n[server]\naddress = \"127.0.0.1\"  # 舊\nport = 8080\n\n[log]\nlevel = \"info\"\n";
        let t = set(
            text,
            "server",
            "address",
            &Value::Str("kairos.example.gov.tw".into()),
        );
        assert!(t.contains("address = \"kairos.example.gov.tw\"\n"));
        assert!(t.starts_with("# 註解\n"));
        let t = set(&t, "server", "use_system_roots", &Value::Bool(false));
        let server: Vec<&str> = t.split("[log]").next().unwrap().lines().collect();
        assert!(server.contains(&"use_system_roots = false"), "{t}");
        let t = set(&t, "enroll", "port", &Value::Int(9444));
        assert!(t.ends_with("[enroll]\nport = 9444\n"), "{t}");
        let t = set(
            &t,
            "server",
            "ca_cert",
            &Value::Str(r#"C:\ProgramData\Pulse\identity\ca.pem"#.into()),
        );
        assert!(
            t.contains(r#"ca_cert = "C:\\ProgramData\\Pulse\\identity\\ca.pem""#),
            "{t}"
        );
        // 解析後的值正確
        let v: toml_value::Doc = toml_value::parse(&t);
        assert_eq!(v.get("server", "port"), Some("8080"));
    }

    #[test]
    fn apply_backs_up_and_writes_atomically() {
        let dir = std::env::temp_dir()
            .join("pulse-toml-edit")
            .join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("pulse.toml");
        assert_eq!(
            apply(&p, &[("server", "port", Value::Int(8443))]).unwrap(),
            None
        );
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            "[server]\nport = 8443\n"
        );
        let backup = apply(&p, &[("server", "port", Value::Int(9443))])
            .unwrap()
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(backup).unwrap(),
            "[server]\nport = 8443\n"
        );
        // 沒有變更時不備份
        assert_eq!(
            apply(&p, &[("server", "port", Value::Int(9443))]).unwrap(),
            None
        );
    }

    /// 測試用：極簡的 `[table] key = value` 讀取
    mod toml_value {
        pub struct Doc(Vec<(String, String, String)>);
        impl Doc {
            pub fn get(&self, t: &str, k: &str) -> Option<&str> {
                self.0
                    .iter()
                    .find(|(a, b, _)| a == t && b == k)
                    .map(|(_, _, v)| v.as_str())
            }
        }
        pub fn parse(text: &str) -> Doc {
            let mut cur = String::new();
            let mut out = Vec::new();
            for l in text.lines() {
                if let Some(s) = super::section_of(l) {
                    cur = s;
                } else if let Some((k, v)) = l.split_once('=')
                    && !l.trim_start().starts_with('#')
                {
                    out.push((
                        cur.clone(),
                        k.trim().into(),
                        v.split('#').next().unwrap().trim().into(),
                    ));
                }
            }
            Doc(out)
        }
    }
}
