mod executor;
mod manifest;
mod report;

use anyhow::{Context, Result};
use clap::Parser;
use executor::{abs_or_join, run_test, CaseResult, CaseStatus};
use manifest::{Manifest, TestSpec};
use report::{build_summary, write_reports};
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

#[derive(Debug, Parser)]
#[command(
    name = "dunitest-runner",
    version,
    about = "DragonOS dunitest runner (M1)"
)]
struct Cli {
    #[arg(long, default_value = "bin")]
    bin_dir: PathBuf,
    #[arg(long, default_value_t = 60)]
    timeout_sec: u64,
    #[arg(long, default_value = "whitelist.txt")]
    whitelist: PathBuf,
    #[arg(long, default_value = "blocklist.txt")]
    blocklist: PathBuf,
    #[arg(long, default_value = "no_skip.txt")]
    no_skip: PathBuf,
    #[arg(long, default_value = "results")]
    results_dir: PathBuf,
    #[arg(long)]
    list: bool,
    #[arg(long)]
    verbose: bool,
    #[arg(long = "pattern")]
    patterns: Vec<String>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let cwd = std::env::current_dir().with_context(|| "获取当前目录失败")?;

    let bin_dir = abs_or_join(&cwd, &cli.bin_dir.display().to_string());
    let whitelist_path = abs_or_join(&cwd, &cli.whitelist.display().to_string());
    let blocklist_path = abs_or_join(&cwd, &cli.blocklist.display().to_string());
    let no_skip_path = abs_or_join(&cwd, &cli.no_skip.display().to_string());
    let results_dir = abs_or_join(&cwd, &cli.results_dir.display().to_string());

    let manifest = Manifest::discover(&bin_dir, cli.timeout_sec)?;
    let whitelist = read_name_list(&whitelist_path);
    let blocklist = read_name_list(&blocklist_path);
    let no_skip = read_strict_name_list(&no_skip_path)?;
    validate_no_skip_config(&manifest, whitelist.as_ref(), &no_skip)?;

    if cli.list {
        for t in &manifest.tests {
            if select_test(t, &cli.patterns, whitelist.as_ref(), blocklist.as_ref()).is_none() {
                println!("{}", t.name);
            }
        }
        return Ok(());
    }

    fs::create_dir_all(&results_dir)
        .with_context(|| format!("创建结果目录失败: {}", results_dir.display()))?;

    let mut results: Vec<CaseResult> = Vec::new();
    let mut runnable_count = 0usize;
    for test in &manifest.tests {
        if let Some(skip_reason) =
            select_test(test, &cli.patterns, whitelist.as_ref(), blocklist.as_ref())
        {
            let skipped = CaseResult {
                name: test.name.clone(),
                status: CaseStatus::Skipped,
                duration_ms: 0,
                exit_code: None,
                message: skip_reason,
                log_file: String::new(),
                gtest_total: 0,
                gtest_passed: 0,
                gtest_failed: 0,
                gtest_skipped: 0,
            };
            println!("[RUNNER] SKIP: {} reason={}", skipped.name, skipped.message);
            results.push(skipped);
            continue;
        }

        runnable_count += 1;
        println!("[RUNNER] START: {}", test.name);

        let mut t = test.clone();
        t.path = abs_or_join(&cwd, &t.path).display().to_string();

        let mut one = run_test(&t, &results_dir, cli.verbose)?;
        enforce_no_skip(&mut one, &no_skip);
        println!(
            "[RUNNER] END: {} status={} duration_ms={} log={}",
            one.name,
            case_status_text(&one.status),
            one.duration_ms,
            one.log_file
        );
        results.push(one);
    }

    let summary = build_summary(results);
    write_reports(&results_dir, &summary)?;
    show_summary(&summary, &results_dir);

    if runnable_count == 0 {
        eprintln!("[RUNNER] ERROR: no runnable tests selected");
        std::process::exit(1);
    }

    if summary.failed > 0 || summary.timeout > 0 {
        std::process::exit(1);
    }

    Ok(())
}

fn select_test(
    test: &TestSpec,
    patterns: &[String],
    whitelist: Option<&HashSet<String>>,
    blocklist: Option<&HashSet<String>>,
) -> Option<String> {
    if let Some(wl) = whitelist {
        if !wl.contains(&test.name) {
            return Some("not_in_whitelist".to_string());
        }
    }
    if let Some(bl) = blocklist {
        if bl.contains(&test.name) {
            return Some("matched_blocklist".to_string());
        }
    }
    if !patterns.is_empty() && !patterns.iter().any(|p| test.name.contains(p)) {
        return Some("pattern_mismatch".to_string());
    }
    None
}

fn read_name_list(path: &Path) -> Option<HashSet<String>> {
    if !path.exists() {
        return None;
    }
    let Ok(content) = fs::read_to_string(path) else {
        return None;
    };
    let mut set = HashSet::new();
    for line in content.lines() {
        let s = line.trim();
        if !s.is_empty() && !s.starts_with('#') {
            set.insert(s.to_string());
        }
    }
    Some(set)
}

fn read_strict_name_list(path: &Path) -> Result<HashSet<String>> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("读取 no-skip 配置失败: {}", path.display()))?;
    let mut set = HashSet::new();
    for (index, line) in content.lines().enumerate() {
        let name = line.trim();
        if name.is_empty() || name.starts_with('#') {
            continue;
        }
        if name.starts_with('/')
            || name
                .split('/')
                .any(|part| part.is_empty() || part == "." || part == "..")
        {
            anyhow::bail!("no-skip 第 {} 行包含非法测试名: {}", index + 1, name);
        }
        if !set.insert(name.to_string()) {
            anyhow::bail!("no-skip 配置包含重复测试: {}", name);
        }
    }
    if set.is_empty() {
        anyhow::bail!("no-skip 配置为空: {}", path.display());
    }
    Ok(set)
}

fn validate_no_skip_config(
    manifest: &Manifest,
    whitelist: Option<&HashSet<String>>,
    no_skip: &HashSet<String>,
) -> Result<()> {
    let discovered: HashSet<_> = manifest
        .tests
        .iter()
        .map(|test| test.name.as_str())
        .collect();
    for name in no_skip {
        if !discovered.contains(name.as_str()) {
            anyhow::bail!("no-skip 测试二进制不存在: {}", name);
        }
        if let Some(whitelist) = whitelist {
            if !whitelist.contains(name) {
                anyhow::bail!("no-skip 测试未进入 dunitest 白名单: {}", name);
            }
        }
    }
    Ok(())
}

fn enforce_no_skip(result: &mut CaseResult, no_skip: &HashSet<String>) {
    if !no_skip.contains(&result.name) {
        return;
    }

    let accounted = result
        .gtest_passed
        .checked_add(result.gtest_failed)
        .and_then(|count| count.checked_add(result.gtest_skipped));
    if result.gtest_total == 0 || accounted != Some(result.gtest_total) {
        result.status = CaseStatus::Failed;
        result.message = format!(
            "严格一致性测试缺少完整 gtest 汇总: total={}, passed={}, failed={}, skipped={}",
            result.gtest_total, result.gtest_passed, result.gtest_failed, result.gtest_skipped
        );
    } else if result.gtest_skipped > 0 {
        result.status = CaseStatus::Failed;
        result.message = format!(
            "严格一致性测试不允许 skip，实际跳过 {} 个 gtest case",
            result.gtest_skipped
        );
    }
}

fn show_summary(summary: &report::Summary, results_dir: &Path) {
    println!();
    println!("================ dunitest ================");
    println!("总测试数: {}", summary.total);
    println!("通过: {}", summary.passed);
    println!("失败: {}", summary.failed);
    println!("跳过: {}", summary.skipped);
    println!("超时: {}", summary.timeout);
    println!("成功率: {:.2}%", summary.success_rate);
    println!("报告目录: {}", results_dir.display());
    println!("==========================================");
}

fn case_status_text(status: &CaseStatus) -> &'static str {
    match status {
        CaseStatus::Passed => "PASSED",
        CaseStatus::Failed => "FAILED",
        CaseStatus::Skipped => "SKIPPED",
        CaseStatus::Timeout => "TIMEOUT",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_skip_policy_converts_success_to_failure() {
        let mut result = CaseResult {
            name: "normal/test_pivot_root".to_string(),
            status: CaseStatus::Passed,
            duration_ms: 1,
            exit_code: Some(0),
            message: "ok".to_string(),
            log_file: String::new(),
            gtest_total: 19,
            gtest_passed: 18,
            gtest_failed: 0,
            gtest_skipped: 1,
        };
        let no_skip = HashSet::from(["normal/test_pivot_root".to_string()]);
        enforce_no_skip(&mut result, &no_skip);
        assert!(matches!(result.status, CaseStatus::Failed));
        assert!(result.message.contains("不允许 skip"));
    }

    #[test]
    fn no_skip_policy_rejects_truncated_gtest_summary() {
        let mut result = CaseResult {
            name: "normal/test_pivot_root".to_string(),
            status: CaseStatus::Passed,
            duration_ms: 1,
            exit_code: Some(0),
            message: "ok".to_string(),
            log_file: String::new(),
            gtest_total: 19,
            gtest_passed: 0,
            gtest_failed: 0,
            gtest_skipped: 0,
        };
        let no_skip = HashSet::from(["normal/test_pivot_root".to_string()]);
        enforce_no_skip(&mut result, &no_skip);
        assert!(matches!(result.status, CaseStatus::Failed));
        assert!(result.message.contains("缺少完整 gtest 汇总"));
    }
}
