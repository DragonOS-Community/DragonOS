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
    let mut passed = 0usize;
    let mut failed = 0usize;
    let mut skipped = 0usize;
    let mut timeout = 0usize;
    let mut gtest_total = 0usize;
    let mut gtest_passed = 0usize;
    let mut gtest_failed = 0usize;
    let mut gtest_skipped = 0usize;

    for c in &cases {
        gtest_total += c.gtest_total;
        gtest_passed += c.gtest_passed;
        gtest_failed += c.gtest_failed;
        gtest_skipped += c.gtest_skipped;

        match c.status {
            CaseStatus::Passed => {
                if c.gtest_total == 0 {
                    passed += 1;
                }
            }
            CaseStatus::Failed => {
                if c.gtest_total == 0 {
                    failed += 1;
                }
            }
            CaseStatus::Skipped => {
                if c.gtest_total == 0 {
                    skipped += 1;
                }
            }
            CaseStatus::Timeout => {
                if c.gtest_total == 0 {
                    timeout += 1;
                }
            }
        }
    }

    if gtest_total > 0 {
        passed = gtest_passed;
        failed = gtest_failed;
        skipped = gtest_skipped;
        timeout = 0;
    }

    let total = if gtest_total > 0 {
        gtest_total
    } else {
        cases.len()
    };
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

fn write_json_report(results_dir: &Path, summary: &Summary) -> Result<()> {
    let file = results_dir.join("summary.json");
    let content = serde_json::to_string_pretty(summary).with_context(|| "序列化 summary.json 失败")?;
    fs::write(&file, content).with_context(|| format!("写入失败: {}", file.display()))?;
    Ok(())
}

fn write_failed_cases(results_dir: &Path, summary: &Summary) -> Result<()> {
    let file = results_dir.join("failed_cases.txt");
    let mut f =
        File::create(&file).with_context(|| format!("创建文件失败: {}", file.display()))?;
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
