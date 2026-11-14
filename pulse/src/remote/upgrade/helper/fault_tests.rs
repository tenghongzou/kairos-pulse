//! 交易紀錄的故障注入測試（REMOTE.md 第 17.9 節；ADR-0003 階段 0b）：
//!
//! - **當機點矩陣**（第 17.9.4 節的對照表）：對照表的每一列 × 安裝方式 × 新版健康／崩潰，在那個 checkpoint
//!   中止第一次執行（[`apply_until`]：之後的程式都不會執行），檢查當機時磁碟上的狀態，再以「重新開機後的
//!   另一個行程」重新呼叫 [`apply`]，斷言結果、執行檔、`pulse.prev`、unit、套件資料庫、指標與工作目錄。
//!   [`the_matrix_covers_every_row_of_the_crash_point_table`] 讓矩陣與文件的對照表不會各自走樣。
//! - 紀錄或指標損毀、遺失（第 17.9.5 節）、舊格式（17.9.6）、路徑檢查（17.9.7）、保留上限（17.9.8）。
//!
//! 假的作業系統在 `tests.rs`。

#![cfg(unix)]

use super::tests::*;
use super::*;
use std::os::unix::fs::PermissionsExt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Tar,
    /// deb，有驗證過的回滾套件（回滾經套件管理員）
    Deb,
    /// deb，沒有可信的回滾套件（回滾換回 `pulse.prev`）
    DebNoRollback,
    /// rpm，有驗證過的回滾套件
    Rpm,
}

impl Kind {
    fn method(self) -> &'static str {
        match self {
            Kind::Tar => "tar",
            Kind::Deb | Kind::DebNoRollback => "deb",
            Kind::Rpm => "rpm",
        }
    }

    fn package(self) -> bool {
        self != Kind::Tar
    }

    fn rollback_package(self) -> bool {
        matches!(self, Kind::Deb | Kind::Rpm)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// 替換到健康確認（新版健康或崩潰都會經過）
    Forward,
    /// 回滾（只有新版崩潰時經過）
    Rollback,
    /// 結果已記錄之後
    Finish,
}

/// 對照表的一列
struct Point {
    /// 第 17.9.4 節的列號
    row: u8,
    /// checkpoint 的名稱（`health_wait`、`rollback_wait`、`package_running`、`rollback_package_running`
    /// 由假的作業系統在等待中或套件管理員執行到一半時注入）
    name: &'static str,
    phase: Phase,
    applies: fn(Kind) -> bool,
}

fn all(_: Kind) -> bool {
    true
}
fn tar_only(k: Kind) -> bool {
    k == Kind::Tar
}
fn package_only(k: Kind) -> bool {
    k.package()
}
fn binary_rollback(k: Kind) -> bool {
    !k.rollback_package()
}
fn package_rollback(k: Kind) -> bool {
    k.rollback_package()
}

const fn point(row: u8, name: &'static str, phase: Phase, applies: fn(Kind) -> bool) -> Point {
    Point {
        row,
        name,
        phase,
        applies,
    }
}

const POINTS: &[Point] = &[
    point(1, "before_record", Phase::Forward, all),
    point(2, "record_preparing", Phase::Forward, all),
    point(3, "record_prepared", Phase::Forward, all),
    point(4, "pointer_written", Phase::Forward, all),
    point(5, "record_replacing", Phase::Forward, all),
    point(6, "prev_copied", Phase::Forward, all),
    point(7, "prev_recorded", Phase::Forward, all),
    point(8, "unit_backed_up", Phase::Forward, tar_only),
    point(9, "unit_backup_recorded", Phase::Forward, tar_only),
    point(10, "package_running", Phase::Forward, package_only),
    point(11, "binary_replaced", Phase::Forward, all),
    point(12, "unit_replaced", Phase::Forward, tar_only),
    point(13, "record_replaced", Phase::Forward, all),
    point(14, "health_wait", Phase::Forward, all),
    point(15, "rollback_intent_recorded", Phase::Rollback, all),
    point(16, "rollback_restoring", Phase::Rollback, binary_rollback),
    point(
        16,
        "rollback_package_running",
        Phase::Rollback,
        package_rollback,
    ),
    point(17, "rollback_restored", Phase::Rollback, all),
    point(18, "record_rolling_back", Phase::Rollback, all),
    point(19, "rollback_wait", Phase::Rollback, all),
    point(20, "record_done", Phase::Finish, all),
    point(21, "result_written", Phase::Finish, all),
    point(22, "request_removed", Phase::Finish, all),
];

fn pkg_db(fake: &Fake) -> String {
    fake.pkg_db.borrow().clone().unwrap_or_default()
}

/// 當機時磁碟上的狀態（對照表的第三欄）
fn crash_state(env: &Env, fake: &Fake, kind: Kind, p: &Point, label: &str) {
    let rec = match txn::load_record(&env.work()) {
        Loaded::V2(r) => Some(*r),
        Loaded::Missing => None,
        other => panic!("{label}: 當機時的紀錄不可用：{other:?}"),
    };
    let expected_step = match p.name {
        "before_record" => None,
        "record_preparing" => Some(Step::Preparing),
        "record_prepared" | "pointer_written" => Some(Step::Prepared),
        "record_replacing"
        | "prev_copied"
        | "prev_recorded"
        | "unit_backed_up"
        | "unit_backup_recorded"
        | "package_running"
        | "binary_replaced"
        | "unit_replaced" => Some(Step::Replacing),
        "record_replaced" | "health_wait" => Some(Step::Replaced),
        "rollback_intent_recorded"
        | "rollback_restoring"
        | "rollback_package_running"
        | "rollback_restored" => Some(Step::RollbackIntent),
        "record_rolling_back" | "rollback_wait" => Some(Step::RollingBack),
        "record_done" | "result_written" | "request_removed" => Some(Step::Done),
        other => panic!("未知的 checkpoint {other}"),
    };
    assert_eq!(rec.as_ref().map(|r| r.step), expected_step, "{label}: 步驟");
    // 指標在 prepared 之後寫入，到交易結束才清除
    assert_eq!(env.active(), p.row >= 4, "{label}: 指標");
    match p.row {
        1..=9 => {
            assert_eq!(env.installed(), "0.9.0", "{label}: 還沒有替換");
            assert_eq!(env.unit(), OLD_UNIT, "{label}");
            assert_eq!(pkg_db(fake), "0.9.0 ii", "{label}");
        }
        10..=15 => assert_eq!(env.installed(), "0.9.1", "{label}: 已替換"),
        16..=19 => assert_eq!(env.installed(), "0.9.0", "{label}: 已換回"),
        _ => {}
    }
    assert_eq!(
        env.prev(),
        if p.row >= 6 { "0.9.0" } else { "" },
        "{label}: pulse.prev"
    );
    let Some(rec) = rec else { return };
    if p.row >= 3 {
        assert_eq!(rec.binding_sha256, rec.compute_binding(), "{label}: 綁定");
    }
    match p.name {
        "record_replacing" | "prev_copied" => assert!(!rec.prev_saved, "{label}"),
        "prev_recorded" => assert!(rec.prev_saved, "{label}"),
        "unit_backed_up" => {
            assert!(!rec.unit_backed_up, "{label}");
            assert_eq!(
                std::fs::read_to_string(env.work().join(txn::UNIT_BACKUP_FILE)).unwrap(),
                OLD_UNIT,
                "{label}"
            );
        }
        "unit_backup_recorded" => assert!(rec.unit_backed_up, "{label}"),
        "package_running" => assert_eq!(pkg_db(fake), "0.9.1 iF", "{label}: 半安裝"),
        "binary_replaced" => {
            if kind == Kind::Tar {
                assert_eq!(env.unit(), OLD_UNIT, "{label}: unit 還是舊的");
            } else {
                assert_eq!(pkg_db(fake), "0.9.1 ii", "{label}");
            }
        }
        "unit_replaced" => {
            assert_eq!(env.unit(), NEW_UNIT, "{label}");
            assert!(!rec.unit_replaced, "{label}: 尚未記錄");
        }
        "record_replaced" => {
            assert!(rec.deadline_unix > 0 && !rec.service_restarted, "{label}");
            assert!(
                !fake.calls().contains(&"restart".to_string()),
                "{label}: 服務尚未重新啟動"
            );
        }
        "health_wait" => assert!(rec.service_restarted, "{label}"),
        "rollback_intent_recorded" => {
            let intent = rec.rollback_intent.clone().unwrap();
            assert!(intent.reason.starts_with("crash_restart"), "{label}");
            assert_eq!(intent.cause, Cause::Health, "{label}");
            assert_eq!(
                intent.via,
                if kind.rollback_package() {
                    Via::Package
                } else {
                    Via::Binary
                },
                "{label}"
            );
        }
        "rollback_restoring" => {
            if kind == Kind::Tar {
                assert_eq!(env.unit(), NEW_UNIT, "{label}: unit 尚未還原");
            }
        }
        "rollback_package_running" => assert_eq!(pkg_db(fake), "0.9.0 iF", "{label}"),
        "rollback_restored" => {
            if kind == Kind::Tar {
                assert_eq!(env.unit(), OLD_UNIT, "{label}");
            }
            if kind.rollback_package() {
                assert_eq!(pkg_db(fake), "0.9.0 ii", "{label}");
            }
        }
        "record_rolling_back" => assert!(!rec.service_restarted, "{label}"),
        "record_done" => {
            assert!(rec.result.is_some(), "{label}");
            assert!(!env.has_result(), "{label}: result.json 尚未寫出");
        }
        "result_written" => {
            assert!(env.has_result() && env.paths.request.exists(), "{label}")
        }
        "request_removed" => {
            assert!(env.has_result() && !env.paths.request.exists(), "{label}")
        }
        _ => {}
    }
}

/// 交易結束後的結果與系統狀態（對照表的最後一欄）。`fake`：最後一次執行的假系統（套件資料庫）
fn final_state(
    env: &Env,
    fake: &Fake,
    kind: Kind,
    p: &Point,
    crashy: bool,
    r: &UpgradeResult,
    label: &str,
) {
    assert_eq!(
        (
            r.upgrade_id.as_str(),
            r.from_version.as_str(),
            r.to_version.as_str()
        ),
        (ID, "0.9.0", "0.9.1"),
        "{label}: {r:?}"
    );
    if crashy {
        assert_eq!(
            (r.state.as_str(), r.code.as_deref()),
            ("rolled_back", Some(codes::W_UPGRADE_ROLLED_BACK)),
            "{label}: {r:?}"
        );
        assert!(
            r.detail.starts_with("crash_restart"),
            "{label}: {}",
            r.detail
        );
        // 回滾途中中斷後接續的才加註 resumed
        assert_eq!(
            r.detail.contains("; resumed"),
            p.phase == Phase::Rollback,
            "{label}: {}",
            r.detail
        );
        assert_eq!(
            r.detail.contains("binary_restored_package_db_shows_0.9.1"),
            kind == Kind::DebNoRollback,
            "{label}: {}",
            r.detail
        );
        assert_eq!(env.installed(), "0.9.0", "{label}: 回滾換回的是舊版");
    } else {
        assert_eq!(
            (r.state.as_str(), r.code.as_deref(), r.detail.as_str()),
            ("succeeded", Some(codes::I_UPGRADE_COMPLETED), ""),
            "{label}: {r:?}"
        );
        assert_eq!(env.installed(), "0.9.1", "{label}");
    }
    // 任何一列接續之後，pulse.prev 都是替換前的版本
    assert_eq!(env.prev(), "0.9.0", "{label}: pulse.prev");
    match kind {
        Kind::Tar => assert_eq!(
            env.unit(),
            if crashy { OLD_UNIT } else { NEW_UNIT },
            "{label}: unit"
        ),
        Kind::Deb | Kind::Rpm => assert_eq!(
            pkg_db(fake),
            if crashy { "0.9.0 ii" } else { "0.9.1 ii" },
            "{label}: 套件資料庫"
        ),
        // 沒有回滾套件：只換回執行檔，套件資料庫仍是新版
        Kind::DebNoRollback => assert_eq!(pkg_db(fake), "0.9.1 ii", "{label}: 套件資料庫"),
    }
    assert_eq!(&env.result(), r, "{label}: result.json");
    assert!(!env.paths.request.exists(), "{label}: request.json");
    let rec = env.record();
    assert_eq!(rec.step, Step::Done, "{label}");
    assert_eq!(rec.result.as_ref(), Some(r), "{label}");
}

fn run_case(kind: Kind, p: &Point, crashy: bool) {
    let label = format!(
        "{kind:?}／第 {} 列 {}／新版{}",
        p.row,
        p.name,
        if crashy { "崩潰" } else { "健康" }
    );
    let mut env = setup(&format!("fm-{kind:?}-{}-{crashy}", p.name), kind.method());
    if kind.rollback_package() {
        stage_rollback(&env, "0.9.0");
    }
    let fake = Fake::new(&env.root);
    if crashy {
        fake.set("0.9.1", Behavior::Crash);
    }
    apply_until(&env.paths, &fake, p.name);
    fake.assert_invariants();
    crash_state(&env, &fake, kind, p, &label);

    // 重新開機：之後執行的輔助程式是目前安裝的執行檔
    env.helper_is_installed();
    if p.row >= 4 {
        // 指標寫入之後的接續不得再讀服務帳號可寫的暫存區
        let _ = std::fs::remove_dir_all(env.staging());
    }
    if p.name == "request_removed" {
        // request.json 已刪除：不會再被觸發，結果已寫出。下一個請求到來時只清除指標，再照常處理
        let r = env.result();
        final_state(&env, &fake, kind, p, crashy, &r, &label);
        assert!(env.active(), "{label}: 指標還在");
        let installed = env.installed();
        let to = if crashy { "0.9.1" } else { "0.9.2" };
        stage_upgrade(&env, OTHER_ID, &installed, to);
        let next = Fake::new(&env.root);
        let r2 = apply(&env.paths, &next).unwrap();
        assert_eq!(
            (r2.upgrade_id.as_str(), r2.state.as_str()),
            (OTHER_ID, "succeeded"),
            "{label}: {r2:?}"
        );
        assert_eq!(env.installed(), to, "{label}");
        assert!(!env.active(), "{label}");
        next.assert_invariants();
        // 只保留最近一筆已結束的紀錄
        assert!(!env.work().exists(), "{label}");
        assert!(env.paths.work_root.join(OTHER_ID).is_dir(), "{label}");
        env.cleanup();
        return;
    }
    let after = fake.reboot();
    let r = apply(&env.paths, &after).unwrap();
    after.assert_invariants();
    final_state(&env, &after, kind, p, crashy, &r, &label);
    assert!(!env.active(), "{label}: 指標已清除");
    assert_eq!(
        txn::traces(&env.work()),
        Vec::<String>::new(),
        "{label}: 已結束的交易只留紀錄與日誌"
    );
    assert!(
        std::fs::read_dir(&env.paths.install_dir)
            .unwrap()
            .flatten()
            .all(|e| !e.file_name().to_string_lossy().starts_with(".upgrade-")),
        "{label}: 預檢用的目錄已刪除"
    );
    // 接續時不重新解開發佈檔（第 4 列之後只用工作目錄裡的複本）
    if p.row >= 4 {
        assert!(
            !after.calls().iter().any(|c| c == "tar" || c == "extract"),
            "{label}: {:?}",
            after.calls()
        );
    }
    env.cleanup();
}

fn run_matrix(kind: Kind) {
    let mut cases = 0;
    for p in POINTS.iter().filter(|p| (p.applies)(kind)) {
        if p.phase != Phase::Rollback {
            run_case(kind, p, false);
            cases += 1;
        }
        run_case(kind, p, true);
        cases += 1;
    }
    assert!(cases >= 30, "{kind:?}: 只跑了 {cases} 個案例");
}

#[test]
fn crash_point_matrix_tar() {
    run_matrix(Kind::Tar);
}

#[test]
fn crash_point_matrix_deb_with_a_rollback_package() {
    run_matrix(Kind::Deb);
}

#[test]
fn crash_point_matrix_deb_without_a_rollback_package() {
    run_matrix(Kind::DebNoRollback);
}

#[test]
fn crash_point_matrix_rpm() {
    run_matrix(Kind::Rpm);
}

/// 矩陣的列與 REMOTE.md 第 17.9.4 節的對照表一致（列號、checkpoint 的名稱、沒有多也沒有少）
#[test]
fn the_matrix_covers_every_row_of_the_crash_point_table() {
    let doc = include_str!("../../../../../crates/kairos-proto/REMOTE.md");
    let section = doc
        .split("#### 17.9.4")
        .nth(1)
        .and_then(|s| s.split("#### 17.9.5").next())
        .expect("REMOTE.md 第 17.9.4 節");
    let mut rows: Vec<(u8, String)> = Vec::new();
    for line in section.lines() {
        let cells: Vec<&str> = line.split('|').map(str::trim).collect();
        if cells.len() < 6 {
            continue;
        }
        let Ok(n) = cells[1].trim_end_matches('★').trim().parse::<u8>() else {
            continue;
        };
        let name = match cells[2].split('`').nth(1) {
            Some(name) => name.to_string(),
            None if cells[2].contains("健康確認中") => "health_wait".to_string(),
            None if cells[2].contains("回滾確認中") => "rollback_wait".to_string(),
            None => panic!("對照表第 {n} 列沒有 checkpoint 的名稱：{line}"),
        };
        rows.push((n, name));
    }
    assert_eq!(rows.len(), 22, "{rows:?}");
    for (i, (n, name)) in rows.iter().enumerate() {
        assert_eq!(usize::from(*n), i + 1, "列號連續");
        let points: Vec<&Point> = POINTS.iter().filter(|p| p.row == *n).collect();
        assert!(
            points.iter().any(|p| p.name == name),
            "對照表第 {n} 列（{name}）沒有對應的測試"
        );
    }
    for p in POINTS {
        assert!(
            rows.iter().any(|(n, _)| *n == p.row),
            "矩陣的 {} 不在對照表中",
            p.name
        );
    }
    // 一次完整的執行（升級後崩潰、回滾）經過程式中的每一個 checkpoint；矩陣的名稱都在其中
    for kind in [Kind::Tar, Kind::Deb] {
        let env = setup(&format!("fm-seen-{kind:?}"), kind.method());
        if kind.rollback_package() {
            stage_rollback(&env, "0.9.0");
        }
        let fake = Fake::new(&env.root);
        fake.set("0.9.1", Behavior::Crash);
        assert_eq!(apply(&env.paths, &fake).unwrap().state, "rolled_back");
        let seen = fake.seen();
        let injected = [
            "health_wait",
            "rollback_wait",
            "package_running",
            "rollback_package_running",
        ];
        for p in POINTS.iter().filter(|p| (p.applies)(kind)) {
            assert!(
                injected.contains(&p.name) || seen.iter().any(|s| s == p.name),
                "{kind:?}: 沒有經過 {}（經過 {seen:?}）",
                p.name
            );
        }
        for s in &seen {
            assert!(
                POINTS.iter().any(|p| p.name == s),
                "程式中的 checkpoint {s} 不在對照表中"
            );
        }
        env.cleanup();
    }
}

/// 對照表第 13、14 列，**健康確認的期限已過才接續**（0.10.1；REMOTE.md 第 17.9.3 節「接續 `replaced` 時的
/// 健康確認」）：不再直接回滾——健康 → `succeeded`；不健康（連不上 Kairos）→ 確認窗結束後回滾。每一種安裝
/// 方式各跑一次
#[test]
fn rows_13_and_14_resumed_after_the_deadline_check_health_before_rolling_back() {
    let mut cases = 0;
    for kind in [Kind::Tar, Kind::Deb, Kind::DebNoRollback, Kind::Rpm] {
        for p in POINTS.iter().filter(|p| p.row == 13 || p.row == 14) {
            for healthy in [true, false] {
                let label = format!(
                    "{kind:?}／第 {} 列 {}／期限已過／新版{}",
                    p.row,
                    p.name,
                    if healthy { "健康" } else { "連不上" }
                );
                let mut env = setup(
                    &format!("fm-late-{kind:?}-{}-{healthy}", p.name),
                    kind.method(),
                );
                if kind.rollback_package() {
                    stage_rollback(&env, "0.9.0");
                }
                let fake = Fake::new(&env.root);
                apply_until(&env.paths, &fake, p.name);
                fake.assert_invariants();
                crash_state(&env, &fake, kind, p, &label);
                env.helper_is_installed();
                let _ = std::fs::remove_dir_all(env.staging());
                // 一天之後才開機（health_timeout_secs 是 600）
                let after = fake.reboot_after(86_400);
                if !healthy {
                    after.set("0.9.1", Behavior::NoConnect);
                }
                let r = apply(&env.paths, &after).unwrap();
                after.assert_invariants();
                if healthy {
                    final_state(&env, &after, kind, p, false, &r, &label);
                } else {
                    assert_eq!(
                        (r.state.as_str(), r.code.as_deref()),
                        ("rolled_back", Some(codes::W_UPGRADE_ROLLED_BACK)),
                        "{label}: {r:?}"
                    );
                    assert!(
                        r.detail.starts_with("health_timeout (not_registered"),
                        "{label}: {}",
                        r.detail
                    );
                    assert_eq!(env.installed(), "0.9.0", "{label}: 回滾換回的是舊版");
                    assert_eq!(env.prev(), "0.9.0", "{label}: pulse.prev");
                    assert_eq!(&env.result(), &r, "{label}: result.json");
                }
                assert!(!env.active(), "{label}: 指標已清除");
                assert!(
                    !after.calls().iter().any(|c| c == "tar" || c == "extract"),
                    "{label}: 接續時不重新解開發佈檔：{:?}",
                    after.calls()
                );
                env.cleanup();
                cases += 1;
            }
        }
    }
    assert_eq!(cases, 16);
}

/// 中斷兩次（替換後、還原後）仍然回到舊版
#[test]
fn two_crashes_in_one_transaction_still_end_with_the_old_version() {
    let mut env = setup("fm-twice", "tar");
    let fake = Fake::new(&env.root);
    fake.set("0.9.1", Behavior::Crash);
    apply_until(&env.paths, &fake, "binary_replaced");
    env.helper_is_installed();
    let second = fake.reboot();
    apply_until(&env.paths, &second, "rollback_restored");
    assert_eq!(env.installed(), "0.9.0");
    env.helper_is_installed();
    let third = second.reboot();
    let r = apply(&env.paths, &third).unwrap();
    assert_eq!(r.state, "rolled_back", "{r:?}");
    assert!(r.detail.contains("; resumed"), "{}", r.detail);
    assert_eq!(
        (
            env.installed().as_str(),
            env.prev().as_str(),
            env.unit().as_str()
        ),
        ("0.9.0", "0.9.0", OLD_UNIT)
    );
    assert!(!env.active());
    second.assert_invariants();
    third.assert_invariants();
    env.cleanup();
}

/// 只有輔助程式被終止（OOM、人工 `kill` 等，很快就被重新觸發）而主機沒有重新開機：服務仍是替換前啟動的行程。接續
/// 時還沒送出的 `systemctl restart` 要再送一次，否則健康確認看到的永遠是舊的行程
#[test]
fn a_helper_killed_without_a_reboot_restarts_the_service_on_resume() {
    let mut env = setup("fm-killed", "tar");
    let fake = Fake::new(&env.root);
    apply_until(&env.paths, &fake, "record_replaced");
    env.helper_is_installed();
    let after = fake.helper_killed();
    let r = apply(&env.paths, &after).unwrap();
    assert_eq!(r.state, "succeeded", "{r:?}");
    assert_eq!(after.calls(), ["restart"]);
    env.cleanup();

    // 已經送出過（健康確認中被終止）：不再重新啟動
    let mut env = setup("fm-killed-health", "tar");
    let fake = Fake::new(&env.root);
    apply_until(&env.paths, &fake, "health_wait");
    env.helper_is_installed();
    let after = fake.helper_killed();
    let r = apply(&env.paths, &after).unwrap();
    assert_eq!(r.state, "succeeded", "{r:?}");
    assert!(after.calls().is_empty(), "{:?}", after.calls());
    env.cleanup();

    // 回滾：已還原、還沒重新啟動就被終止——服務仍是崩潰中的新版行程
    let mut env = setup("fm-killed-rollback", "tar");
    let fake = Fake::new(&env.root);
    fake.set("0.9.1", Behavior::Crash);
    apply_until(&env.paths, &fake, "record_rolling_back");
    env.helper_is_installed();
    let after = fake.helper_killed();
    let r = apply(&env.paths, &after).unwrap();
    assert_eq!(r.state, "rolled_back", "{r:?}");
    assert_eq!(after.calls(), ["restart"]);
    env.cleanup();
}

/// 接續時 `request.json` 讀不到、不合法或不見了：有指標就只依交易紀錄接續
#[test]
fn a_transaction_is_resumed_without_a_readable_request() {
    for (i, damage) in ["garbage", "missing"].into_iter().enumerate() {
        let mut env = setup(&format!("fm-noreq{i}"), "tar");
        let fake = Fake::new(&env.root);
        apply_until(&env.paths, &fake, "health_wait");
        if damage == "garbage" {
            std::fs::write(&env.paths.request, "{ not json").unwrap();
        } else {
            std::fs::remove_file(&env.paths.request).unwrap();
        }
        env.helper_is_installed();
        let after = fake.reboot();
        let r = apply(&env.paths, &after).unwrap();
        assert_eq!(
            (r.state.as_str(), r.upgrade_id.as_str(), r.detail.as_str()),
            ("succeeded", ID, NOTE_REQUEST_UNREADABLE),
            "{damage}: {r:?}"
        );
        assert_eq!(
            (r.from_version.as_str(), r.to_version.as_str()),
            ("0.9.0", "0.9.1")
        );
        assert!(!env.paths.request.exists() && !env.active());
        assert_eq!(env.result(), r);
        env.cleanup();
    }
}

/// 替換無法完成（REMOTE.md 第 17.9.3 節）：系統沒被改過 → `failed`、不重新啟動；已被改過 → 換回
/// `pulse.prev` 並確認舊版恢復，結果是 `failed`（`restored`）或 `rollback_failed`——不會有「回報 failed 但
/// 系統停在替換到一半」的結果
#[test]
fn a_replacement_that_cannot_complete_fails_untouched_or_restores_the_old_version() {
    // 套件管理員一開始就失敗：什麼都沒變
    let env = setup("fm-pkg-fails", "deb");
    let fake = Fake::new(&env.root);
    fake.pkg_fails.set(true);
    let r = apply(&env.paths, &fake).unwrap();
    assert_eq!(
        (r.state.as_str(), r.code.as_deref()),
        ("failed", Some(codes::E_UPGRADE_REPLACE)),
        "{r:?}"
    );
    assert!(r.detail.contains("simulated failure"), "{}", r.detail);
    assert!(!r.detail.contains(DETAIL_RESTORED), "{}", r.detail);
    assert!(!fake.calls().contains(&"restart".to_string()));
    assert_eq!(env.installed(), "0.9.0");
    assert_eq!(pkg_db(&fake), "0.9.0 ii");
    assert!(!env.active());
    assert_eq!(env.record().step, Step::Done);
    fake.assert_invariants();
    env.cleanup();

    // 套件管理員執行到一半被中斷（執行檔已是新版、套件資料庫半安裝），接續時仍然失敗：換回 pulse.prev
    // （有驗證過的回滾套件也一樣：替換失敗一律換回執行檔）
    for (i, old_recovers) in [true, false].into_iter().enumerate() {
        let mut env = setup(&format!("fm-pkg-half{i}"), "deb");
        stage_rollback(&env, "0.9.0");
        let fake = Fake::new(&env.root);
        apply_until(&env.paths, &fake, "package_running");
        env.helper_is_installed();
        let after = fake.reboot();
        after.pkg_fails.set(true);
        if !old_recovers {
            after.set("0.9.0", Behavior::NoConnect);
        }
        let r = apply(&env.paths, &after).unwrap();
        assert_eq!(env.installed(), "0.9.0", "已換回舊版：{r:?}");
        assert_eq!(r.from_version, "0.9.0");
        let calls = after.calls();
        assert!(
            calls.contains(&"restart".to_string())
                && !calls.contains(&"dpkg rollback.pkg".to_string()),
            "{calls:?}"
        );
        let intent = env.record().rollback_intent.unwrap();
        assert_eq!(
            (intent.cause, intent.via),
            (Cause::ReplaceFailed, Via::Binary)
        );
        if old_recovers {
            assert_eq!(
                (r.state.as_str(), r.code.as_deref()),
                ("failed", Some(codes::E_UPGRADE_REPLACE)),
                "{r:?}"
            );
            assert!(
                r.detail.starts_with("replace_failed: ")
                    && r.detail.contains("package_db_shows_0.9.1 iF")
                    && r.detail.ends_with(DETAIL_RESTORED),
                "{}",
                r.detail
            );
        } else {
            assert_eq!(
                (r.state.as_str(), r.code.as_deref()),
                ("rollback_failed", Some(codes::E_UPGRADE_ROLLBACK_FAILED)),
                "{r:?}"
            );
        }
        assert!(!env.active());
        after.assert_invariants();
        env.cleanup();
    }

    // 工作目錄裡驗證過的複本在中斷期間被換掉：不使用（系統還沒被改過 → failed）
    let env = setup("fm-copy-changed", "tar");
    let fake = Fake::new(&env.root);
    apply_until(&env.paths, &fake, "pointer_written");
    std::fs::write(env.work().join(txn::NEW_BINARY_FILE), "VERSION=evil\n").unwrap();
    let after = fake.reboot();
    let r = apply(&env.paths, &after).unwrap();
    assert_eq!(
        (r.state.as_str(), r.code.as_deref()),
        ("failed", Some(codes::E_UPGRADE_REPLACE)),
        "{r:?}"
    );
    assert!(r.detail.starts_with(DETAIL_COPY_CHANGED), "{}", r.detail);
    assert_eq!(env.installed(), "0.9.0");
    assert!(after.calls().is_empty(), "{:?}", after.calls());
    assert!(!env.active());
    env.cleanup();
}

/// 回滾只使用與紀錄的雜湊相符的複本：`rollback.pkg`、`pulse.prev` 在中斷期間被換掉時不安裝它，
/// 結果是 `rollback_failed`（由人處理）
#[test]
fn rollback_only_uses_copies_that_match_the_recorded_hashes() {
    let mut env = setup("fm-rb-pkg-changed", "deb");
    stage_rollback(&env, "0.9.0");
    let fake = Fake::new(&env.root);
    fake.set("0.9.1", Behavior::Crash);
    apply_until(&env.paths, &fake, "rollback_intent_recorded");
    std::fs::write(env.work().join(txn::ROLLBACK_PKG_FILE), "VERSION=0.8.0\n").unwrap();
    env.helper_is_installed();
    let after = fake.reboot();
    let r = apply(&env.paths, &after).unwrap();
    assert_eq!(
        (r.state.as_str(), r.code.as_deref()),
        ("rollback_failed", Some(codes::E_UPGRADE_ROLLBACK_FAILED)),
        "{r:?}"
    );
    assert!(r.detail.contains(DETAIL_COPY_CHANGED), "{}", r.detail);
    assert!(
        !after.calls().iter().any(|c| c.starts_with("dpkg")),
        "{:?}",
        after.calls()
    );
    assert_eq!(env.installed(), "0.9.1", "沒有安裝被換掉的套件");
    env.cleanup();

    let mut env = setup("fm-rb-prev-changed", "tar");
    let fake = Fake::new(&env.root);
    fake.set("0.9.1", Behavior::Crash);
    apply_until(&env.paths, &fake, "rollback_intent_recorded");
    std::fs::write(env.paths.install_dir.join("pulse.prev"), "VERSION=0.8.0\n").unwrap();
    env.helper_is_installed();
    let after = fake.reboot();
    let r = apply(&env.paths, &after).unwrap();
    assert_eq!(r.state, "rollback_failed", "{r:?}");
    assert!(r.detail.contains("pulse.prev"), "{}", r.detail);
    assert_eq!(env.installed(), "0.9.1", "沒有換上不是替換前的執行檔");
    env.cleanup();
}

/// 結果（`done`）寫不進紀錄、連刪除紀錄都失敗時：指標照常清除，留下的未結束紀錄讓之後的請求以
/// `state_unusable` 擋下——不會在期限早已過去之後「接續」並回滾一個其實已完成的升級
#[test]
fn a_result_that_cannot_be_recorded_blocks_later_requests_instead_of_being_resumed() {
    if running_as_root() {
        return;
    }
    let mut env = setup("fm-done-unsaved", "tar");
    let fake = Fake::new(&env.root);
    let work = env.work();
    *fake.on_restart.borrow_mut() = Some(Box::new(move || chmod(&work, 0o500)));
    let r = apply(&env.paths, &fake).unwrap();
    chmod(&env.work(), 0o700);
    assert_eq!(r.state, "succeeded", "這次執行仍然做完：{r:?}");
    assert_eq!(env.result(), r);
    assert!(!env.active(), "指標已清除");
    assert_eq!(env.record().step, Step::Replaced, "紀錄停在最後一次寫入");
    // 之後的請求（重新開機很久以後）
    env.helper_is_installed();
    stage_upgrade(&env, OTHER_ID, "0.9.1", "0.9.2");
    let later = Fake::new(&env.root).reboot_after(86_400);
    let r = apply(&env.paths, &later).unwrap();
    assert_eq!(
        (r.state.as_str(), r.code.as_deref()),
        ("failed", Some(codes::E_UPGRADE_INTEGRITY)),
        "{r:?}"
    );
    assert!(
        r.detail.starts_with(DETAIL_STATE_UNUSABLE) && r.detail.contains(ID),
        "{}",
        r.detail
    );
    assert_eq!(env.installed(), "0.9.1", "沒有回滾已完成的升級");
    assert!(later.calls().is_empty(), "{:?}", later.calls());
    env.cleanup();
}

/// 現行行為（鎖住，不是期望的最終設計）：結果（`done`）寫不進紀錄，而且進行中交易的指標（`active.json`）
/// **也刪不掉**（例如整個檔案系統變成唯讀：`state.json` 與指標的 unlink 一起失敗）。這次執行仍然回報
/// 成功，但指標與停在 `replaced` 的紀錄都留著；之後的請求會**接續**這筆交易——期限早已過去，
/// `after_replace` 直接判 `health_timeout`，把一個其實已完成的升級回滾。上一個測試的保護（指標刪得掉時以
/// `state_unusable` 擋下）在這個情況不成立
#[test]
fn a_result_that_cannot_be_recorded_with_a_pointer_that_cannot_be_removed_is_resumed_later() {
    if running_as_root() {
        return;
    }
    let mut env = setup("fm-done-unsaved-pointer", "tar");
    let fake = Fake::new(&env.root);
    let (work, work_root) = (env.work(), env.paths.work_root.clone());
    // 重新啟動服務的那一刻起，root 的工作目錄（根目錄與這筆交易的）都不能再寫入或刪除檔案
    *fake.on_restart.borrow_mut() = Some(Box::new(move || {
        chmod(&work, 0o500);
        chmod(&work_root, 0o500);
    }));
    let r = apply(&env.paths, &fake).unwrap();
    chmod(&env.paths.work_root, 0o700);
    chmod(&env.work(), 0o700);
    assert_eq!(r.state, "succeeded", "這次執行仍然做完：{r:?}");
    assert_eq!(env.result(), r);
    assert!(!env.paths.request.exists());
    assert_eq!(env.installed(), "0.9.1");
    assert!(env.active(), "指標刪不掉，留著");
    let rec = env.record();
    assert_eq!(rec.step, Step::Replaced, "紀錄停在最後一次寫入");
    assert!(rec.result.is_none());

    // 之後的請求（檔案系統恢復可寫、一天以後；另一筆升級）：接續留下的交易，而不是被擋下
    env.helper_is_installed();
    stage_upgrade(&env, OTHER_ID, "0.9.1", "0.9.2");
    let later = Fake::new(&env.root).reboot_after(86_400);
    let r = apply(&env.paths, &later).unwrap();
    assert_eq!(
        (r.state.as_str(), r.upgrade_id.as_str(), r.code.as_deref()),
        ("rolled_back", ID, Some(codes::W_UPGRADE_ROLLED_BACK)),
        "{r:?}"
    );
    assert!(
        r.detail.starts_with("health_timeout")
            && !r.detail.contains("health_timeout (")
            && r.detail.contains(NOTE_UPGRADE_ID_IGNORED),
        "{}",
        r.detail
    );
    assert_eq!(
        (r.from_version.as_str(), r.to_version.as_str()),
        ("0.9.0", "0.9.1")
    );
    assert_eq!(env.installed(), "0.9.0", "已完成的升級被回滾");
    assert!(
        later.calls().contains(&"restart".to_string()),
        "{:?}",
        later.calls()
    );
    assert!(!env.active(), "這次交易結束，指標清除");
    assert_eq!(env.record().step, Step::Done);
    assert!(
        !env.paths.work_root.join(OTHER_ID).exists(),
        "新的請求沒有開始"
    );
    env.cleanup();
}

// ---- 第 17.9.5 節：紀錄或指標損毀、遺失 -------------------------------------------

fn assert_stopped(env: &Env, fake: &Fake, r: &UpgradeResult, detail: &str, label: &str) {
    assert_eq!(
        (r.state.as_str(), r.code.as_deref()),
        ("failed", Some(codes::E_UPGRADE_INTEGRITY)),
        "{label}: {r:?}"
    );
    assert!(
        r.detail.starts_with(detail) && r.detail.ends_with("manual check required"),
        "{label}: {}",
        r.detail
    );
    // 不替換、不回滾、不重新啟動
    assert!(fake.calls().is_empty(), "{label}: {:?}", fake.calls());
    assert!(!env.paths.request.exists(), "{label}: 不再觸發");
    assert_eq!(&env.result(), r, "{label}");
}

/// 交易紀錄以 `f` 改寫後重新寫入（checksum 正確）
fn edit_record(env: &Env, f: impl FnOnce(&mut Record)) {
    let mut r = env.record();
    f(&mut r);
    r.save(&env.work()).unwrap();
}

/// 改寫檔案的文字；`reseal` 時重新計算 checksum
fn edit_text(path: &Path, reseal: bool, f: impl FnOnce(String) -> String) {
    let text = f(std::fs::read_to_string(path).unwrap());
    let text = if reseal { txn::reseal(&text) } else { text };
    std::fs::write(path, text).unwrap();
}

#[test]
fn an_unusable_pointer_stops_everything_and_is_kept() {
    type Damage = Box<dyn Fn(&Path)>;
    let cases: Vec<(&str, Damage)> = vec![
        (
            "garbage",
            Box::new(|p| std::fs::write(p, "{ not json").unwrap()),
        ),
        (
            "checksum",
            Box::new(|p| edit_text(p, false, |t| t.replace(ID, OTHER_ID))),
        ),
        (
            "schema",
            Box::new(|p| {
                edit_text(p, true, |t| {
                    t.replace("upgrade-active/v1", "upgrade-active/v9")
                })
            }),
        ),
        (
            "truncated",
            Box::new(|p| {
                let t = std::fs::read(p).unwrap();
                std::fs::write(p, &t[..t.len() / 2]).unwrap();
            }),
        ),
        (
            "unreadable",
            Box::new(|p| {
                std::fs::remove_file(p).unwrap();
                std::fs::create_dir(p).unwrap();
            }),
        ),
    ];
    for (name, damage) in cases {
        let mut env = setup(&format!("fm-ptr-{name}"), "tar");
        let fake = Fake::new(&env.root);
        apply_until(&env.paths, &fake, "binary_replaced");
        let pointer = env.paths.work_root.join(ACTIVE_FILE);
        damage(&pointer);
        env.helper_is_installed();
        let after = fake.reboot();
        let r = apply(&env.paths, &after).unwrap();
        assert_stopped(&env, &after, &r, DETAIL_POINTER_UNUSABLE, name);
        assert!(pointer.exists(), "{name}: 指標保留");
        assert_eq!(
            (env.installed().as_str(), env.prev().as_str()),
            ("0.9.1", "0.9.0"),
            "{name}: 現場保留"
        );
        assert_eq!(env.record().step, Step::Replacing, "{name}");
        env.cleanup();
    }
}

#[test]
fn a_pointer_without_a_usable_record_stops_everything_and_is_kept() {
    type Damage = Box<dyn Fn(&Env)>;
    let state = |env: &Env| env.work().join(txn::STATE_FILE);
    let cases: Vec<(&str, Damage)> = vec![
        (
            "missing",
            Box::new(move |e| std::fs::remove_file(state(e)).unwrap()),
        ),
        (
            "garbage",
            Box::new(move |e| std::fs::write(state(e), "{ not json").unwrap()),
        ),
        (
            "checksum",
            Box::new(move |e| {
                edit_text(&state(e), false, |t| t.replace("\"tar\"", "\"deb\""));
            }),
        ),
        (
            "schema",
            Box::new(move |e| {
                edit_text(&state(e), true, |t| {
                    t.replace("upgrade-helper/v2", "upgrade-helper/v9")
                });
            }),
        ),
        (
            "legacy",
            Box::new(move |e| {
                std::fs::write(
                    state(e),
                    r#"{"schema":"pulse.upgrade-helper/v1","step":"replaced"}"#,
                )
                .unwrap();
            }),
        ),
        (
            "other-id",
            Box::new(|e| edit_record(e, |r| r.upgrade_id = OTHER_ID.into())),
        ),
        (
            "binding",
            Box::new(|e| edit_record(e, |r| r.binding_sha256 = sha(b"something else"))),
        ),
        (
            "preparing",
            Box::new(|e| edit_record(e, |r| r.step = Step::Preparing)),
        ),
        (
            "done-without-result",
            Box::new(|e| edit_record(e, |r| r.step = Step::Done)),
        ),
        (
            "symlink",
            Box::new(move |e| {
                // 內容是可用的紀錄，但 state.json 被換成符號連結：以 O_NOFOLLOW 開啟，不跟隨
                let elsewhere = e.root.join("elsewhere.json");
                std::fs::rename(state(e), &elsewhere).unwrap();
                std::os::unix::fs::symlink(&elsewhere, state(e)).unwrap();
            }),
        ),
    ];
    for (name, damage) in cases {
        let mut env = setup(&format!("fm-rec-{name}"), "tar");
        let fake = Fake::new(&env.root);
        apply_until(&env.paths, &fake, "binary_replaced");
        damage(&env);
        env.helper_is_installed();
        // 第二次：之後的請求（另一個 upgrade_id）同樣被擋下，直到人工檢查
        for attempt in 0..2 {
            if attempt == 1 {
                stage_upgrade(&env, OTHER_ID, "0.9.1", "0.9.2");
            }
            let after = fake.reboot();
            let r = apply(&env.paths, &after).unwrap();
            assert_stopped(&env, &after, &r, DETAIL_STATE_UNUSABLE, name);
            assert_eq!(r.upgrade_id, ID, "{name}: 回報的是指標指向的交易");
            assert!(env.active(), "{name}: 指標保留");
            assert_eq!(
                (env.installed().as_str(), env.prev().as_str()),
                ("0.9.1", "0.9.0"),
                "{name}: 現場保留"
            );
            assert!(!env.paths.work_root.join(OTHER_ID).exists(), "{name}");
        }
        env.cleanup();
    }
}

/// 指標遺失：紀錄停在已動手的步驟時不得繼續，也不得當成全新的請求
#[test]
fn an_unfinished_record_without_its_pointer_is_not_resumed_or_restarted() {
    for (point, step, other_request) in [
        ("binary_replaced", "replacing", false),
        ("health_wait", "replaced", true),
        ("rollback_intent_recorded", "rollback_intent", false),
        ("rollback_wait", "rolling_back", true),
    ] {
        let mut env = setup(&format!("fm-noptr-{point}"), "deb");
        stage_rollback(&env, "0.9.0");
        let fake = Fake::new(&env.root);
        fake.set("0.9.1", Behavior::Crash);
        apply_until(&env.paths, &fake, point);
        std::fs::remove_file(env.paths.work_root.join(ACTIVE_FILE)).unwrap();
        let installed = env.installed();
        env.helper_is_installed();
        if other_request {
            // 服務帳號另外寫了一個「誠實」的新請求
            stage_upgrade(&env, OTHER_ID, &installed, "0.9.2");
        }
        let after = fake.reboot();
        let r = apply(&env.paths, &after).unwrap();
        assert_stopped(&env, &after, &r, DETAIL_STATE_UNUSABLE, point);
        assert!(
            r.detail.contains(ID) && r.detail.contains(&format!("step {step} ")),
            "{point}: {}",
            r.detail
        );
        assert_eq!(env.installed(), installed, "{point}: 現場保留");
        assert_eq!(env.prev(), "0.9.0", "{point}");
        assert!(!env.active(), "{point}");
        assert!(!env.paths.work_root.join(OTHER_ID).exists(), "{point}");
        assert_eq!(env.record().step.name(), step, "{point}");
        env.cleanup();
    }
}

/// 沒有指標時，任何一個工作目錄的紀錄讀不到或 checksum 不符，都無法判斷那筆交易是否已結束
#[test]
fn a_damaged_record_in_any_work_directory_blocks_new_transactions() {
    for (i, valid_then_flipped) in [false, true].into_iter().enumerate() {
        let env = setup(&format!("fm-other-damaged{i}"), "tar");
        let other = env.paths.work_root.join(OTHER_ID);
        std::fs::create_dir_all(&other).unwrap();
        chmod(&env.paths.work_root, 0o700);
        chmod(&other, 0o700);
        if valid_then_flipped {
            let mut rec = Record::new(OTHER_ID, "0.8.0", "0000000", "tar");
            rec.save(&other).unwrap();
            edit_text(&other.join(txn::STATE_FILE), false, |t| {
                t.replace("\"preparing\"", "\"prepared\"")
            });
        } else {
            std::fs::write(other.join(txn::STATE_FILE), "\0\0\0").unwrap();
        }
        let fake = Fake::new(&env.root);
        let r = apply(&env.paths, &fake).unwrap();
        assert_stopped(&env, &fake, &r, DETAIL_STATE_UNUSABLE, "other");
        assert!(r.detail.contains(OTHER_ID), "{}", r.detail);
        assert_eq!(env.installed(), "0.9.0");
        assert!(other.join(txn::STATE_FILE).exists(), "現場保留");
        assert!(!env.work().exists(), "沒有為請求建立工作目錄");
        env.cleanup();
    }
}

/// 沒有指標的 `preparing`／`prepared`：系統仍是紀錄的替換前狀態才重來；紀錄說還沒動手、系統卻已不同時
/// 不重來
#[test]
fn a_prepared_record_that_contradicts_the_system_is_not_restarted() {
    // 執行檔已不同
    let env = setup("fm-prep-binary", "deb");
    let fake = Fake::new(&env.root);
    apply_until(&env.paths, &fake, "record_prepared");
    std::fs::write(
        env.paths.install_dir.join("pulse"),
        "VERSION=0.9.0\n# changed\n",
    )
    .unwrap();
    let after = fake.reboot();
    let r = apply(&env.paths, &after).unwrap();
    assert_stopped(&env, &after, &r, DETAIL_STATE_UNUSABLE, "binary");
    assert!(
        r.detail.contains("installed binary has changed"),
        "{}",
        r.detail
    );
    env.cleanup();

    // 套件資料庫已不同
    let env = setup("fm-prep-db", "deb");
    let fake = Fake::new(&env.root);
    apply_until(&env.paths, &fake, "record_preparing");
    let after = fake.reboot();
    *after.pkg_db.borrow_mut() = Some("0.9.1 iF".into());
    let r = apply(&env.paths, &after).unwrap();
    assert_stopped(&env, &after, &r, DETAIL_STATE_UNUSABLE, "db");
    assert!(
        r.detail.contains("package database has changed"),
        "{}",
        r.detail
    );
    assert_eq!(env.installed(), "0.9.0");
    env.cleanup();

    // 輔助程式已是另一個版本（那筆紀錄屬於更早、已無關的嘗試；例如中斷後管理者手動升級）：照全新的請求處理
    let mut env = setup("fm-prep-other-version", "tar");
    let fake = Fake::new(&env.root);
    apply_until(&env.paths, &fake, "record_prepared");
    std::fs::write(env.paths.install_dir.join("pulse"), "VERSION=0.9.1\n").unwrap();
    env.helper_is_installed();
    stage_upgrade(&env, ID, "0.9.1", "0.9.2");
    let next = Fake::new(&env.root);
    let r = apply(&env.paths, &next).unwrap();
    assert_eq!(
        (
            r.state.as_str(),
            r.from_version.as_str(),
            r.to_version.as_str()
        ),
        ("succeeded", "0.9.1", "0.9.2"),
        "{r:?}"
    );
    assert_eq!(env.installed(), "0.9.2");
    assert_eq!(env.record().from_version, "0.9.1");
    next.assert_invariants();
    env.cleanup();
}

/// 已結束的交易：同一個 `upgrade_id` 的請求再次出現時回報記下的結果，不再做任何事
#[test]
fn a_finished_transaction_reports_its_recorded_result_again() {
    let mut env = setup("fm-done-again", "tar");
    let first = apply(&env.paths, &Fake::new(&env.root)).unwrap();
    assert_eq!(first.state, "succeeded");
    std::fs::remove_file(env.root.join("var/lib/pulse/upgrade/result.json")).unwrap();
    env.helper_is_installed();
    stage_upgrade(&env, ID, "0.9.0", "0.9.1");
    let fake = Fake::new(&env.root);
    let again = apply(&env.paths, &fake).unwrap();
    assert_eq!(again, first, "同一份結果（含完成時間）");
    assert_eq!(env.result(), first);
    assert!(fake.calls().is_empty(), "{:?}", fake.calls());
    assert!(!env.paths.request.exists() && !env.active());
    assert_eq!(env.installed(), "0.9.1");
    env.cleanup();
}

/// 紀錄與指標整個消失（`/var/lib/pulse-upgrade` 被清空）：沒有痕跡可查，視為沒有進行中的交易；執行檔
/// 已是新版時，原本的請求因 `from_version` 不符被拒絕
#[test]
fn everything_gone_is_treated_as_no_transaction() {
    let mut env = setup("fm-gone", "tar");
    let fake = Fake::new(&env.root);
    apply_until(&env.paths, &fake, "binary_replaced");
    std::fs::remove_dir_all(&env.paths.work_root).unwrap();
    env.helper_is_installed();
    let after = fake.reboot();
    let r = apply(&env.paths, &after).unwrap();
    assert_eq!(
        (r.state.as_str(), r.code.as_deref()),
        ("failed", Some(codes::E_UPGRADE_INTEGRITY)),
        "{r:?}"
    );
    assert!(
        r.detail.starts_with(DETAIL_FROM_VERSION_MISMATCH),
        "{}",
        r.detail
    );
    assert_eq!(
        (env.installed().as_str(), env.prev().as_str()),
        ("0.9.1", "0.9.0")
    );
    assert!(after.calls().is_empty(), "{:?}", after.calls());
    env.cleanup();
}

// ---- 第 17.9.6 節：舊格式（v1） --------------------------------------------------

fn write_legacy(env: &Env, id: &str, state: serde_json::Value) {
    let dir = env.paths.work_root.join(id);
    std::fs::create_dir_all(&dir).unwrap();
    chmod(&env.paths.work_root, 0o700);
    chmod(&dir, 0o700);
    std::fs::write(
        dir.join(txn::STATE_FILE),
        serde_json::to_vec(&state).unwrap(),
    )
    .unwrap();
}

/// 0.9.0 的輔助程式寫的狀態檔（沒有版本、安裝方式、目標）
fn legacy_state(step: &str, deadline_in: i64) -> serde_json::Value {
    let now = chrono::Utc::now().timestamp();
    serde_json::json!({
        "schema": txn::LEGACY_SCHEMA,
        "step": step,
        "deadline_unix": now + deadline_in,
        "n_restarts_before": 0,
        "restarted_unix": now - 5,
        "unit_replaced": false,
        "notes": [],
        "result": null,
    })
}

/// 由舊版輔助程式替換完成的主機：執行檔已是 0.9.1、`pulse.prev` 是 0.9.0，接續的輔助程式是 0.9.1
fn legacy_replaced_host(name: &str, method: &str) -> Env {
    let mut env = setup(name, method);
    std::fs::write(env.paths.install_dir.join("pulse"), "VERSION=0.9.1\n").unwrap();
    std::fs::write(env.paths.install_dir.join("pulse.prev"), "VERSION=0.9.0\n").unwrap();
    chmod(&env.paths.install_dir.join("pulse.prev"), 0o755);
    env.helper_is_installed();
    env
}

#[test]
fn a_legacy_transaction_is_resumed_only_after_the_replacement() {
    // replaced：轉成 v2 的 replaced，接續健康確認
    let env = legacy_replaced_host("fm-v1-replaced", "deb");
    write_legacy(&env, ID, legacy_state("replaced", 600));
    let fake = Fake::new(&env.root);
    let r = apply(&env.paths, &fake).unwrap();
    assert_eq!(
        (
            r.state.as_str(),
            r.from_version.as_str(),
            r.to_version.as_str()
        ),
        ("succeeded", "0.9.0", "0.9.1"),
        "v1 沒有記版本：沿用請求中的值：{r:?}"
    );
    assert!(fake.calls().is_empty(), "{:?}", fake.calls());
    let rec = env.record();
    assert!(rec.legacy, "轉換後是 v2 的紀錄");
    assert_eq!(
        (rec.schema.as_str(), rec.step),
        (txn::RECORD_SCHEMA, Step::Done)
    );
    assert!(!env.active(), "轉換時寫入的指標在結束時清除");
    fake.assert_invariants();
    env.cleanup();

    // replaced、新版崩潰：照 v1 的行為回滾（沒有雜湊可比對；tar 還原 unit）
    let env = legacy_replaced_host("fm-v1-replaced-crash", "tar");
    let mut state = legacy_state("replaced", 600);
    state["unit_replaced"] = true.into();
    write_legacy(&env, ID, state);
    std::fs::write(env.work().join(txn::UNIT_BACKUP_FILE), OLD_UNIT).unwrap();
    std::fs::write(&env.paths.unit_path, NEW_UNIT).unwrap();
    let fake = Fake::new(&env.root);
    fake.set("0.9.1", Behavior::Crash);
    let r = apply(&env.paths, &fake).unwrap();
    assert_eq!(r.state, "rolled_back", "{r:?}");
    assert!(r.detail.starts_with("crash_restart"), "{}", r.detail);
    assert_eq!(
        (env.installed().as_str(), env.unit().as_str()),
        ("0.9.0", OLD_UNIT)
    );
    env.cleanup();

    // 請求被換成另一個 upgrade_id：恰好一筆未結束的 v1 → 接續那一筆
    let env = legacy_replaced_host("fm-v1-other-id", "tar");
    write_legacy(&env, ID, legacy_state("replaced", 600));
    rewrite_request(&env, |r| r.upgrade_id = OTHER_ID.into());
    let r = apply(&env.paths, &Fake::new(&env.root)).unwrap();
    assert_eq!(
        (r.state.as_str(), r.upgrade_id.as_str()),
        ("succeeded", ID),
        "{r:?}"
    );
    assert!(r.detail.contains(NOTE_UPGRADE_ID_IGNORED), "{}", r.detail);
    assert!(!env.paths.work_root.join(OTHER_ID).exists());
    env.cleanup();

    // rolling_back：0.9.0 沒有記回滾的原因 → 只回報 resumed；0.10.0 開發期間的版本有記
    for (i, (reason, detail)) in [
        (None, "resumed"),
        (
            Some("crash_restart (NRestarts 0 → 1)"),
            "crash_restart (NRestarts 0 → 1); resumed",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let env = setup(&format!("fm-v1-rolling-back{i}"), "tar");
        let mut state = legacy_state("rolling_back", 600);
        if let Some(reason) = reason {
            state["rollback_reason"] = reason.into();
        }
        write_legacy(&env, ID, state);
        let fake = Fake::new(&env.root);
        let r = apply(&env.paths, &fake).unwrap();
        assert_eq!(
            (r.state.as_str(), r.detail.as_str()),
            ("rolled_back", detail),
            "{r:?}"
        );
        assert!(fake.calls().is_empty(), "{:?}", fake.calls());
        assert!(!env.active());
        env.cleanup();
    }
}

/// v1 的 `verified` 涵蓋「尚未替換」到「已替換、尚未記錄」：以輔助程式自己的版本判斷
#[test]
fn a_legacy_verified_transaction_is_judged_by_the_running_helper_version() {
    let verified = |env: &Env| {
        let mut state = legacy_state("verified", 0);
        state["from_version"] = "0.9.0".into();
        state["method"] = env.paths.method.clone().into();
        state["target"] = serde_json::json!({"version": "0.9.1", "commit": "5e6f7a8"});
        state["health_timeout_secs"] = 600.into();
        state["prev_saved"] = true.into();
        write_legacy(env, ID, state);
    };
    // 輔助程式已是目標版本：執行檔已替換、還沒重新啟動 → 當成 replaced，重新啟動並重新起算期限
    let env = legacy_replaced_host("fm-v1-verified-new", "tar");
    verified(&env);
    // 請求中的目標被改過：以紀錄為準
    rewrite_request(&env, |r| r.target.version = "0.9.9".into());
    let fake = Fake::new(&env.root);
    let r = apply(&env.paths, &fake).unwrap();
    assert_eq!(
        (
            r.state.as_str(),
            r.from_version.as_str(),
            r.to_version.as_str()
        ),
        ("succeeded", "0.9.0", "0.9.1"),
        "{r:?}"
    );
    assert!(
        r.detail.contains(NOTE_REQUEST_FIELDS_IGNORED),
        "{}",
        r.detail
    );
    assert_eq!(fake.calls(), ["restart"], "不重新替換，只重新啟動");
    assert_eq!(env.prev(), "0.9.0");
    env.cleanup();

    // 輔助程式不是目標版本：無法判斷替換到哪裡，不重新替換
    let env = setup("fm-v1-verified-old", "tar");
    verified(&env);
    let fake = Fake::new(&env.root);
    let r = apply(&env.paths, &fake).unwrap();
    assert_stopped(&env, &fake, &r, DETAIL_STATE_UNUSABLE, "verified");
    assert!(r.detail.contains("verified"), "{}", r.detail);
    assert_eq!(env.installed(), "0.9.0");
    assert_eq!(env.prev(), "");
    assert!(!env.active(), "沒有轉換、沒有寫入指標");
    assert!(matches!(txn::load_record(&env.work()), Loaded::Legacy(_)));
    env.cleanup();
}

#[test]
fn more_than_one_unfinished_legacy_transaction_is_not_guessed() {
    let env = legacy_replaced_host("fm-v1-two", "tar");
    write_legacy(&env, ID, legacy_state("replaced", 600));
    write_legacy(&env, OTHER_ID, legacy_state("rolling_back", 600));
    let fake = Fake::new(&env.root);
    let r = apply(&env.paths, &fake).unwrap();
    assert_stopped(&env, &fake, &r, DETAIL_STATE_UNUSABLE, "two");
    assert!(
        r.detail.contains("more than one") && r.detail.contains(ID) && r.detail.contains(OTHER_ID),
        "{}",
        r.detail
    );
    assert_eq!(env.installed(), "0.9.1");
    env.cleanup();

    // 已結束的 v1 紀錄：同一個 upgrade_id 的請求回報記下的結果
    let env = setup("fm-v1-done", "tar");
    let recorded = UpgradeResult {
        schema: files::RESULT_SCHEMA.into(),
        upgrade_id: ID.into(),
        state: "rolled_back".into(),
        code: Some(codes::W_UPGRADE_ROLLED_BACK.into()),
        detail: "crash_restart".into(),
        from_version: "0.9.0".into(),
        to_version: "0.9.1".into(),
        finished_at: "2026-10-21T00:00:00Z".into(),
    };
    let mut state = legacy_state("done", 0);
    state["result"] = serde_json::to_value(&recorded).unwrap();
    write_legacy(&env, ID, state);
    let fake = Fake::new(&env.root);
    let r = apply(&env.paths, &fake).unwrap();
    assert_eq!(r, recorded);
    assert!(fake.calls().is_empty(), "{:?}", fake.calls());
    assert_eq!(env.installed(), "0.9.0");
    env.cleanup();
}

// ---- 第 17.9.7 節：路徑檢查 ------------------------------------------------------

/// Pulse 在 hello 之前的預檢（`install.upgrade = "unsafe_path"`）對這個環境的結果
fn precheck(env: &Env) -> Result<(), String> {
    precheck_paths(
        &env.paths.install_dir,
        &env.paths.work_root,
        env.paths.require_root_owner,
    )
}

/// 這次請求因路徑不安全被拒絕，沒有任何變更。預檢（[`precheck_paths`]）對同一個環境回報**同一個**路徑與
/// 原因——兩者共用同一套檢查；例外是每筆交易的工作目錄（`work-` 開頭的情境），預檢不涵蓋
fn refused_as_unsafe(env: &Env, why: &str, label: &str) {
    let pre = precheck(env);
    let fake = Fake::new(&env.root);
    let r = apply(&env.paths, &fake).unwrap();
    if label.starts_with("work-") {
        assert_eq!(pre, Ok(()), "{label}: 預檢不涵蓋每筆交易的工作目錄");
    } else {
        assert_eq!(
            pre.map_err(|why| format!("{DETAIL_UNSAFE_PATH}: {why}")),
            Err(r.detail.clone()),
            "{label}: 預檢與輔助程式的結論相同"
        );
    }
    assert_eq!(
        (r.state.as_str(), r.code.as_deref()),
        ("failed", Some(codes::E_UPGRADE_PRECHECK)),
        "{label}: {r:?}"
    );
    assert!(
        r.detail.starts_with(DETAIL_UNSAFE_PATH) && r.detail.contains(why),
        "{label}: {}",
        r.detail
    );
    assert!(fake.calls().is_empty(), "{label}: {:?}", fake.calls());
    assert_eq!(env.installed(), "0.9.0", "{label}");
    assert_eq!(env.prev(), "", "{label}");
    assert!(!env.paths.request.exists(), "{label}");
    assert_eq!(env.result(), r, "{label}");
}

#[test]
fn the_work_directories_must_be_private_and_not_symbolic_links() {
    // 根目錄是符號連結
    let env = setup("fm-path-root-link", "tar");
    let elsewhere = env.root.join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    chmod(&elsewhere, 0o700);
    std::os::unix::fs::symlink(&elsewhere, &env.paths.work_root).unwrap();
    refused_as_unsafe(&env, "is a symbolic link", "root-link");
    assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0);
    env.cleanup();

    // 根目錄可由群組或其他人寫入
    for (i, mode) in [0o770, 0o702, 0o777].into_iter().enumerate() {
        let env = setup(&format!("fm-path-root-mode{i}"), "tar");
        std::fs::create_dir(&env.paths.work_root).unwrap();
        chmod(&env.paths.work_root, mode);
        refused_as_unsafe(&env, "writable by group or others", "root-mode");
        env.cleanup();
    }

    // 只多了讀取／執行權限：改回 0700 後照常執行
    let env = setup("fm-path-root-755", "tar");
    std::fs::create_dir(&env.paths.work_root).unwrap();
    chmod(&env.paths.work_root, 0o755);
    // 預檢：算通過（輔助程式會改回 0700），而且預檢自己不修改任何東西
    assert_eq!(precheck(&env), Ok(()));
    assert_eq!(
        std::fs::metadata(&env.paths.work_root)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o755
    );
    let r = apply(&env.paths, &Fake::new(&env.root)).unwrap();
    assert_eq!(r.state, "succeeded", "{r:?}");
    assert_eq!(
        std::fs::metadata(&env.paths.work_root)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert!(
        std::fs::read_to_string(env.work().join(txn::LOG_FILE))
            .unwrap()
            .contains("已改回 700"),
        "記 WARN"
    );
    env.cleanup();

    // 這筆交易的工作目錄是符號連結、或可由其他人寫入
    let env = setup("fm-path-work-link", "tar");
    std::fs::create_dir(&env.paths.work_root).unwrap();
    chmod(&env.paths.work_root, 0o700);
    let elsewhere = env.root.join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    chmod(&elsewhere, 0o700);
    std::os::unix::fs::symlink(&elsewhere, env.work()).unwrap();
    refused_as_unsafe(&env, "is a symbolic link", "work-link");
    assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0);
    env.cleanup();

    let env = setup("fm-path-work-mode", "tar");
    env.make_work();
    chmod(&env.work(), 0o777);
    refused_as_unsafe(&env, "writable by group or others", "work-mode");
    env.cleanup();

    // 上層目錄可由群組寫入；sticky 的目錄（例如 /tmp 那樣的 1777）除外
    let env = setup("fm-path-parent", "tar");
    chmod(&env.root.join("var/lib"), 0o775);
    refused_as_unsafe(&env, "writable by group or others", "parent");
    env.cleanup();
    let env = setup("fm-path-parent-sticky", "tar");
    chmod(&env.root.join("var/lib"), 0o1777);
    assert_eq!(precheck(&env), Ok(()));
    assert!(
        !env.paths.work_root.exists(),
        "預檢不建立工作目錄的根目錄（不存在算通過）"
    );
    let r = apply(&env.paths, &Fake::new(&env.root)).unwrap();
    assert_eq!(r.state, "succeeded", "{r:?}");
    env.cleanup();

    // 擁有者必須是 root（正式環境）
    if !running_as_root() {
        let mut env = setup("fm-path-owner", "tar");
        env.paths.require_root_owner = true;
        refused_as_unsafe(&env, "not root", "owner");
        env.cleanup();
    }
}

#[test]
fn the_install_directory_and_binary_must_be_safe() {
    // 安裝目錄是符號連結
    let env = setup("fm-path-install-link", "tar");
    let real = env.root.join("opt/real");
    std::fs::rename(&env.paths.install_dir, &real).unwrap();
    std::os::unix::fs::symlink(&real, &env.paths.install_dir).unwrap();
    refused_as_unsafe(&env, "is a symbolic link", "install-link");
    env.cleanup();

    // 安裝目錄可由群組寫入
    let env = setup("fm-path-install-mode", "tar");
    chmod(&env.paths.install_dir, 0o775);
    refused_as_unsafe(&env, "writable by group or others", "install-mode");
    env.cleanup();

    // 執行檔是符號連結、或可由群組寫入
    let env = setup("fm-path-binary-link", "tar");
    let bin = env.paths.install_dir.join("pulse");
    std::fs::rename(&bin, env.paths.install_dir.join("pulse.real")).unwrap();
    std::os::unix::fs::symlink("pulse.real", &bin).unwrap();
    refused_as_unsafe(&env, "not a regular file", "binary-link");
    env.cleanup();

    let env = setup("fm-path-binary-mode", "tar");
    chmod(&env.paths.install_dir.join("pulse"), 0o775);
    refused_as_unsafe(&env, "writable by group or others", "binary-mode");
    env.cleanup();

    // pulse.prev 是符號連結
    let env = setup("fm-path-prev-link", "tar");
    let fake = Fake::new(&env.root);
    std::os::unix::fs::symlink("pulse", env.paths.install_dir.join("pulse.prev")).unwrap();
    let pre = precheck(&env);
    let r = apply(&env.paths, &fake).unwrap();
    assert_eq!(r.code.as_deref(), Some(codes::E_UPGRADE_PRECHECK), "{r:?}");
    assert_eq!(
        pre.map_err(|why| format!("{DETAIL_UNSAFE_PATH}: {why}")),
        Err(r.detail.clone())
    );
    assert!(
        r.detail.starts_with(DETAIL_UNSAFE_PATH) && r.detail.contains("pulse.prev"),
        "{}",
        r.detail
    );
    assert!(fake.calls().is_empty());
    assert_eq!(env.installed(), "0.9.0");
    env.cleanup();
}

/// 預檢由服務帳號（不是 root）執行：因為權限不足而看不到的部分略過，不回報不安全；看得到的部分照常檢查
#[test]
fn the_precheck_skips_what_the_service_account_cannot_see() {
    if running_as_root() {
        return;
    }
    // 安裝目錄進不去（`lstat /opt/pulse/pulse` 得到 EACCES）：略過安裝目錄的其餘檢查
    let env = setup("fm-pre-denied-install", "tar");
    chmod(&env.paths.install_dir.join("pulse"), 0o775);
    assert!(precheck(&env).is_err(), "看得到時：執行檔群組可寫");
    chmod(&env.paths.install_dir, 0o000);
    assert_eq!(precheck(&env), Ok(()), "看不到執行檔：不誤報");
    // 工作目錄的根目錄是獨立的一部分，仍然檢查
    std::fs::create_dir(&env.paths.work_root).unwrap();
    chmod(&env.paths.work_root, 0o777);
    let why = precheck(&env).unwrap_err();
    assert!(
        why.contains("pulse-upgrade") && why.contains("writable by group or others"),
        "{why}"
    );
    chmod(&env.paths.install_dir, 0o755);
    env.cleanup();

    // 工作目錄的上層目錄進不去（`lstat /var/lib/pulse-upgrade` 得到 EACCES）：略過，不誤報
    let env = setup("fm-pre-denied-work", "tar");
    std::fs::create_dir(&env.paths.work_root).unwrap();
    chmod(&env.paths.work_root, 0o777);
    assert!(precheck(&env).is_err(), "看得到時：根目錄其他人可寫");
    chmod(&env.root.join("var/lib"), 0o000);
    assert_eq!(precheck(&env), Ok(()), "看不到工作目錄：不誤報");
    // 安裝目錄仍然檢查
    chmod(&env.paths.install_dir, 0o775);
    let why = precheck(&env).unwrap_err();
    assert!(
        why.contains("opt/pulse: writable by group or others"),
        "{why}"
    );
    chmod(&env.root.join("var/lib"), 0o755);
    env.cleanup();

    // 輔助程式（root）對「看不到」沒有例外：同樣的情況它回報 unsafe_path（cannot be examined）
    let env = setup("fm-pre-denied-helper", "tar");
    chmod(&env.paths.install_dir, 0o000);
    let fake = Fake::new(&env.root);
    let r = apply(&env.paths, &fake).unwrap();
    chmod(&env.paths.install_dir, 0o755);
    assert_eq!(r.code.as_deref(), Some(codes::E_UPGRADE_PRECHECK), "{r:?}");
    assert!(
        r.detail.starts_with(DETAIL_UNSAFE_PATH) && r.detail.contains("cannot be examined"),
        "{}",
        r.detail
    );
    env.cleanup();
}

/// 進行中的交易的工作目錄同樣要通過檢查：不通過時不接續，指標保留
#[test]
fn an_unsafe_work_directory_stops_a_resumed_transaction() {
    let mut env = setup("fm-path-resume", "tar");
    let fake = Fake::new(&env.root);
    apply_until(&env.paths, &fake, "binary_replaced");
    chmod(&env.work(), 0o777);
    env.helper_is_installed();
    let after = fake.reboot();
    let r = apply(&env.paths, &after).unwrap();
    assert_eq!(
        (r.state.as_str(), r.code.as_deref(), r.upgrade_id.as_str()),
        ("failed", Some(codes::E_UPGRADE_PRECHECK), ID),
        "{r:?}"
    );
    assert!(r.detail.starts_with(DETAIL_UNSAFE_PATH), "{}", r.detail);
    assert!(after.calls().is_empty(), "{:?}", after.calls());
    assert!(env.active(), "指標保留");
    assert_eq!(
        (env.installed().as_str(), env.prev().as_str()),
        ("0.9.1", "0.9.0"),
        "現場保留"
    );
    assert!(!env.paths.request.exists());
    env.cleanup();
}

// ---- 第 17.9.7 節：請求目錄的防符號連結（安全稽核 2026-11-14 A1）------------------
//
// 請求目錄 /var/lib/pulse/upgrade 由低權限的服務帳號擁有、可寫。輔助程式以 root 執行，一開始就以
// O_DIRECTORY|O_NOFOLLOW 開啟它取得 fd，之後對它的所有操作都相對那個 fd。這些測試只驗證「修好後攻擊被
// 擋」：不建立可運作的提權鏈。

/// 請求目錄在輔助程式執行前就被換成指向第三方目錄的符號連結：不跟隨、不做任何變更、回傳錯誤
#[test]
fn a_symlinked_request_directory_is_refused_without_following_it() {
    let env = setup("req-dir-link", "tar");
    let reqdir = env.root.join("var/lib/pulse/upgrade");
    let elsewhere = env.root.join("attacker");
    std::fs::create_dir(&elsewhere).unwrap();
    chmod(&elsewhere, 0o700);
    // 真正的請求目錄（含 request.json）移走，原位置換成指向攻擊者目錄的符號連結
    let moved = env.root.join("var/lib/pulse/upgrade.real");
    std::fs::rename(&reqdir, &moved).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &reqdir).unwrap();

    let fake = Fake::new(&env.root);
    let r = apply(&env.paths, &fake);
    // (b) 回傳錯誤
    assert!(r.is_err(), "請求目錄是符號連結：回傳錯誤，實際 {r:?}");
    // (a) 不在連結目標建立任何檔案，也沒有做任何系統變更
    assert_eq!(
        std::fs::read_dir(&elsewhere).unwrap().count(),
        0,
        "不寫進符號連結的目標"
    );
    assert!(fake.calls().is_empty(), "沒有任何變更：{:?}", fake.calls());
    assert_eq!(env.installed(), "0.9.0");
    env.cleanup();
}

/// 執行期間（寫 result.json 之前）服務帳號把請求目錄改名、換成符號連結：root 握住的是開啟當下的 fd，
/// 寫入與刪除都落在原本那個 inode，不會被導向攻擊者的目標
#[test]
fn a_mid_run_swap_of_the_request_directory_does_not_redirect_root_writes() {
    let env = setup("req-dir-swap", "tar");
    let reqdir = env.root.join("var/lib/pulse/upgrade");
    let elsewhere = env.root.join("attacker");
    std::fs::create_dir(&elsewhere).unwrap();
    chmod(&elsewhere, 0o700);
    let moved = env.root.join("var/lib/pulse/upgrade.real");

    let fake = Fake::new(&env.root);
    let reqdir2 = reqdir.clone();
    let moved2 = moved.clone();
    let elsewhere2 = elsewhere.clone();
    // 在 root 記下結果、寫 result.json 之前換掉請求目錄
    *fake.at_checkpoint.borrow_mut() = Some((
        "record_done".to_string(),
        Box::new(move || {
            std::fs::rename(&reqdir2, &moved2).unwrap();
            std::os::unix::fs::symlink(&elsewhere2, &reqdir2).unwrap();
        }),
    ));
    let r = apply(&env.paths, &fake).unwrap();
    assert_eq!(r.state, "succeeded", "{r:?}");
    // result.json 落在開啟當下那個 inode（被改名後的真目錄），攻擊者的連結目標保持空的
    assert_eq!(
        std::fs::read_dir(&elsewhere).unwrap().count(),
        0,
        "不寫進符號連結的目標"
    );
    assert!(
        moved.join("result.json").exists(),
        "result.json 落在原本的請求目錄"
    );
    assert!(
        !moved.join("request.json").exists(),
        "request.json 在原本的請求目錄被刪除"
    );
    env.cleanup();
}

/// request.json 的 target.version／target.commit 含 shell 注入字元、換行或超長：在寫任何結果或日誌之前
/// 就被 `Request::validate` 擋下，危險字元不會進入 result.json
#[test]
fn a_dangerous_target_is_rejected_before_anything_is_written() {
    let env = setup("req-target-danger", "tar");
    let dangers = [
        "0.9.1\n$(touch /tmp/pwn)".to_string(),
        "0.9.1`id`".to_string(),
        "$(reboot)".to_string(),
        "9".repeat(600),
    ];
    for danger in dangers {
        stage_upgrade(&env, ID, "0.9.0", "0.9.1");
        rewrite_request(&env, |r| r.target.version = danger.clone());
        let fake = Fake::new(&env.root);
        let r = apply(&env.paths, &fake);
        assert!(r.is_err(), "target.version {danger:?}：被拒");
        assert!(!env.has_result(), "不寫結果：{danger:?}");
        assert!(
            !env.paths.request.exists(),
            "request.json 已刪除：{danger:?}"
        );
        assert!(fake.calls().is_empty(), "沒有任何變更：{danger:?}");
        assert_eq!(env.installed(), "0.9.0", "{danger:?}");
    }
    // commit 同樣受限（7..=40 個小寫十六進位）
    stage_upgrade(&env, ID, "0.9.0", "0.9.1");
    rewrite_request(&env, |r| r.target.commit = "5e6f7a8`id`".to_string());
    let fake = Fake::new(&env.root);
    assert!(
        apply(&env.paths, &fake).is_err(),
        "危險的 target.commit 被拒"
    );
    assert!(!env.has_result());
    env.cleanup();
}

// ---- 第 17.9.8 節：工作目錄的保留上限 ---------------------------------------------

fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// 名稱是 UUID 的工作目錄
fn uuid_dirs(root: &Path) -> Vec<String> {
    let mut ids: Vec<String> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| kairos_proto::is_uuid_text(n))
        .collect();
    ids.sort();
    ids
}

/// 在 `root` 放一筆紀錄（`step`；`done` 時結果為 `state`），`age` 秒前寫入，另有一個大檔
fn put_record(root: &Path, step: Step, state: &str, age: u64) -> String {
    let id = new_id();
    let dir = root.join(&id);
    std::fs::create_dir(&dir).unwrap();
    chmod(&dir, 0o700);
    let mut rec = Record::new(&id, "0.9.0", "1a2b3c4", "tar");
    rec.step = step;
    if step == Step::Done {
        rec.result = Some(UpgradeResult {
            schema: files::RESULT_SCHEMA.into(),
            upgrade_id: id.clone(),
            state: state.into(),
            code: None,
            detail: String::new(),
            from_version: "0.9.0".into(),
            to_version: "0.9.1".into(),
            finished_at: crate::protocol::now_ts(),
        });
    }
    rec.save(&dir).unwrap();
    std::fs::write(dir.join(txn::NEW_BINARY_FILE), "big file\n").unwrap();
    std::fs::write(dir.join(txn::LOG_FILE), "log\n").unwrap();
    let when = std::time::SystemTime::now() - Duration::from_secs(age);
    std::fs::File::options()
        .write(true)
        .open(dir.join(txn::STATE_FILE))
        .unwrap()
        .set_modified(when)
        .unwrap();
    id
}

fn prune_root(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("pulse-helper-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

#[test]
fn pruning_keeps_the_latest_finished_record_and_three_rollback_failures() {
    let has_big = |root: &Path, id: &str| root.join(id).join(txn::NEW_BINARY_FILE).exists();
    let has_record = |root: &Path, id: &str| root.join(id).join(txn::STATE_FILE).exists();

    // 開始新的交易之前：只留最近一筆已結束的紀錄（只剩紀錄與日誌）
    let root = prune_root("prune-latest");
    let old: Vec<String> = [50, 40, 30, 20]
        .into_iter()
        .map(|age| put_record(&root, Step::Done, "failed", age))
        .collect();
    let latest = put_record(&root, Step::Done, "rolled_back", 10);
    let current = new_id();
    let mut removed = txn::prune(&root, &[current.as_str()], None);
    removed.sort();
    let mut expected = old.clone();
    expected.sort();
    assert_eq!(removed, expected);
    assert_eq!(uuid_dirs(&root), std::slice::from_ref(&latest));
    assert!(has_record(&root, &latest) && !has_big(&root, &latest));
    let _ = std::fs::remove_dir_all(&root);

    // 交易結束時：這一筆就是「最近一筆」；另外保留最近 3 筆 rollback_failed，只有最近的一筆保留全部檔案
    let root = prune_root("prune-failed");
    let failed: Vec<String> = [50, 40, 30, 20, 10]
        .into_iter()
        .map(|age| put_record(&root, Step::Done, "rollback_failed", age))
        .collect();
    let succeeded = put_record(&root, Step::Done, "succeeded", 5);
    let current = put_record(&root, Step::Done, "succeeded", 0);
    txn::prune(&root, &[current.as_str()], Some("succeeded"));
    let mut expected = vec![
        current.clone(),
        failed[2].clone(),
        failed[3].clone(),
        failed[4].clone(),
    ];
    expected.sort();
    assert_eq!(
        uuid_dirs(&root),
        expected,
        "較早的成功紀錄 {succeeded} 已刪除"
    );
    assert!(
        has_big(&root, &failed[4]),
        "最近一筆 rollback_failed 保留全部檔案"
    );
    assert!(!has_big(&root, &failed[3]) && !has_big(&root, &failed[2]));
    assert!(has_record(&root, &failed[3]) && has_record(&root, &failed[2]));
    assert!(has_big(&root, &current), "不動這一筆（由呼叫端處理）");
    let _ = std::fs::remove_dir_all(&root);

    // 這一筆自己是 rollback_failed：算進保留數，較早的只留紀錄與日誌
    let root = prune_root("prune-failed-current");
    let failed: Vec<String> = [40, 30, 20, 10]
        .into_iter()
        .map(|age| put_record(&root, Step::Done, "rollback_failed", age))
        .collect();
    let current = put_record(&root, Step::Done, "rollback_failed", 0);
    txn::prune(&root, &[current.as_str()], Some("rollback_failed"));
    let mut expected = vec![current.clone(), failed[2].clone(), failed[3].clone()];
    expected.sort();
    assert_eq!(uuid_dirs(&root), expected);
    assert!(has_big(&root, &current));
    assert!(!has_big(&root, &failed[3]) && !has_big(&root, &failed[2]));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn pruning_never_removes_unfinished_or_undecidable_records_or_foreign_entries() {
    let root = prune_root("prune-protected");
    let unfinished: Vec<String> = [
        Step::Replacing,
        Step::Replaced,
        Step::RollbackIntent,
        Step::RollingBack,
    ]
    .into_iter()
    .map(|step| put_record(&root, step, "", 100))
    .collect();
    let damaged = new_id();
    std::fs::create_dir(root.join(&damaged)).unwrap();
    std::fs::write(root.join(&damaged).join(txn::STATE_FILE), "{ not json").unwrap();
    // 會被清掉的：沒有紀錄的殘留目錄、還沒動手就中斷的紀錄
    let leftover = new_id();
    std::fs::create_dir(root.join(&leftover)).unwrap();
    std::fs::write(root.join(&leftover).join(txn::LOG_FILE), "log\n").unwrap();
    let preparing = put_record(&root, Step::Preparing, "", 100);
    let prepared = put_record(&root, Step::Prepared, "", 100);
    // 名稱不是 UUID 的項目不動
    std::fs::create_dir(root.join("archive")).unwrap();
    std::fs::write(root.join("archive/keep"), "x").unwrap();
    std::fs::write(root.join(ACTIVE_FILE), "x").unwrap();
    let current = new_id();
    let mut removed = txn::prune(&root, &[current.as_str()], None);
    removed.sort();
    let mut expected = vec![leftover, preparing, prepared];
    expected.sort();
    assert_eq!(removed, expected);
    let mut kept = unfinished.clone();
    kept.push(damaged);
    kept.sort();
    assert_eq!(uuid_dirs(&root), kept);
    for id in &unfinished {
        assert!(
            root.join(id).join(txn::NEW_BINARY_FILE).exists(),
            "不動檔案"
        );
    }
    assert!(root.join("archive/keep").exists() && root.join(ACTIVE_FILE).exists());
    let _ = std::fs::remove_dir_all(&root);
}

/// `pulse` 帳號不斷寫入新的 `request.json`（不同的 `upgrade_id`、反覆讓升級失敗）：
/// `/var/lib/pulse-upgrade` 不隨請求的次數成長
#[test]
fn the_work_root_does_not_grow_with_the_number_of_requests() {
    // 在還沒有紀錄時就被拒絕（from_version 不符）
    let env = setup("fm-cap-refused", "tar");
    for _ in 0..8 {
        let id = new_id();
        stage_upgrade(&env, &id, "0.8.0", "0.9.1");
        let r = apply(&env.paths, &Fake::new(&env.root)).unwrap();
        assert_eq!(
            (r.upgrade_id.as_str(), r.state.as_str()),
            (id.as_str(), "failed")
        );
        assert!(uuid_dirs(&env.paths.work_root).len() <= 1);
    }
    env.cleanup();

    // 驗證失敗（發佈檔被換掉）：有紀錄，只留最近一筆
    let env = setup("fm-cap-failed", "tar");
    for _ in 0..8 {
        let id = new_id();
        let req = stage_upgrade(&env, &id, "0.9.0", "0.9.1");
        std::fs::write(
            env.root
                .join("var/lib/pulse/upgrade")
                .join(&id)
                .join(&req.artifact),
            "VERSION=evil\n",
        )
        .unwrap();
        let r = apply(&env.paths, &Fake::new(&env.root)).unwrap();
        assert_eq!(r.code.as_deref(), Some(codes::E_UPGRADE_INTEGRITY), "{r:?}");
        assert_eq!(uuid_dirs(&env.paths.work_root), std::slice::from_ref(&id));
        assert_eq!(
            txn::traces(&env.paths.work_root.join(&id)),
            Vec::<String>::new(),
            "已結束的交易只留紀錄與日誌"
        );
    }
    env.cleanup();

    // 反覆 rollback_failed：最多 3 筆，只有最近的一筆保留全部檔案
    let env = setup("fm-cap-rollback-failed", "tar");
    let mut ids = Vec::new();
    for _ in 0..5 {
        let id = new_id();
        stage_upgrade(&env, &id, "0.9.0", "0.9.1");
        let fake = Fake::new(&env.root);
        fake.set("0.9.1", Behavior::Crash);
        fake.set("0.9.0", Behavior::NoConnect);
        let r = apply(&env.paths, &fake).unwrap();
        assert_eq!(r.state, "rollback_failed", "{r:?}");
        assert_eq!(env.installed(), "0.9.0");
        ids.push(id);
        let dirs = uuid_dirs(&env.paths.work_root);
        assert!(dirs.len() <= txn::KEEP_ROLLBACK_FAILED, "{dirs:?}");
        let full: Vec<&String> = dirs
            .iter()
            .filter(|d| !txn::traces(&env.paths.work_root.join(d.as_str())).is_empty())
            .collect();
        assert_eq!(full, [ids.last().unwrap()], "只有最近的一筆保留全部檔案");
    }
    let mut expected = ids[2..].to_vec();
    expected.sort();
    assert_eq!(uuid_dirs(&env.paths.work_root), expected);
    // 之後成功的升級：rollback_failed 的紀錄仍保留（最近 3 筆），成功的紀錄 1 筆
    let id = new_id();
    stage_upgrade(&env, &id, "0.9.0", "0.9.1");
    let r = apply(&env.paths, &Fake::new(&env.root)).unwrap();
    assert_eq!(r.state, "succeeded", "{r:?}");
    assert_eq!(uuid_dirs(&env.paths.work_root).len(), 4);
    env.cleanup();
}

/// 先前中斷留下的預檢目錄（`/opt/pulse/.upgrade-*`）在開始新的交易時清掉
#[test]
fn leftover_precheck_directories_are_removed() {
    let env = setup("fm-precheck-left", "tar");
    let stale = env.paths.install_dir.join(format!(".upgrade-{OTHER_ID}"));
    std::fs::create_dir_all(stale.join("pulse-old")).unwrap();
    let r = apply(&env.paths, &Fake::new(&env.root)).unwrap();
    assert_eq!(r.state, "succeeded", "{r:?}");
    assert!(!stale.exists());
    env.cleanup();
}

// ---- 第 17.9.9 節：故障注入只在測試用建置 -----------------------------------------

/// 打包時的檢查（`packaging/check-release-binary.sh`）認得的標記，就是開啟 feature `fault-injection` 時才
/// 編譯進執行檔的那一個字串與 `--version` 的結尾標記；預設的建置兩者都沒有
#[test]
fn the_packaging_check_knows_the_fault_injection_marker() {
    let script = include_str!("../../../../../packaging/check-release-binary.sh");
    let source = include_str!("../helper.rs");
    let marker = source
        .lines()
        .find_map(|l| l.strip_prefix("pub const FAULT_INJECTION_MARKER: &str = \""))
        .and_then(|l| l.strip_suffix("\";"))
        .expect("helper.rs 的 FAULT_INJECTION_MARKER");
    assert!(marker.len() >= 16, "標記要夠長，不會碰巧出現在別的地方");
    assert!(
        script.lines().any(|l| l == format!("MARKER={marker}")),
        "check-release-binary.sh 的 MARKER 與 helper.rs 不同"
    );
    assert!(
        script
            .lines()
            .any(|l| l == "VERSION_TAG='[fault-injection]'")
    );
    #[cfg(feature = "fault-injection")]
    {
        assert_eq!(marker, FAULT_INJECTION_MARKER);
        assert!(crate::cli::VERSION.ends_with(" [fault-injection]"));
    }
    #[cfg(not(feature = "fault-injection"))]
    assert!(!crate::cli::VERSION.contains("[fault-injection]"));
    // 正式的 RealSystem 在沒有 feature 時不做任何事：呼叫不存在的 checkpoint 立即返回
    RealSystem::default().checkpoint("no-such-checkpoint");
}
