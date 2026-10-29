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

const PIPE_JOIN_TIMEOUT: Duration = Duration::from_secs(2);
const GTEST_PRECHECK_MAX_TIMEOUT: Duration = Duration::from_secs(15);

#[cfg(unix)]
use std::os::unix::process::CommandExt;

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
    let precheck_start = Instant::now();
    let log_path = results_dir.join(format!("{}.log", sanitize_case_name(&spec.name)));
    let mut precheck_log = File::create(&log_path)
        .with_context(|| format!("创建日志文件失败: {}", log_path.display()))?;

    match validate_gtest_binary(spec)? {
        GtestPrecheck::Valid => {}
        GtestPrecheck::Invalid(msg) => {
            writeln!(precheck_log, "{}", msg).with_context(|| "写入日志失败")?;
            let result = CaseResult {
                name: spec.name.clone(),
                status: CaseStatus::Failed,
                duration_ms: precheck_start.elapsed().as_millis(),
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
        GtestPrecheck::Timeout(msg) => {
            writeln!(precheck_log, "{}", msg).with_context(|| "写入日志失败")?;
            let result = CaseResult {
                name: spec.name.clone(),
                status: CaseStatus::Timeout,
                duration_ms: precheck_start.elapsed().as_millis(),
                exit_code: None,
                message: msg,
                log_file: log_path.display().to_string(),
                gtest_total: 0,
                gtest_passed: 0,
                gtest_failed: 0,
                gtest_skipped: 0,
            };
            return Ok(result);
        }
        GtestPrecheck::CleanupFailed(msg) => {
            writeln!(precheck_log, "{}", msg).with_context(|| "写入日志失败")?;
            let result = CaseResult {
                name: spec.name.clone(),
                status: CaseStatus::Failed,
                duration_ms: precheck_start.elapsed().as_millis(),
                exit_code: None,
                message: msg,
                log_file: log_path.display().to_string(),
                gtest_total: 0,
                gtest_passed: 0,
                gtest_failed: 0,
                gtest_skipped: 0,
            };
            return Ok(result);
        }
    }
    drop(precheck_log);

    let log_file = File::create(&log_path)
        .with_context(|| format!("创建日志文件失败: {}", log_path.display()))?;
    let shared_log = Arc::new(Mutex::new(log_file));

    let start = Instant::now();
    let mut cmd = Command::new(&spec.path);
    cmd.args(&spec.args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_case_process_group(&mut cmd);

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
            let kill_group_result = terminate_case_process_group(child.id());
            let _ = child.kill();
            let _ = child.wait();

            let mut message = format!("超时: {} 秒", spec.timeout_sec);
            if let Err(e) = kill_group_result {
                message.push_str(&format!("; 进程组终止失败，跳过管道线程收尾: {e:#}"));
            } else {
                let stdout_join = join_pipe_forwarder_bounded(stdout_thread, PIPE_JOIN_TIMEOUT);
                let stderr_join = join_pipe_forwarder_bounded(stderr_thread, PIPE_JOIN_TIMEOUT);
                if let Err(e) = stdout_join {
                    message.push_str(&format!("; stdout 日志收尾失败: {e:#}"));
                }
                if let Err(e) = stderr_join {
                    message.push_str(&format!("; stderr 日志收尾失败: {e:#}"));
                }
            }

            let gtest = parse_gtest_counts(&log_path).unwrap_or((0, 0, 0, 0));
            let result = CaseResult {
                name: spec.name.clone(),
                status: CaseStatus::Timeout,
                duration_ms: start.elapsed().as_millis(),
                exit_code: None,
                message,
                log_file: log_path.display().to_string(),
                gtest_total: gtest.0,
                gtest_passed: gtest.1,
                gtest_failed: gtest.2,
                gtest_skipped: gtest.3,
            };
            return Ok(result);
        }
        thread::sleep(Duration::from_millis(50));
    };
    let cleanup_result = terminate_case_process_group(child.id());
    let stdout_join = join_pipe_forwarder_bounded(stdout_thread, PIPE_JOIN_TIMEOUT);
    let stderr_join = join_pipe_forwarder_bounded(stderr_thread, PIPE_JOIN_TIMEOUT);
    let mut cleanup_message = String::new();
    if let Err(e) = cleanup_result {
        cleanup_message.push_str(&format!("; 进程组清理失败: {e:#}"));
    }
    if let Err(e) = stdout_join {
        cleanup_message.push_str(&format!("; stdout 日志收尾失败: {e:#}"));
    }
    if let Err(e) = stderr_join {
        cleanup_message.push_str(&format!("; stderr 日志收尾失败: {e:#}"));
    }

    let elapsed = start.elapsed().as_millis();
    let code = status.code();
    let passed = status.success();
    let gtest = parse_gtest_counts(&log_path).unwrap_or((0, 0, 0, 0));

    let result = if !cleanup_message.is_empty() {
        CaseResult {
            name: spec.name.clone(),
            status: CaseStatus::Failed,
            duration_ms: elapsed,
            exit_code: code,
            message: format!("测试进程退出后清理失败{cleanup_message}"),
            log_file: log_path.display().to_string(),
            gtest_total: gtest.0,
            gtest_passed: gtest.1,
            gtest_failed: gtest.2,
            gtest_skipped: gtest.3,
        }
    } else if passed {
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
            message: format!("gtest 返回失败退出码: {:?}", code),
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

#[cfg(unix)]
fn configure_case_process_group(cmd: &mut Command) {
    unsafe {
        cmd.pre_exec(|| {
            if libc::setpgid(0, 0) == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
}

#[cfg(not(unix))]
fn configure_case_process_group(_cmd: &mut Command) {}

#[cfg(unix)]
fn terminate_case_process_group(child_pid: u32) -> Result<()> {
    let pgid = -(child_pid as libc::pid_t);
    if unsafe { libc::kill(pgid, libc::SIGKILL) } == 0 {
        return Ok(());
    }

    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(err).with_context(|| format!("kill(-{}, SIGKILL) 失败", child_pid))
}

#[cfg(not(unix))]
fn terminate_case_process_group(_child_pid: u32) -> Result<()> {
    anyhow::bail!("process-group termination is unsupported on this platform")
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

        let has_test_count = s.contains(" tests from ") || s.contains(" test from ");
        if s.starts_with("[==========]") && has_test_count && s.contains(" ran.") {
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
    if p.is_absolute() {
        p
    } else {
        base.join(p)
    }
}

enum GtestPrecheck {
    Valid,
    Invalid(String),
    Timeout(String),
    CleanupFailed(String),
}

fn validate_gtest_binary(spec: &TestSpec) -> Result<GtestPrecheck> {
    let mut cmd = Command::new(&spec.path);
    cmd.arg("--gtest_help")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_case_process_group(&mut cmd);

    let mut child = cmd
        .spawn()
        .with_context(|| format!("预检查 gtest 失败: {}", spec.path))?;
    let stdout_pipe = child
        .stdout
        .take()
        .with_context(|| "获取预检查 stdout 管道失败")?;
    let stderr_pipe = child
        .stderr
        .take()
        .with_context(|| "获取预检查 stderr 管道失败")?;
    let stdout_thread = spawn_pipe_collector(stdout_pipe);
    let stderr_thread = spawn_pipe_collector(stderr_pipe);

    let timeout = gtest_precheck_timeout(spec.timeout_sec);
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().with_context(|| "等待预检查进程状态失败")?
        {
            break status;
        }
        if start.elapsed() >= timeout {
            let kill_group_result = terminate_case_process_group(child.id());
            let _ = child.kill();
            let _ = child.wait();

            let mut message = format!("gtest 预检查超时: {} 秒", timeout.as_secs());
            if let Err(e) = kill_group_result {
                message.push_str(&format!("; 进程组终止失败，跳过管道线程收尾: {e:#}"));
            } else {
                let _ = join_pipe_collector_bounded(stdout_thread, PIPE_JOIN_TIMEOUT);
                let _ = join_pipe_collector_bounded(stderr_thread, PIPE_JOIN_TIMEOUT);
            }
            return Ok(GtestPrecheck::Timeout(message));
        }
        thread::sleep(Duration::from_millis(20));
    };

    let cleanup_result = terminate_case_process_group(child.id());
    let stdout_result = join_pipe_collector_bounded(stdout_thread, PIPE_JOIN_TIMEOUT);
    let stderr_result = join_pipe_collector_bounded(stderr_thread, PIPE_JOIN_TIMEOUT);

    let mut cleanup_message = String::new();
    if let Err(e) = cleanup_result {
        cleanup_message.push_str(&format!("; 进程组清理失败: {e:#}"));
    }
    if let Err(e) = stdout_result.as_ref() {
        cleanup_message.push_str(&format!("; stdout 收集失败: {e:#}"));
    }
    if let Err(e) = stderr_result.as_ref() {
        cleanup_message.push_str(&format!("; stderr 收集失败: {e:#}"));
    }
    if !cleanup_message.is_empty() {
        return Ok(GtestPrecheck::CleanupFailed(format!(
            "gtest 预检查清理失败{cleanup_message}"
        )));
    }

    let stdout = stdout_result.expect("stdout precheck result checked above");
    let stderr = stderr_result.expect("stderr precheck result checked above");
    let stdout = String::from_utf8_lossy(&stdout);
    let stderr = String::from_utf8_lossy(&stderr);
    let merged = format!("{}\n{}", stdout, stderr);
    let marker = "This program contains tests written using Google Test";

    if status.success() && merged.contains(marker) {
        return Ok(GtestPrecheck::Valid);
    }

    Ok(GtestPrecheck::Invalid(format!(
        "dunitest: '{}' 不是有效 gtest 测例，缺少 gtest 标识文本。\n预检查退出码: {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        spec.path,
        status.code(),
        stdout,
        stderr
    )))
}

fn gtest_precheck_timeout(case_timeout_sec: u64) -> Duration {
    Duration::from_secs(case_timeout_sec.max(1)).min(GTEST_PRECHECK_MAX_TIMEOUT)
}

fn spawn_pipe_collector<R>(mut reader: R) -> JoinHandle<Result<Vec<u8>>>
where
    R: Read + Send + 'static,
{
    thread::spawn(move || -> Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut buf = [0_u8; 4096];
        loop {
            let n = reader
                .read(&mut buf)
                .with_context(|| "读取预检查管道失败")?;
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n]);
        }
        Ok(out)
    })
}

fn join_pipe_collector(handle: JoinHandle<Result<Vec<u8>>>) -> Result<Vec<u8>> {
    match handle.join() {
        Ok(inner) => inner,
        Err(_) => anyhow::bail!("预检查输出收集线程发生 panic"),
    }
}

fn join_pipe_collector_bounded(
    handle: JoinHandle<Result<Vec<u8>>>,
    timeout: Duration,
) -> Result<Vec<u8>> {
    let start = Instant::now();
    while !handle.is_finished() {
        if start.elapsed() >= timeout {
            anyhow::bail!("预检查输出收集线程未在 {:?} 内退出", timeout);
        }
        thread::sleep(Duration::from_millis(10));
    }
    join_pipe_collector(handle)
}

fn spawn_pipe_forwarder<R>(
    mut reader: R,
    shared_log: Arc<Mutex<File>>,
    is_stderr: bool,
) -> JoinHandle<Result<()>>
where
    R: Read + Send + 'static,
{
    thread::spawn(move || -> Result<()> {
        let mut buf = [0_u8; 4096];
        loop {
            let n = reader
                .read(&mut buf)
                .with_context(|| "读取子进程管道失败")?;
            if n == 0 {
                break;
            }

            if is_stderr {
                let mut term = std::io::stderr().lock();
                term.write_all(&buf[..n])
                    .with_context(|| "写入终端 stderr 失败")?;
                term.flush().with_context(|| "刷新终端 stderr 失败")?;
            } else {
                let mut term = std::io::stdout().lock();
                term.write_all(&buf[..n])
                    .with_context(|| "写入终端 stdout 失败")?;
                term.flush().with_context(|| "刷新终端 stdout 失败")?;
            }

            let mut log = shared_log
                .lock()
                .map_err(|_| anyhow::anyhow!("日志锁已损坏"))?;
            log.write_all(&buf[..n])
                .with_context(|| "写入日志文件失败")?;
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

fn join_pipe_forwarder_bounded(handle: JoinHandle<Result<()>>, timeout: Duration) -> Result<()> {
    let start = Instant::now();
    while !handle.is_finished() {
        if start.elapsed() >= timeout {
            anyhow::bail!("输出转发线程在 {:?} 内未结束", timeout);
        }
        thread::sleep(Duration::from_millis(20));
    }
    join_pipe_forwarder(handle)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::manifest::TestSpec;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn gtest_precheck_timeout_is_bounded_but_not_too_aggressive() {
        assert_eq!(gtest_precheck_timeout(0), Duration::from_secs(1));
        assert_eq!(gtest_precheck_timeout(1), Duration::from_secs(1));
        assert_eq!(gtest_precheck_timeout(5), Duration::from_secs(5));
        assert_eq!(gtest_precheck_timeout(60), Duration::from_secs(15));
    }

    #[test]
    fn timeout_kills_forked_descendants_that_keep_pipes_open() {
        let tmp = std::env::temp_dir().join(format!(
            "dunitest-runner-pgrp-{}-{}",
            std::process::id(),
            unique_nanos()
        ));
        let results = tmp.join("results");
        fs::create_dir_all(&results).unwrap();

        let script = tmp.join("forking_gtest.sh");
        fs::write(
            &script,
            r#"#!/bin/sh
if [ "$1" = "--gtest_help" ]; then
  echo "This program contains tests written using Google Test"
  exit 0
fi
echo "[==========] Running 1 test from 1 test suite."
echo "[ RUN      ] Timeout.KeepsPipeOpen"
(sleep 30) &
sleep 30
"#,
        )
        .unwrap();
        let mut perms = fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script, perms).unwrap();

        let spec = TestSpec {
            name: "normal/forking_timeout".to_string(),
            path: script.display().to_string(),
            args: Vec::new(),
            timeout_sec: 1,
        };

        let started = Instant::now();
        let result = run_test(&spec, &results, false).unwrap();

        assert!(matches!(result.status, CaseStatus::Timeout));
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "timeout path did not return promptly"
        );

        let _ = fs::remove_dir_all(tmp);
    }

    #[test]
    fn precheck_timeout_kills_forked_descendants_that_keep_pipes_open() {
        let tmp = std::env::temp_dir().join(format!(
            "dunitest-runner-precheck-pgrp-{}-{}",
            std::process::id(),
            unique_nanos()
        ));
        let results = tmp.join("results");
        fs::create_dir_all(&results).unwrap();

        let script = tmp.join("forking_gtest_help.sh");
        fs::write(
            &script,
            r#"#!/bin/sh
if [ "$1" = "--gtest_help" ]; then
  (sleep 30) &
  sleep 30
fi
echo "[==========] Running 1 test from 1 test suite."
echo "[  PASSED  ] 1 test."
"#,
        )
        .unwrap();
        let mut perms = fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script, perms).unwrap();

        let spec = TestSpec {
            name: "normal/forking_precheck_timeout".to_string(),
            path: script.display().to_string(),
            args: Vec::new(),
            timeout_sec: 1,
        };

        let started = Instant::now();
        let result = run_test(&spec, &results, false).unwrap();

        assert!(matches!(result.status, CaseStatus::Timeout));
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "precheck timeout path did not return promptly"
        );

        let _ = fs::remove_dir_all(tmp);
    }

    #[test]
    fn precheck_success_reaps_descendants_that_keep_pipes_open() {
        let tmp = std::env::temp_dir().join(format!(
            "dunitest-runner-precheck-success-pgrp-{}-{}",
            std::process::id(),
            unique_nanos()
        ));
        let results = tmp.join("results");
        fs::create_dir_all(&results).unwrap();

        let script = tmp.join("forking_gtest_help_success.sh");
        fs::write(
            &script,
            r#"#!/bin/sh
if [ "$1" = "--gtest_help" ]; then
  echo "This program contains tests written using Google Test"
  (sleep 30) &
  exit 0
fi
echo "[==========] Running 1 test from 1 test suite."
echo "[ RUN      ] Smoke.Pass"
echo "[       OK ] Smoke.Pass"
echo "[==========] 1 test from 1 test suite ran."
echo "[  PASSED  ] 1 test."
"#,
        )
        .unwrap();
        let mut perms = fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script, perms).unwrap();

        let spec = TestSpec {
            name: "normal/forking_precheck_success".to_string(),
            path: script.display().to_string(),
            args: Vec::new(),
            timeout_sec: 1,
        };

        let started = Instant::now();
        let result = run_test(&spec, &results, false).unwrap();

        assert!(matches!(result.status, CaseStatus::Passed));
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "precheck success path did not clean descendant pipe holders"
        );

        let _ = fs::remove_dir_all(tmp);
    }

    #[test]
    fn success_path_kills_forked_descendants_that_keep_pipes_open() {
        let tmp = std::env::temp_dir().join(format!(
            "dunitest-runner-success-pgrp-{}-{}",
            std::process::id(),
            unique_nanos()
        ));
        let results = tmp.join("results");
        fs::create_dir_all(&results).unwrap();

        let script = tmp.join("forking_gtest_success.sh");
        fs::write(
            &script,
            r#"#!/bin/sh
if [ "$1" = "--gtest_help" ]; then
  echo "This program contains tests written using Google Test"
  exit 0
fi
echo "[==========] Running 1 test from 1 test suite."
echo "[ RUN      ] Smoke.Pass"
(sleep 30) &
echo "[       OK ] Smoke.Pass"
echo "[==========] 1 test from 1 test suite ran."
echo "[  PASSED  ] 1 test."
exit 0
"#,
        )
        .unwrap();
        let mut perms = fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&script, perms).unwrap();

        let spec = TestSpec {
            name: "normal/forking_success".to_string(),
            path: script.display().to_string(),
            args: Vec::new(),
            timeout_sec: 5,
        };

        let started = Instant::now();
        let result = run_test(&spec, &results, false).unwrap();

        assert!(matches!(result.status, CaseStatus::Passed));
        assert_eq!(
            (
                result.gtest_total,
                result.gtest_passed,
                result.gtest_failed,
                result.gtest_skipped
            ),
            (1, 1, 0, 0)
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "success path did not clean descendant pipe holders"
        );

        let _ = fs::remove_dir_all(tmp);
    }

    fn unique_nanos() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }
}
