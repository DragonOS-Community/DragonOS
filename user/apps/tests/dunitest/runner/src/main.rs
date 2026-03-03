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
#[command(name = "dunitest-runner", version, about = "DragonOS dunitest runner (M1)")]
struct Cli {
    #[arg(long, default_value = "bin")]
    bin_dir: PathBuf,
    #[arg(long, default_value_t = 60)]
    timeout_sec: u64,
    #[arg(long, default_value = "whitelist.txt")]
    whitelist: PathBuf,
    #[arg(long, default_value = "blocklist.txt")]
    blocklist: PathBuf,
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
    let results_dir = abs_or_join(&cwd, &cli.results_dir.display().to_string());

    let manifest = Manifest::discover(&bin_dir, cli.timeout_sec)?;
    let whitelist = read_name_list(&whitelist_path);
    let blocklist = read_name_list(&blocklist_path);

    if cli.list {
        for t in &manifest.tests {
            if select_test(
                t,
                &cli.patterns,
                whitelist.as_ref(),
                blocklist.as_ref(),
            )
            .is_none()
            {
                println!("{}", t.name);
            }
        }
        return Ok(());
    }

    fs::create_dir_all(&results_dir)
        .with_context(|| format!("创建结果目录失败: {}", results_dir.display()))?;

    let mut results: Vec<CaseResult> = Vec::new();
    for test in &manifest.tests {
        if let Some(skip_reason) = select_test(
            test,
            &cli.patterns,
            whitelist.as_ref(),
            blocklist.as_ref(),
        ) {
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
            println!(
                "[RUNNER] SKIP: {} reason={}",
                skipped.name, skipped.message
            );
            results.push(skipped);
            continue;
        }

        println!("[RUNNER] START: {}", test.name);

        let mut t = test.clone();
        t.path = abs_or_join(&cwd, &t.path).display().to_string();

        let one = run_test(&t, &results_dir, cli.verbose)?;
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
