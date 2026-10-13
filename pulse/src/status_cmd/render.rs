//! 人看的輸出（`cli-spec.md` §4.3～§4.6）：檢查列寫 stdout，三段式說明寫 stderr（`cli-style.md` §2）。

use super::eval::{HostStatus, Outcome, Report};
use super::issues::{Issue, Severity};
use super::term::Style;

/// 檢查列名稱後的空白（「服務」後三個空白，續行縮排到同一欄）
const NAME_PAD: &str = "   ";
/// 續行的縮排：結果欄 6 欄＋空白＋名稱 4 欄＋3 個空白
const CONT_INDENT: &str = "              ";

/// 檢查列的結果欄（Unicode「✓ 通過」／ASCII「[ OK ]」，同為 6 欄寬）
fn result_label(o: Outcome, style: &Style) -> String {
    let (sym, word, ascii, color) = match o {
        Outcome::Ok => ("✓", "通過", "[ OK ]", "32"),
        Outcome::Warning => ("⚠", "警告", "[WARN]", "33"),
        Outcome::Error => ("✗", "錯誤", "[FAIL]", "1;31"),
        Outcome::Offline => ("○", "離線", "[DOWN]", "1"),
        Outcome::Pending => ("…", "等待", "[WAIT]", "36"),
        Outcome::Skipped => ("-", "略過", "[SKIP]", ""),
    };
    let text = if style.unicode {
        format!("{sym} {word}")
    } else {
        ascii.to_string()
    };
    style.paint(color, &text)
}

fn status_color(s: HostStatus) -> &'static str {
    match s {
        HostStatus::Online => "32",
        HostStatus::Warning => "33",
        HostStatus::Critical => "1;31",
        HostStatus::Offline => "1",
        HostStatus::Pending => "36",
        HostStatus::Unknown => "",
    }
}

/// ASCII 模式：`…` 改為 `...`、`›` 改為 `>`（`cli-spec.md` §1.1）
fn ascii_fallback(text: String, style: &Style) -> String {
    if style.unicode {
        text
    } else {
        text.replace('…', "...").replace('›', ">")
    }
}

/// 「結果：1 項離線、1 個警告、4 項通過。」
fn summary(report: &Report) -> String {
    let count = |o: Outcome| report.rows.iter().filter(|r| r.result == o).count();
    let mut parts = Vec::new();
    for (o, unit) in [
        (Outcome::Error, "個錯誤"),
        (Outcome::Offline, "項離線"),
        (Outcome::Warning, "個警告"),
        (Outcome::Pending, "項等待"),
        (Outcome::Skipped, "項略過"),
        (Outcome::Ok, "項通過"),
    ] {
        let n = count(o);
        if n > 0 {
            parts.push(format!("{n} {unit}"));
        }
    }
    format!("結果：{}。", parts.join("、"))
}

/// stdout 的內容：標頭、檢查列、本機狀態與摘要
pub fn rows(report: &Report, style: &Style) -> String {
    let mut out = String::new();
    let agent = match report.agent_id.as_deref() {
        Some(id) => id,
        // 未啟用 mTLS 時 agent_id 等同身分憑據，狀態檔不寫；root 執行時從 agent.id 讀得到
        None if report.agent_id_hidden => "（未啟用 mTLS，不顯示）",
        None => "（未知）",
    };
    out.push_str(&format!(
        "{} · {} · agent_id {agent}\n\n",
        style.paint(
            "1",
            &format!("Pulse {} ({})", report.version, report.commit)
        ),
        report.hostname
    ));
    for row in &report.rows {
        let mut lines = row.lines.iter();
        if let Some(first) = lines.next() {
            out.push_str(&format!(
                "{} {}{NAME_PAD}{first}\n",
                result_label(row.result, style),
                row.name
            ));
        }
        for more in lines {
            out.push_str(&format!("{CONT_INDENT}{more}\n"));
        }
    }
    out.push_str(&format!(
        "\n本機狀態：{}\n{}\n",
        style.paint(status_color(report.status), report.status.label()),
        summary(report)
    ));
    ascii_fallback(out, style)
}

/// stderr 的內容：每則說明前空一行
pub fn issues(report: &Report, style: &Style, quiet: bool, verbose: bool) -> String {
    let mut out = String::new();
    for issue in &report.issues {
        if quiet && issue.severity != Severity::Error {
            continue;
        }
        out.push('\n');
        out.push_str(&issue_block(issue, style, verbose));
    }
    ascii_fallback(out, style)
}

/// 一則三段式說明（`cli-style.md` §5.1）
pub fn issue_block(issue: &Issue, style: &Style, verbose: bool) -> String {
    let mut out = String::new();
    let head = match issue.severity {
        Severity::Error => {
            let sym = if style.unicode { "✗" } else { "[FAIL]" };
            format!(
                "{} {}：{}\n",
                style.paint("1;31", sym),
                style.paint("1;31", &format!("錯誤 {}", issue.code)),
                issue.title
            )
        }
        Severity::Warning => {
            let sym = if style.unicode { "⚠" } else { "[WARN]" };
            format!(
                "{} {} {}：{}\n",
                style.paint("33", sym),
                style.paint("33", "警告"),
                issue.code,
                issue.title
            )
        }
    };
    out.push_str(&head);
    for (i, line) in issue.cause.iter().enumerate() {
        if i == 0 {
            out.push_str(&format!("  原因：{line}\n"));
        } else {
            out.push_str(&format!("        {line}\n"));
        }
    }
    let label = style.paint("36", "處理方式");
    match (&issue.fix_intro, issue.fix.as_slice()) {
        (None, []) => {}
        (Some(intro), []) => out.push_str(&format!("  {label}：{intro}\n")),
        (None, [only]) => {
            let mut lines = only.lines.iter();
            out.push_str(&format!(
                "  {label}：{}\n",
                lines.next().map(String::as_str).unwrap_or("")
            ));
            for l in lines {
                out.push_str(&format!("            {l}\n"));
            }
            for c in &only.commands {
                out.push_str(&format!("         {}\n", style.paint("36", c)));
            }
        }
        (intro, steps) => {
            match intro {
                Some(intro) => out.push_str(&format!("  {label}：{intro}\n")),
                None => out.push_str(&format!("  {label}：\n")),
            }
            for (n, s) in steps.iter().enumerate() {
                for (i, l) in s.lines.iter().enumerate() {
                    if i == 0 {
                        out.push_str(&format!("    {}. {l}\n", n + 1));
                    } else {
                        out.push_str(&format!("       {l}\n"));
                    }
                }
                for c in &s.commands {
                    out.push_str(&format!("         {}\n", style.paint("36", c)));
                }
            }
        }
    }
    if verbose && !issue.details.is_empty() {
        let details: Vec<String> = issue
            .details
            .iter()
            .filter(|(_, v)| !v.is_null())
            .map(|(k, v)| match v {
                serde_json::Value::String(s) => format!("{k}={s}"),
                other => format!("{k}={other}"),
            })
            .collect();
        if !details.is_empty() {
            out.push_str(&format!("  技術細節：{}\n", details.join("；")));
        }
    }
    out.push_str(&format!("  說明：pulse explain {}\n", issue.code));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_are_six_columns_in_both_modes() {
        let unicode = Style {
            unicode: true,
            color: false,
        };
        for o in [
            Outcome::Ok,
            Outcome::Warning,
            Outcome::Error,
            Outcome::Offline,
            Outcome::Pending,
            Outcome::Skipped,
        ] {
            // Unicode：符號（1 欄）＋空白＋兩個中文字（4 欄）
            assert_eq!(result_label(o, &unicode).chars().count(), 4, "{o:?}");
            assert_eq!(result_label(o, &Style::PLAIN).len(), 6, "{o:?}");
        }
        assert_eq!(result_label(Outcome::Ok, &unicode), "✓ 通過");
        assert_eq!(result_label(Outcome::Offline, &Style::PLAIN), "[DOWN]");
        let colored = Style {
            unicode: true,
            color: true,
        };
        assert_eq!(
            result_label(Outcome::Error, &colored),
            "\x1b[1;31m✗ 錯誤\x1b[0m"
        );
    }

    #[test]
    fn ascii_mode_replaces_ellipsis() {
        assert_eq!(ascii_fallback("a…b›c".into(), &Style::PLAIN), "a...b>c");
    }
}
