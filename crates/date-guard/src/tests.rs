//! 共用核心的單元測試。完整情境（逐步推進、跨重新啟動、狀態檔讀寫）另由 Kairos 的
//! `retention.rs` 與 Pulse 的 `log_files.rs` 透過各自的包裝層測試。

use super::*;
use chrono::TimeZone;
use std::path::Path;

const H: Duration = Duration::from_secs(3600);

fn date(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).unwrap()
}

fn wall(y: i32, m: u32, d: u32, h: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(y, m, d, h, 0, 0).unwrap()
}

fn load(json: &str, legacy: LegacyState) -> Option<DateGuard> {
    DateGuard::from_state(
        json.as_bytes(),
        legacy,
        Instant::now(),
        wall(2026, 10, 9, 12),
    )
}

#[test]
fn legacy_state_is_accepted_only_when_requested() {
    // 舊版 Kairos 的格式：只有 accepted 與 jump_since_unix
    let legacy = r#"{"accepted":"2026-10-09","jump_since_unix":null}"#;
    let g = load(legacy, LegacyState::Accept).unwrap();
    assert_eq!(g.accepted(), date(2026, 10, 9));
    assert_eq!(g.anchor_date(), date(2026, 10, 9));
    assert!(!g.jump_pending());
    assert!(load(legacy, LegacyState::Reject).is_none());

    // 基準點欄位只有一部分：兩種模式都視為損毀
    let partial = r#"{"accepted":"2026-10-09","anchor_date":"2026-10-09"}"#;
    assert!(load(partial, LegacyState::Accept).is_none());
    assert!(load(partial, LegacyState::Reject).is_none());
    // 新格式的 null 等同缺少
    let nulls =
        r#"{"accepted":"2026-10-09","anchor_date":null,"anchor_elapsed_secs":1,"saved_unix":1}"#;
    assert!(load(nulls, LegacyState::Accept).is_none());
    assert!(load(nulls, LegacyState::Reject).is_none());
}

#[test]
fn jump_fields_must_be_paired_unless_legacy() {
    let base = r#""accepted":"2026-10-09","anchor_date":"2026-10-09","anchor_elapsed_secs":0,"saved_unix":1791547200"#;
    let since_only = format!(r#"{{{base},"jump_since_unix":1791547200}}"#);
    let date_only = format!(r#"{{{base},"jump_date":"2027-10-09"}}"#);
    let both = format!(r#"{{{base},"jump_since_unix":1791547200,"jump_date":"2027-10-09"}}"#);

    // 舊版：有起點沒有日期 → 暫停中、日期待補；只有日期 → 忽略
    let g = load(&since_only, LegacyState::Accept).unwrap();
    assert!(g.jump_pending());
    assert_eq!(g.jump_date(), None);
    let g = load(&date_only, LegacyState::Accept).unwrap();
    assert!(!g.jump_pending());
    // 新格式：必須成對
    assert!(load(&since_only, LegacyState::Reject).is_none());
    assert!(load(&date_only, LegacyState::Reject).is_none());
    for legacy in [LegacyState::Accept, LegacyState::Reject] {
        let g = load(&both, legacy).unwrap();
        assert_eq!(g.jump_date(), Some(date(2027, 10, 9)));
    }
}

#[test]
fn legacy_jump_without_date_is_filled_on_next_check() {
    let t0 = Instant::now();
    let json = r#"{"accepted":"2026-10-09","jump_since_unix":1791547200}"#;
    let mut g = DateGuard::from_state(
        json.as_bytes(),
        LegacyState::Accept,
        t0,
        wall(2027, 10, 9, 12),
    )
    .unwrap();
    let d = g.decide(date(2027, 10, 9), t0, wall(2027, 10, 9, 12));
    assert_eq!(d.cleanup_date, None);
    // 補上日期時不是「重新起算」，也不產生事件
    assert!(d.events.is_empty(), "{:?}", d.events);
    assert_eq!(g.jump_date(), Some(date(2027, 10, 9)));
    assert_eq!(
        g.state(t0, wall(2027, 10, 9, 12)).jump_date.as_deref(),
        Some("2027-10-09")
    );
}

#[test]
fn events_describe_each_transition() {
    let t0 = Instant::now();
    let w = wall(2026, 10, 9, 12);
    let mut g = DateGuard::new(date(2026, 10, 9), t0, w);
    let mut check = |today: NaiveDate, at: Duration| g.decide(today, t0 + at, w);

    // 正常推進：沒有事件
    assert_eq!(
        check(date(2026, 10, 10), H),
        Decision {
            cleanup_date: Some(date(2026, 10, 10)),
            events: vec![]
        }
    );
    // 跳躍
    let d = check(date(2027, 1, 1), H * 2);
    assert_eq!(d.cleanup_date, None);
    assert_eq!(
        d.events,
        [GuardEvent::JumpStarted {
            today: date(2027, 1, 1),
            allowed_max: date(2026, 10, 11),
            accepted: date(2026, 10, 10),
        }]
    );
    // 暫停中再往前超過 2 天：重新起算
    assert_eq!(
        check(date(2027, 1, 4), H * 3).events,
        [GuardEvent::JumpRestarted {
            today: date(2027, 1, 4),
            previous: date(2027, 1, 1),
        }]
    );
    // 持續 24 小時後接受
    assert_eq!(check(date(2027, 1, 4), H * 26).cleanup_date, None);
    let d = check(date(2027, 1, 5), H * 27);
    assert_eq!(d.cleanup_date, Some(date(2027, 1, 5)));
    assert_eq!(
        d.events,
        [GuardEvent::JumpAccepted {
            today: date(2027, 1, 5)
        }]
    );
    // 回撥：開始計時，24 小時後降回
    assert_eq!(
        check(date(2026, 10, 11), H * 28).events,
        [GuardEvent::RollbackStarted {
            today: date(2026, 10, 11),
            accepted: date(2027, 1, 5),
        }]
    );
    assert_eq!(
        check(date(2026, 10, 12), H * 52).events,
        [GuardEvent::RollbackAccepted {
            today: date(2026, 10, 12),
            previous: date(2027, 1, 5),
        }]
    );
    // 再回撥後回到 accepted：取消回撥計時
    assert_eq!(check(date(2026, 10, 11), H * 53).events.len(), 1);
    assert_eq!(
        check(date(2026, 10, 12), H * 54).events,
        [GuardEvent::RollbackCleared {
            accepted: date(2026, 10, 12)
        }]
    );
    // 暫停中日期回到合理範圍，與因回撥而結束暫停，分別標示
    assert_eq!(check(date(2027, 6, 1), H * 55).events.len(), 1);
    assert_eq!(
        check(date(2026, 10, 12), H * 56).events,
        [GuardEvent::JumpCleared {
            today: date(2026, 10, 12),
            during_rollback: false
        }]
    );
    assert_eq!(check(date(2027, 6, 1), H * 57).events.len(), 1);
    assert_eq!(
        check(date(2026, 10, 1), H * 58).events,
        [
            GuardEvent::JumpCleared {
                today: date(2026, 10, 1),
                during_rollback: true
            },
            GuardEvent::RollbackStarted {
                today: date(2026, 10, 1),
                accepted: date(2026, 10, 12),
            }
        ]
    );
}

#[test]
fn state_round_trips_through_file_bytes() {
    let t0 = Instant::now();
    let w = wall(2026, 10, 9, 12);
    let mut g = DateGuard::new(date(2026, 10, 9), t0, w);
    g.decide(date(2027, 1, 1), t0 + H, w + chrono::Duration::hours(1));
    let w1 = w + chrono::Duration::hours(2);
    let state = g.state(t0 + H * 2, w1);
    let bytes = state.to_file_bytes().unwrap();
    // 兩個產品寫出的格式：縮排 JSON、固定欄位順序、結尾換行
    let text = String::from_utf8(bytes.clone()).unwrap();
    assert!(text.ends_with("}\n"), "{text}");
    let keys: Vec<&str> = text
        .lines()
        .filter_map(|l| l.trim().strip_prefix('"')?.split('"').next())
        .collect();
    assert_eq!(
        keys,
        [
            "accepted",
            "anchor_date",
            "anchor_elapsed_secs",
            "saved_unix",
            "jump_since_unix",
            "jump_date",
            "jump_elapsed_secs",
            "rollback_since_unix"
        ]
    );
    assert_eq!(state.anchor_elapsed_secs, Some(7200));
    assert_eq!(state.jump_elapsed_secs, Some(3600));
    // 新寫出的狀態兩種模式都能讀回
    for legacy in [LegacyState::Accept, LegacyState::Reject] {
        let back = DateGuard::from_state(&bytes, legacy, Instant::now(), w1).unwrap();
        assert_eq!(back.accepted(), date(2026, 10, 9));
        assert_eq!(back.jump_date(), Some(date(2027, 1, 1)));
        assert!(!back.rollback_pending());
    }
}

#[test]
fn dated_names_and_retention() {
    const LOGS: DatedName = DatedName {
        prefix: "x-",
        suffix: ".log",
    };
    let d = date(2026, 10, 9);
    assert_eq!(LOGS.file_name(d), "x-2026-10-09.log");
    assert_eq!(LOGS.parse("x-2026-10-09.log"), Some(d));
    for bad in [
        "x-2026-10-9.log",
        "x-2026-02-30.log",
        "x-+026-10-09.log",
        "x-2026-10-09.log.1",
        "y-2026-10-09.log",
    ] {
        assert_eq!(LOGS.parse(bad), None, "{bad}");
    }
    assert_eq!(LOGS.newest(Path::new("no-such-dir-date-guard"), d), None);
    assert_eq!(guard_baseline(Some(date(2030, 1, 1)), d), d);
    assert_eq!(
        guard_baseline(Some(date(2026, 10, 1)), d),
        date(2026, 10, 1)
    );
    assert_eq!(guard_baseline(None, d), d);
    assert!(!should_delete(date(2026, 9, 25), d, 14));
    assert!(should_delete(date(2026, 9, 24), d, 14));
    assert!(!should_delete(date(2000, 1, 1), d, 0));
}
