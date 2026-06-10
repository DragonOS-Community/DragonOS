use crate::executor::{CaseResult, CaseStatus};
use anyhow::{Context, Result};
use serde::Serialize;
use std::{
    fs::{self, File},
    io::Write,
    path::Path,
};

#[derive(Debug, Serialize)]
pub struct Summary {
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    pub timeout: usize,
    pub success_rate: f64,
    pub cases: Vec<CaseResult>,
}

pub fn build_summary(cases: Vec<CaseResult>) -> Summary {
    let mut total = 0usize;
    let mut passed = 0usize;
    let mut failed = 0usize;
    let mut skipped = 0usize;
    let mut timeout = 0usize;

    for c in &cases {
        if c.gtest_total > 0 {
            total += c.gtest_total;
            passed += c.gtest_passed;
            failed += c.gtest_failed;
            skipped += c.gtest_skipped;

            match c.status {
                CaseStatus::Timeout => {
                    total += 1;
                    timeout += 1;
                }
                CaseStatus::Failed if c.gtest_failed == 0 => {
                    total += 1;
                    failed += 1;
                }
                CaseStatus::Skipped if c.gtest_skipped == 0 => {
                    total += 1;
                    skipped += 1;
                }
                CaseStatus::Passed | CaseStatus::Failed | CaseStatus::Skipped => {}
            }
            continue;
        }

        total += 1;
        match c.status {
            CaseStatus::Passed => passed += 1,
            CaseStatus::Failed => failed += 1,
            CaseStatus::Skipped => skipped += 1,
            CaseStatus::Timeout => timeout += 1,
        }
    }

    let success_rate = if total == 0 {
        0.0
    } else {
        (passed as f64) * 100.0 / (total as f64)
    };

    Summary {
        total,
        passed,
        failed,
        skipped,
        timeout,
        success_rate,
        cases,
    }
}

pub fn write_reports(results_dir: &Path, summary: &Summary) -> Result<()> {
    fs::create_dir_all(results_dir)
        .with_context(|| format!("创建结果目录失败: {}", results_dir.display()))?;

    write_text_report(results_dir, summary)?;
    write_json_report(results_dir, summary)?;
    write_failed_cases(results_dir, summary)?;

    Ok(())
}

fn write_text_report(results_dir: &Path, summary: &Summary) -> Result<()> {
    let report = results_dir.join("test_report.txt");
    let mut f =
        File::create(&report).with_context(|| format!("创建报告文件失败: {}", report.display()))?;

    writeln!(f, "dunitest 报告")?;
    writeln!(f, "==========================")?;
    writeln!(f, "总测试数: {}", summary.total)?;
    writeln!(f, "通过: {}", summary.passed)?;
    writeln!(f, "失败: {}", summary.failed)?;
    writeln!(f, "跳过: {}", summary.skipped)?;
    writeln!(f, "超时: {}", summary.timeout)?;
    writeln!(f, "成功率: {:.2}%", summary.success_rate)?;
    writeln!(f)?;
    writeln!(f, "失败/超时列表:")?;

    for c in &summary.cases {
        match c.status {
            CaseStatus::Failed | CaseStatus::Timeout => {
                writeln!(f, "- {}: {}", c.name, c.message)?;
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn case(
        name: &str,
        status: CaseStatus,
        gtest_total: usize,
        gtest_passed: usize,
        gtest_failed: usize,
        gtest_skipped: usize,
    ) -> CaseResult {
        CaseResult {
            name: name.to_string(),
            status,
            duration_ms: 0,
            exit_code: None,
            message: String::new(),
            log_file: String::new(),
            gtest_total,
            gtest_passed,
            gtest_failed,
            gtest_skipped,
        }
    }

    #[test]
    fn preserves_timeout_when_mixed_with_gtest_passes() {
        let summary = build_summary(vec![
            case("normal/passed", CaseStatus::Passed, 3, 3, 0, 0),
            case("normal/timeout", CaseStatus::Timeout, 0, 0, 0, 0),
        ]);

        assert_eq!(summary.total, 4);
        assert_eq!(summary.passed, 3);
        assert_eq!(summary.failed, 0);
        assert_eq!(summary.skipped, 0);
        assert_eq!(summary.timeout, 1);
        assert!(summary.success_rate < 100.0);
    }

    #[test]
    fn timeout_status_wins_over_partial_gtest_counts() {
        let summary = build_summary(vec![case(
            "normal/partial-timeout",
            CaseStatus::Timeout,
            2,
            2,
            0,
            0,
        )]);

        assert_eq!(summary.total, 3);
        assert_eq!(summary.passed, 2);
        assert_eq!(summary.failed, 0);
        assert_eq!(summary.skipped, 0);
        assert_eq!(summary.timeout, 1);
    }

    #[test]
    fn failed_status_is_preserved_when_gtest_counts_do_not_explain_it() {
        let summary = build_summary(vec![case(
            "normal/failed-after-gtest",
            CaseStatus::Failed,
            2,
            2,
            0,
            0,
        )]);

        assert_eq!(summary.total, 3);
        assert_eq!(summary.passed, 2);
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.skipped, 0);
        assert_eq!(summary.timeout, 0);
    }

    #[test]
    fn mixes_gtest_failure_and_case_skip() {
        let summary = build_summary(vec![
            case("normal/gtest-failed", CaseStatus::Failed, 4, 2, 1, 1),
            case("normal/skipped", CaseStatus::Skipped, 0, 0, 0, 0),
        ]);

        assert_eq!(summary.total, 5);
        assert_eq!(summary.passed, 2);
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.skipped, 2);
        assert_eq!(summary.timeout, 0);
    }

    #[test]
    fn keeps_non_gtest_case_only_behavior() {
        let summary = build_summary(vec![
            case("case/passed", CaseStatus::Passed, 0, 0, 0, 0),
            case("case/failed", CaseStatus::Failed, 0, 0, 0, 0),
            case("case/skipped", CaseStatus::Skipped, 0, 0, 0, 0),
            case("case/timeout", CaseStatus::Timeout, 0, 0, 0, 0),
        ]);

        assert_eq!(summary.total, 4);
        assert_eq!(summary.passed, 1);
        assert_eq!(summary.failed, 1);
        assert_eq!(summary.skipped, 1);
        assert_eq!(summary.timeout, 1);
    }
}

fn write_json_report(results_dir: &Path, summary: &Summary) -> Result<()> {
    let file = results_dir.join("summary.json");
    let content =
        serde_json::to_string_pretty(summary).with_context(|| "序列化 summary.json 失败")?;
    fs::write(&file, content).with_context(|| format!("写入失败: {}", file.display()))?;
    Ok(())
}

fn write_failed_cases(results_dir: &Path, summary: &Summary) -> Result<()> {
    let file = results_dir.join("failed_cases.txt");
    let mut f = File::create(&file).with_context(|| format!("创建文件失败: {}", file.display()))?;
    for c in &summary.cases {
        match c.status {
            CaseStatus::Failed | CaseStatus::Timeout => {
                writeln!(f, "{}", c.name)?;
            }
            _ => {}
        }
    }
    Ok(())
}
