use crate::manifest::TestSpec;
use anyhow::{Context, Result};
use serde::Serialize;
use std::{
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    thread::JoinHandle,
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CaseStatus {
    Passed,
    Failed,
    Skipped,
    Timeout,
}

#[derive(Debug, Clone, Serialize)]
pub struct CaseResult {
    pub name: String,
    pub status: CaseStatus,
    pub duration_ms: u128,
    pub exit_code: Option<i32>,
    pub message: String,
    pub log_file: String,
    pub gtest_total: usize,
    pub gtest_passed: usize,
    pub gtest_failed: usize,
    pub gtest_skipped: usize,
}

pub fn run_test(spec: &TestSpec, results_dir: &Path, verbose: bool) -> Result<CaseResult> {
    let start = Instant::now();
    let log_path = results_dir.join(format!("{}.log", sanitize_case_name(&spec.name)));
    let mut precheck_log =
        File::create(&log_path).with_context(|| format!("创建日志文件失败: {}", log_path.display()))?;

    if let Some(msg) = validate_gtest_binary(spec)? {
        writeln!(precheck_log, "{}", msg).with_context(|| "写入日志失败")?;
        let result = CaseResult {
            name: spec.name.clone(),
            status: CaseStatus::Failed,
            duration_ms: start.elapsed().as_millis(),
            exit_code: None,
            message: "非 gtest 测例，已拒绝执行".to_string(),
            log_file: log_path.display().to_string(),
            gtest_total: 0,
            gtest_passed: 0,
            gtest_failed: 0,
            gtest_skipped: 0,
        };
        return Ok(result);
    }
    drop(precheck_log);

    let log_file =
        File::create(&log_path).with_context(|| format!("创建日志文件失败: {}", log_path.display()))?;
    let shared_log = Arc::new(Mutex::new(log_file));

    let mut cmd = Command::new(&spec.path);
    cmd.args(&spec.args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().with_context(|| {
        format!(
            "启动测试进程失败: name={}, path={}",
            spec.name.as_str(),
            spec.path.as_str()
        )
    })?;
    let stdout_pipe = child
        .stdout
        .take()
        .with_context(|| "获取子进程 stdout 管道失败")?;
    let stderr_pipe = child
        .stderr
        .take()
        .with_context(|| "获取子进程 stderr 管道失败")?;

    let stdout_thread = spawn_pipe_forwarder(stdout_pipe, Arc::clone(&shared_log), false);
    let stderr_thread = spawn_pipe_forwarder(stderr_pipe, Arc::clone(&shared_log), true);

    let timeout = Duration::from_secs(spec.timeout_sec);
    let status = loop {
        if let Some(status) = child.try_wait().with_context(|| "等待测试进程状态失败")? {
            break status;
        }
        if start.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            let result = CaseResult {
                name: spec.name.clone(),
                status: CaseStatus::Timeout,
                duration_ms: start.elapsed().as_millis(),
                exit_code: None,
                message: format!("超时: {} 秒", spec.timeout_sec),
                log_file: log_path.display().to_string(),
                gtest_total: 0,
                gtest_passed: 0,
                gtest_failed: 0,
                gtest_skipped: 0,
            };
            return Ok(result);
        }
        thread::sleep(Duration::from_millis(50));
    };
    join_pipe_forwarder(stdout_thread)?;
    join_pipe_forwarder(stderr_thread)?;

    let elapsed = start.elapsed().as_millis();
    let code = status.code();
    let passed = status.success();
    let gtest = parse_gtest_counts(&log_path).unwrap_or((0, 0, 0, 0));

    let result = if passed {
        CaseResult {
            name: spec.name.clone(),
            status: CaseStatus::Passed,
            duration_ms: elapsed,
            exit_code: code,
            message: "ok".to_string(),
            log_file: log_path.display().to_string(),
            gtest_total: gtest.0,
            gtest_passed: gtest.1,
            gtest_failed: gtest.2,
            gtest_skipped: gtest.3,
        }
    } else {
        CaseResult {
            name: spec.name.clone(),
            status: CaseStatus::Failed,
            duration_ms: elapsed,
            exit_code: code,
            message: format!(
                "gtest 返回失败退出码: {:?}",
                code
            ),
            log_file: log_path.display().to_string(),
            gtest_total: gtest.0,
            gtest_passed: gtest.1,
            gtest_failed: gtest.2,
            gtest_skipped: gtest.3,
        }
    };

    let _ = verbose;

    Ok(result)
}

fn parse_gtest_counts(log_path: &Path) -> Result<(usize, usize, usize, usize)> {
    let content = std::fs::read_to_string(log_path)
        .with_context(|| format!("读取 gtest 日志失败: {}", log_path.display()))?;

    let mut total = 0usize;
    let mut passed = 0usize;
    let mut failed = 0usize;
    let mut skipped = 0usize;

    for line in content.lines() {
        let s = line.trim();

        if s.starts_with("[==========]") && s.contains(" tests from ") && s.contains(" ran.") {
            if let Some(v) = first_usize_token(s) {
                total = v;
            }
            continue;
        }

        if let Some(v) = parse_summary_counter_line(s, "[  PASSED  ]") {
            passed = v;
            continue;
        }
        if let Some(v) = parse_summary_counter_line(s, "[  FAILED  ]") {
            failed = v;
            continue;
        }
        if let Some(v) = parse_summary_counter_line(s, "[  SKIPPED ]") {
            skipped = v;
            continue;
        }
    }

    Ok((total, passed, failed, skipped))
}

fn parse_summary_counter_line(line: &str, prefix: &str) -> Option<usize> {
    if !line.starts_with(prefix) {
        return None;
    }
    first_usize_token(line)
}

fn first_usize_token(s: &str) -> Option<usize> {
    for token in s.split_whitespace() {
        if let Ok(v) = token.parse::<usize>() {
            return Some(v);
        }
    }
    None
}

fn sanitize_case_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

pub fn abs_or_join(base: &Path, path: &str) -> PathBuf {
    let p = PathBuf::from(path);
    if p.is_absolute() { p } else { base.join(p) }
}

fn validate_gtest_binary(spec: &TestSpec) -> Result<Option<String>> {
    let output = Command::new(&spec.path)
        .arg("--gtest_help")
        .output()
        .with_context(|| format!("预检查 gtest 失败: {}", spec.path))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let merged = format!("{}\n{}", stdout, stderr);
    let marker = "This program contains tests written using Google Test";

    if output.status.success() && merged.contains(marker) {
        return Ok(None);
    }

    Ok(Some(format!(
        "dunitest: '{}' 不是有效 gtest 测例，缺少 gtest 标识文本。\n预检查退出码: {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        spec.path,
        output.status.code(),
        stdout,
        stderr
    )))
}

fn spawn_pipe_forwarder<R>(mut reader: R, shared_log: Arc<Mutex<File>>, is_stderr: bool) -> JoinHandle<Result<()>>
where
    R: Read + Send + 'static,
{
    thread::spawn(move || -> Result<()> {
        let mut buf = [0_u8; 4096];
        loop {
            let n = reader.read(&mut buf).with_context(|| "读取子进程管道失败")?;
            if n == 0 {
                break;
            }

            if is_stderr {
                let mut term = std::io::stderr().lock();
                term.write_all(&buf[..n]).with_context(|| "写入终端 stderr 失败")?;
                term.flush().with_context(|| "刷新终端 stderr 失败")?;
            } else {
                let mut term = std::io::stdout().lock();
                term.write_all(&buf[..n]).with_context(|| "写入终端 stdout 失败")?;
                term.flush().with_context(|| "刷新终端 stdout 失败")?;
            }

            let mut log = shared_log.lock().map_err(|_| anyhow::anyhow!("日志锁已损坏"))?;
            log.write_all(&buf[..n]).with_context(|| "写入日志文件失败")?;
            log.flush().with_context(|| "刷新日志文件失败")?;
        }
        Ok(())
    })
}

fn join_pipe_forwarder(handle: JoinHandle<Result<()>>) -> Result<()> {
    match handle.join() {
        Ok(inner) => inner,
        Err(_) => anyhow::bail!("输出转发线程发生 panic"),
    }
}
