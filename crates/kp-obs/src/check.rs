//! 多項檢查（`--check-config`、`diag`）的結果與輸出（`design-system/cli-style.md` §5.5、§8）：
//!
//! ```text
//! ✓ 通過 設定檔     /etc/pulse/pulse.toml 可以載入
//! ⚠ 警告 權限       client.key 權限過寬（0644）
//!
//! 結果：1 個警告、7 項通過。設定可以使用。
//!
//! ⚠ 警告 PLS-W8202：權限
//!   原因：…
//! ```

use serde_json::{Value, json};

/// 一項檢查的結果（依嚴重度排序）
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Outcome {
    Ok,
    Skipped,
    Warning,
    Error,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::Skipped => "skipped",
            Outcome::Warning => "warning",
            Outcome::Error => "error",
        }
    }

    fn mark(self) -> &'static str {
        match self {
            Outcome::Ok => "✓ 通過",
            Outcome::Warning => "⚠ 警告",
            Outcome::Error => "✗ 錯誤",
            Outcome::Skipped => "- 略過",
        }
    }
}

/// 一項檢查
#[derive(Debug, Clone)]
pub struct Check {
    pub id: &'static str,
    pub label: &'static str,
    pub result: Outcome,
    pub code: Option<&'static str>,
    pub summary: String,
    /// 警告與錯誤的原因與處理方式（人看的輸出在結果之後列出）
    pub details: Vec<String>,
    /// 憑證問題（結束代碼 4）
    pub cert: bool,
}

impl Check {
    pub fn new(
        id: &'static str,
        label: &'static str,
        result: Outcome,
        summary: impl Into<String>,
    ) -> Self {
        Self {
            id,
            label,
            result,
            code: None,
            summary: summary.into(),
            details: Vec::new(),
            cert: false,
        }
    }

    pub fn code(mut self, c: &'static str) -> Self {
        self.code = Some(c);
        self
    }

    pub fn detail(mut self, d: impl Into<String>) -> Self {
        self.details.push(d.into());
        self
    }

    /// 提高到至少 `o`，並加上說明
    pub fn raise(&mut self, o: Outcome, detail: impl Into<String>) {
        self.result = self.result.max(o);
        self.details.push(detail.into());
    }

    pub fn cert_error(mut self) -> Self {
        self.cert = true;
        self
    }

    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "label": self.label,
            "result": self.result.as_str(),
            "code": self.code,
            "summary": self.summary,
            "details": self.details,
        })
    }
}

/// 某種結果的數量
pub fn count(checks: &[Check], o: Outcome) -> usize {
    checks.iter().filter(|c| c.result == o).count()
}

/// 結束代碼：憑證錯誤 4、其他錯誤 3、`strict` 且有警告 8，否則 0
pub fn exit_code(checks: &[Check], strict: bool) -> u8 {
    if checks.iter().any(|c| c.result == Outcome::Error && c.cert) {
        4
    } else if count(checks, Outcome::Error) > 0 {
        3
    } else if strict && count(checks, Outcome::Warning) > 0 {
        8
    } else {
        0
    }
}

/// 「結果：1 個錯誤、2 個警告、14 項通過。<結論>」
pub fn summary_line(checks: &[Check], product: &str) -> String {
    let mut parts = Vec::new();
    let e = count(checks, Outcome::Error);
    let w = count(checks, Outcome::Warning);
    let ok = count(checks, Outcome::Ok);
    let s = count(checks, Outcome::Skipped);
    if e > 0 {
        parts.push(format!("{e} 個錯誤"));
    }
    if w > 0 {
        parts.push(format!("{w} 個警告"));
    }
    if ok > 0 {
        parts.push(format!("{ok} 項通過"));
    }
    if s > 0 {
        parts.push(format!("{s} 項略過"));
    }
    let tail = if e > 0 {
        format!("{product} 無法以此設定啟動。")
    } else {
        "設定可以使用。".to_string()
    };
    format!("結果：{}。{tail}", parts.join("、"))
}

/// 人看的輸出（`header` 為第一行的環境摘要）
pub fn render_text(header: &str, checks: &[Check], product: &str) -> String {
    let mut out = format!("{header}\n\n");
    for c in checks {
        out.push_str(&format!(
            "{} {:<10} {}\n",
            c.result.mark(),
            c.label,
            c.summary
        ));
    }
    out.push('\n');
    out.push_str(&summary_line(checks, product));
    out.push('\n');
    for c in checks
        .iter()
        .filter(|c| matches!(c.result, Outcome::Warning | Outcome::Error))
    {
        out.push('\n');
        match c.code {
            Some(code) => out.push_str(&format!("{} {code}：{}\n", c.result.mark(), c.label)),
            None => out.push_str(&format!("{}：{}\n", c.result.mark(), c.label)),
        }
        for d in &c.details {
            out.push_str(&format!("  {d}\n"));
        }
    }
    out
}

/// `--json` 的外層（`cli-style.md` §7）
pub fn to_json(
    schema: &str,
    generated_at: &str,
    checks: &[Check],
    strict: bool,
    extra: Value,
) -> Value {
    let result = if count(checks, Outcome::Error) > 0 {
        "error"
    } else if count(checks, Outcome::Warning) > 0 {
        "warning"
    } else {
        "ok"
    };
    let mut data = json!({
        "result": result,
        "checks": checks.iter().map(Check::to_json).collect::<Vec<_>>(),
    });
    if let (Value::Object(d), Value::Object(e)) = (&mut data, extra) {
        d.extend(e);
    }
    json!({
        "schema": schema,
        "ok": count(checks, Outcome::Error) == 0,
        "generated_at": generated_at,
        "exit_code": exit_code(checks, strict),
        "data": data,
        "warnings": [],
        "error": null,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rendering_and_exit_codes() {
        let mut checks = vec![
            Check::new("a", "設定檔", Outcome::Ok, "可以載入"),
            Check::new("b", "權限", Outcome::Warning, "過寬")
                .code("PLS-W8202")
                .detail("原因：x"),
            Check::new("c", "其他", Outcome::Skipped, "略過"),
        ];
        assert_eq!(exit_code(&checks, false), 0);
        assert_eq!(exit_code(&checks, true), 8);
        let t = render_text("pulse 0.6.0", &checks, "Pulse");
        assert!(t.contains("✓ 通過 設定檔"));
        assert!(
            t.contains("結果：1 個警告、1 項通過、1 項略過。設定可以使用。"),
            "{t}"
        );
        assert!(t.contains("⚠ 警告 PLS-W8202：權限\n  原因：x"));
        checks[0].raise(Outcome::Error, "壞了");
        assert_eq!(exit_code(&checks, false), 3);
        checks.push(Check::new("d", "憑證", Outcome::Error, "x").cert_error());
        assert_eq!(exit_code(&checks, false), 4);
        let v = to_json("x/v1", "t", &checks, false, json!({"config_file": "p"}));
        assert_eq!(v["data"]["result"], "error");
        assert_eq!(v["data"]["config_file"], "p");
        assert_eq!(v["exit_code"], 4);
        assert_eq!(v["data"]["checks"][1]["code"], "PLS-W8202");
    }
}
