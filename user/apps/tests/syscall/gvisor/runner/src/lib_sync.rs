use crate::gtest_xml::parse_gtest_xml;
use anyhow::{Context, Result};
use chrono::{DateTime, Local};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{BufRead, BufReader, Write},
    os::unix::{
        fs::PermissionsExt,
        process::{CommandExt, ExitStatusExt},
    },
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

macro_rules! safe_println {
    ($($arg:tt)*) => {{
        // Rust's println!/eprintln! panic when DragonOS returns a transient
        // console error. Test result collection must remain fail-closed and
        // continue to the next binary even when diagnostic output is lost.
        let stdout = std::io::stdout();
        let mut lock = stdout.lock();
        let _ = writeln!(lock, $($arg)*);
    }};
}

/// 测试统计信息
#[derive(Debug, Default)]
pub struct TestStats {
    total: AtomicUsize,
    passed: AtomicUsize,
    failed: AtomicUsize,
    skipped: AtomicUsize,
}

impl TestStats {
    pub fn increment_total(&self) {
        self.total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn increment_passed(&self) {
        self.passed.fetch_add(1, Ordering::Relaxed);
    }

    pub fn increment_failed(&self) {
        self.failed.fetch_add(1, Ordering::Relaxed);
    }

    #[allow(dead_code)]
    pub fn increment_skipped(&self) {
        self.skipped.fetch_add(1, Ordering::Relaxed);
    }

    pub fn get_totals(&self) -> (usize, usize, usize, usize) {
        (
            self.total.load(Ordering::Relaxed),
            self.passed.load(Ordering::Relaxed),
            self.failed.load(Ordering::Relaxed),
            self.skipped.load(Ordering::Relaxed),
        )
    }
}

/// 测试运行器配置
#[derive(Debug, Clone)]
pub struct Config {
    pub verbose: bool,
    pub timeout: u64,
    pub parallel: usize,
    pub use_blocklist: bool,
    pub use_whitelist: bool,
    pub whitelist_file: PathBuf,
    pub required_tests_file: PathBuf,
    pub tests_dir: PathBuf,
    pub blocklists_dir: PathBuf,
    pub results_dir: PathBuf,
    pub temp_dir: PathBuf,
    pub extra_blocklist_dirs: Vec<PathBuf>,
    pub test_patterns: Vec<String>,
    pub output_to_stdout: bool, // 是否输出到控制台而不是文件
    pub enforce_required: bool,
}

impl Default for Config {
    fn default() -> Self {
        let script_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self {
            verbose: false,
            timeout: 300,
            parallel: 1,
            use_blocklist: true,
            use_whitelist: true,
            whitelist_file: script_dir.join("whitelist.txt"),
            required_tests_file: script_dir.join("required_tests.txt"),
            tests_dir: script_dir.join("tests"),
            blocklists_dir: script_dir.join("blocklists"),
            results_dir: script_dir.join("results"),
            temp_dir: PathBuf::from(
                std::env::var("SYSCALL_TEST_WORKDIR")
                    .unwrap_or_else(|_| "/tmp/gvisor_tests".to_string()),
            ),
            extra_blocklist_dirs: Vec::new(),
            test_patterns: Vec::new(),
            output_to_stdout: true,
            enforce_required: true,
        }
    }
}

/// 颜色输出辅助函数（简化版）
pub fn print_colored(color: &str, prefix: &str, msg: &str) {
    match color {
        "green" => safe_println!("\x1b[32m[{}]\x1b[0m {}", prefix, msg),
        "yellow" => safe_println!("\x1b[33m[{}]\x1b[0m {}", prefix, msg),
        "red" => safe_println!("\x1b[31m[{}]\x1b[0m {}", prefix, msg),
        "blue" => safe_println!("\x1b[34m[{}]\x1b[0m {}", prefix, msg),
        _ => safe_println!("[{}] {}", prefix, msg),
    }
}

/// gvisor系统调用测试运行器
pub struct TestRunner {
    pub config: Config,
    pub stats: Arc<TestStats>,
    required_tests: HashMap<String, RequiredTest>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RequiredTest {
    case_count: usize,
    allowed_skips: HashSet<String>,
}

impl TestRunner {
    pub fn new(config: Config) -> Result<Self> {
        let required_tests = if config.enforce_required {
            read_required_tests(&config.required_tests_file)?
        } else {
            HashMap::new()
        };
        Ok(Self {
            config,
            stats: Arc::new(TestStats::default()),
            required_tests,
        })
    }

    /// 打印信息日志
    fn print_info(&self, msg: &str) {
        print_colored("green", "INFO", msg);
    }

    /// 打印警告日志
    fn print_warn(&self, msg: &str) {
        print_colored("yellow", "WARN", msg);
    }

    /// 打印错误日志
    fn print_error(&self, msg: &str) {
        print_colored("red", "ERROR", msg);
    }

    /// 打印测试日志
    fn print_test(&self, msg: &str) {
        print_colored("blue", "TEST", msg);
    }

    /// 检查测试套件是否存在
    pub fn check_test_suite(&self) -> Result<()> {
        if !self.config.tests_dir.exists() {
            anyhow::bail!(
                "测试目录不存在: {:?}\n请先运行 ./download_tests.sh 下载测试套件",
                self.config.tests_dir
            );
        }

        let test_files: Vec<_> = fs::read_dir(&self.config.tests_dir)?
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry.path().is_file() && entry.file_name().to_string_lossy().ends_with("_test")
            })
            .collect();

        if test_files.is_empty() {
            anyhow::bail!("测试套件未找到\n请先运行 ./download_tests.sh 下载测试套件");
        }

        Ok(())
    }

    /// 创建必要的目录
    pub fn setup_directories(&self) -> Result<()> {
        fs::create_dir_all(&self.config.results_dir)?;
        fs::create_dir_all(&self.config.temp_dir)?;
        fs::create_dir_all(&self.config.blocklists_dir)?;
        Ok(())
    }

    /// 读取白名单中的测试程序
    fn get_whitelist_tests(&self) -> Result<HashSet<String>> {
        if !self.config.whitelist_file.exists() {
            anyhow::bail!("白名单文件不存在: {:?}", self.config.whitelist_file);
        }

        let file = File::open(&self.config.whitelist_file)?;
        let reader = BufReader::new(file);
        let mut tests = HashSet::new();

        for line in reader.lines() {
            let line = line?;
            let line = line.trim();
            if !line.is_empty() && !line.starts_with('#') {
                tests.insert(line.to_string());
            }
        }

        Ok(tests)
    }

    /// 获取测试的blocklist
    fn get_test_blocklist(&self, test_name: &str) -> Vec<String> {
        if !self.config.use_blocklist {
            return Vec::new();
        }

        let mut blocked_subtests = Vec::new();

        // 检查主blocklist目录
        let blocklist_file = self.config.blocklists_dir.join(test_name);
        if let Ok(content) = self.read_blocklist_file(&blocklist_file) {
            blocked_subtests.extend(content);
        }

        // 检查额外的blocklist目录
        for extra_dir in &self.config.extra_blocklist_dirs {
            let extra_blocklist = extra_dir.join(test_name);
            if let Ok(content) = self.read_blocklist_file(&extra_blocklist) {
                blocked_subtests.extend(content);
            }
        }

        blocked_subtests
    }

    /// 读取blocklist文件
    fn read_blocklist_file(&self, path: &Path) -> Result<Vec<String>> {
        if !path.exists() {
            return Ok(Vec::new());
        }

        let file = File::open(path)?;
        let reader = BufReader::new(file);
        let mut blocked = Vec::new();

        for line in reader.lines() {
            let line = line?;
            let line = line.trim();
            if !line.is_empty() && !line.starts_with('#') {
                blocked.push(line.to_string());
            }
        }

        Ok(blocked)
    }

    /// 获取要运行的测试列表
    pub fn get_test_list(&self) -> Result<Vec<String>> {
        // 获取所有测试文件
        let mut all_tests = Vec::new();
        for entry in fs::read_dir(&self.config.tests_dir)? {
            let entry = entry?;
            if entry.path().is_file() {
                let file_name = entry.file_name();
                let file_name_str = file_name.to_string_lossy();
                if file_name_str.ends_with("_test") {
                    all_tests.push(file_name_str.to_string());
                }
            }
        }

        // 应用白名单过滤
        let all_test_names: HashSet<_> = all_tests.iter().cloned().collect();
        let mut candidate_tests = Vec::new();
        if self.config.use_whitelist {
            let whitelist = self.get_whitelist_tests()?;
            for test in &all_tests {
                if whitelist.contains(test) {
                    candidate_tests.push(test.clone());
                }
            }

            for stale in whitelist.difference(&all_test_names) {
                if !self.required_tests.contains_key(stale) {
                    self.print_warn(&format!("白名单测试在发布包中不存在，已忽略: {}", stale));
                }
            }

            if self.config.enforce_required {
                for required in self.required_tests.keys() {
                    if !whitelist.contains(required) {
                        anyhow::bail!("required 测试未进入默认白名单: {}", required);
                    }
                    self.validate_required_binary(required)?;
                }
            }

            if self.config.verbose {
                self.print_info(&format!(
                    "白名单过滤后有 {} 个测试可用",
                    candidate_tests.len()
                ));
            }
        } else {
            candidate_tests = all_tests;
        }

        // 如果没有指定模式，返回候选测试
        let mut result = if self.config.test_patterns.is_empty() {
            candidate_tests
        } else {
            let mut filtered_tests = HashSet::new();
            for pattern in &self.config.test_patterns {
                for test in &candidate_tests {
                    if test == pattern {
                        filtered_tests.insert(test.clone());
                    }
                }
            }
            filtered_tests.into_iter().collect()
        };

        result.sort();
        if result.is_empty() {
            anyhow::bail!("没有找到匹配的测试用例");
        }
        if self.config.enforce_required {
            let selected: HashSet<_> = result.iter().cloned().collect();
            for required in self.required_tests.keys() {
                if !selected.contains(required) {
                    anyhow::bail!("默认执行列表缺少 required 测试: {}", required);
                }
            }
        }
        Ok(result)
    }

    fn validate_required_binary(&self, test_name: &str) -> Result<()> {
        let path = self.config.tests_dir.join(test_name);
        let metadata = fs::metadata(&path)
            .with_context(|| format!("required 测试不存在: {}", path.display()))?;
        if !metadata.is_file() {
            anyhow::bail!("required 测试不是普通文件: {}", path.display());
        }
        if metadata.permissions().mode() & 0o111 == 0 {
            anyhow::bail!("required 测试不可执行: {}", path.display());
        }
        Ok(())
    }

    /// 运行单个测试
    pub fn run_single_test(&self, test_name: &str) -> Result<bool> {
        safe_println!("[DEBUG] 开始运行测试: {}", test_name);
        let test_path = self.config.tests_dir.join(test_name);
        safe_println!("[DEBUG] 测试路径: {:?}", test_path);

        if !test_path.exists() || !test_path.is_file() {
            self.print_warn(&format!("测试不存在或不可执行: {}", test_name));
            return Ok(false);
        }

        self.print_test(&format!("运行测试用例: {}", test_name));

        // 获取blocklist
        let blocked_subtests = self.get_test_blocklist(test_name);
        if self.required_tests.contains_key(test_name) && !blocked_subtests.is_empty() {
            anyhow::bail!("required 测试禁止 blocklist: {}", test_name);
        }

        let xml_path = self.config.results_dir.join(format!("{}.xml", test_name));
        remove_stale_xml(&xml_path)?;

        safe_println!("[DEBUG] 工作目录: {:?}", self.config.tests_dir);
        safe_println!("[DEBUG] TEST_TMPDIR: {:?}", self.config.temp_dir);
        safe_println!("[DEBUG] 直接执行: {:?}", test_path);
        if !blocked_subtests.is_empty() {
            safe_println!("[DEBUG] gtest_filter: -{}", blocked_subtests.join(":"));
        }

        // 根据配置决定输出方式
        let (stdout, stderr) = if self.config.output_to_stdout {
            // 单个测例：直接输出到控制台
            (
                std::process::Stdio::inherit(),
                std::process::Stdio::inherit(),
            )
        } else {
            // 批量测试：输出到文件
            // 确保结果目录存在
            if let Err(e) = fs::create_dir_all(&self.config.results_dir) {
                self.print_error(&format!("创建结果目录失败: {}", e));
            }

            // 结果输出文件（使用绝对路径，避免工作目录切换影响）
            let output_file = self
                .config
                .results_dir
                .join(format!("{}.output", test_name));

            // 打开输出文件，并将 stdout/stderr 重定向到该文件
            let out = File::create(&output_file);
            if let Err(e) = out.as_ref() {
                self.print_error(&format!("创建输出文件失败: {:?}, 错误: {}", output_file, e));
            }
            let out = out?;
            let err = out.try_clone()?;
            (
                std::process::Stdio::from(out),
                std::process::Stdio::from(err),
            )
        };

        // 构造并执行命令（不使用 shell，不捕获输出，不创建管道）
        let start_time = Instant::now();
        let mut cmd = Command::new(&test_path);
        cmd.arg(format!("--gtest_output=xml:{}", xml_path.display()));
        if !blocked_subtests.is_empty() {
            cmd.arg(format!("--gtest_filter=-{}", blocked_subtests.join(":")));
        }
        // Run each test binary in its own process group so timeout cleanup also
        // terminates descendants. Network tests additionally get a fresh
        // namespace to avoid sysctl leakage.
        let name_lc = test_name.to_ascii_lowercase();
        let isolate_network = name_lc.contains("socket") || name_lc.contains("net");
        unsafe {
            cmd.pre_exec(move || {
                if libc::setpgid(0, 0) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if isolate_network {
                    let ret = libc::unshare(libc::CLONE_NEWNET);
                    if ret != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }

        cmd.current_dir(&self.config.tests_dir)
            .env("TEST_TMPDIR", &self.config.temp_dir)
            .stdout(stdout)
            .stderr(stderr);
        let status = wait_with_timeout(&mut cmd, Duration::from_secs(self.config.timeout));

        // 清理临时目录
        let _ = fs::remove_dir_all(&self.config.temp_dir);
        let _ = fs::create_dir_all(&self.config.temp_dir);

        let duration = start_time.elapsed();
        let process_ok = match status {
            Ok(TestProcessOutcome::Exited(ref status)) if status.success() => true,
            Ok(TestProcessOutcome::Exited(ref status)) => {
                self.print_error(&format!(
                    "✗ {} 进程失败 ({:.2}s), 退出码: {:?}, 信号: {:?}",
                    test_name,
                    duration.as_secs_f64(),
                    status.code(),
                    status.signal()
                ));
                false
            }
            Ok(TestProcessOutcome::TimedOut) => {
                self.print_error(&format!(
                    "✗ {} 超时 ({:.2}s), 上限: {} 秒",
                    test_name,
                    duration.as_secs_f64(),
                    self.config.timeout
                ));
                false
            }
            Err(ref error) => {
                self.print_error(&format!("✗ {} 执行错误: {}", test_name, error));
                false
            }
        };

        let xml_ok = match fs::symlink_metadata(&xml_path) {
            Ok(metadata) if metadata.file_type().is_file() => match parse_gtest_xml(&xml_path) {
                Ok(report) => {
                    safe_println!(
                        "[GTEST_XML] binary={} total={} failures={} errors={} disabled={} skipped={}",
                        test_name,
                        report.total,
                        report.failures,
                        report.errors,
                        report.disabled,
                        report.skipped
                    );
                    match self.required_tests.get(test_name) {
                        Some(required) => match report.validate_required(
                            test_name,
                            required.case_count,
                            &required.allowed_skips,
                        ) {
                            Ok(()) => true,
                            Err(error) => {
                                self.print_error(&format!("required 结果不合格: {:#}", error));
                                false
                            }
                        },
                        None => true,
                    }
                }
                Err(error) => {
                    self.print_error(&format!("gtest XML 无效: {:#}", error));
                    false
                }
            },
            Ok(_) => {
                self.print_error(&format!("gtest XML 不是普通文件: {}", xml_path.display()));
                false
            }
            Err(error) => {
                self.print_error(&format!(
                    "本轮未生成 gtest XML: {}: {}",
                    xml_path.display(),
                    error
                ));
                false
            }
        };

        if process_ok && xml_ok {
            self.print_info(&format!(
                "✓ {} 通过 ({:.2}s)",
                test_name,
                duration.as_secs_f64()
            ));
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// 运行所有测试
    pub fn run_all_tests(&self) -> Result<()> {
        let test_list = self.get_test_list()?;

        self.print_info(&format!("准备运行 {} 个测试用例", test_list.len()));

        // 初始化结果文件
        let failed_cases_file = self.config.results_dir.join("failed_cases.txt");
        let mut failed_cases = File::create(&failed_cases_file)?;

        // 运行测试
        for test_name in test_list {
            self.stats.increment_total();

            match self.run_single_test(&test_name) {
                Ok(true) => {
                    self.stats.increment_passed();
                }
                Ok(false) => {
                    self.stats.increment_failed();
                    writeln!(failed_cases, "{}", test_name)?;
                }
                Err(e) => {
                    self.stats.increment_failed();
                    writeln!(failed_cases, "{}", test_name)?;
                    self.print_error(&format!("测试 {} 出错: {}", test_name, e));
                }
            }

            safe_println!("---");
        }

        Ok(())
    }

    /// 生成测试报告
    pub fn generate_report(&self) -> Result<()> {
        let report_file = self.config.results_dir.join("test_report.txt");
        let mut file = File::create(&report_file)?;
        let (total, passed, failed, _skipped) = self.stats.get_totals();

        let now: DateTime<Local> = Local::now();
        let success_rate = if total > 0 {
            passed as f64 * 100.0 / total as f64
        } else {
            0.0
        };

        let report = format!(
            "gvisor系统调用测试报告\n\
            ==========================\n\
            测试时间: {}\n\
            测试目录: {:?}\n\
            \n\
            测试统计:\n\
            总测试数: {}\n\
            通过: {}\n\
            失败: {}\n\
            成功率: {:.2}%\n\
            \n",
            now.format("%Y-%m-%d %H:%M:%S"),
            self.config.tests_dir,
            total,
            passed,
            failed,
            success_rate
        );

        file.write_all(report.as_bytes())?;
        safe_println!("{}", report);

        if failed > 0 {
            let failed_cases_file = self.config.results_dir.join("failed_cases.txt");
            if failed_cases_file.exists() {
                let failed_content = fs::read_to_string(&failed_cases_file)?;
                let failed_section = format!("失败的测试用例:\n{}", failed_content);
                file.write_all(failed_section.as_bytes())?;
                safe_println!("{}", failed_section);
            }
        }

        Ok(())
    }

    /// 显示测试结果
    pub fn show_results(&self) {
        let (total, passed, failed, _skipped) = self.stats.get_totals();

        log::info!("");
        log::info!("===============================================");
        self.print_info("测试完成");
        log::info!(
            "\x1b[32m{}\x1b[0m / \x1b[32m{}\x1b[0m 测试用例通过",
            passed,
            total
        );

        if failed > 0 {
            log::info!("\x1b[31m{}\x1b[0m 个测试用例失败:", failed);
            let failed_cases_file = self.config.results_dir.join("failed_cases.txt");
            if let Ok(content) = fs::read_to_string(&failed_cases_file) {
                for line in content.lines() {
                    if !line.trim().is_empty() {
                        log::info!("  \x1b[31m[X]\x1b[0m {}", line);
                    }
                }
            }
        }

        log::info!("");
        log::info!(
            "详细报告保存在: {:?}",
            self.config.results_dir.join("test_report.txt")
        );
    }

    /// 列出所有测试用例
    pub fn list_tests(&self) -> Result<()> {
        if !self.config.tests_dir.exists() {
            self.print_error(&format!("测试目录不存在: {:?}", self.config.tests_dir));
            self.print_info("请先运行 ./download_tests.sh 下载测试套件");
            return Ok(());
        }

        if self.config.use_whitelist {
            let whitelist = self.get_whitelist_tests()?;
            self.print_info(&format!(
                "白名单模式 - 可运行的测试用例 (来自: {:?}):",
                self.config.whitelist_file
            ));
            let test_list = self.get_test_list()?;
            for test_name in &test_list {
                log::info!("  \x1b[32m✓\x1b[0m {}", test_name);
            }

            self.print_info("所有可用测试用例 (包括未在白名单中的):");
            for entry in fs::read_dir(&self.config.tests_dir)? {
                let entry = entry?;
                if entry.path().is_file() {
                    let file_name = entry.file_name();
                    let file_name_str = file_name.to_string_lossy();
                    if file_name_str.ends_with("_test") {
                        if whitelist.contains(file_name_str.as_ref()) {
                            log::info!("  \x1b[32m✓\x1b[0m {} (在白名单中)", file_name_str);
                        } else {
                            log::info!("  \x1b[33m○\x1b[0m {} (不在白名单中)", file_name_str);
                        }
                    }
                }
            }
        } else {
            self.print_info("所有可用的测试用例:");
            for entry in fs::read_dir(&self.config.tests_dir)? {
                let entry = entry?;
                if entry.path().is_file() {
                    let file_name = entry.file_name();
                    let file_name_str = file_name.to_string_lossy();
                    if file_name_str.ends_with("_test") {
                        log::info!("  {}", file_name_str);
                    }
                }
            }
        }

        Ok(())
    }
}

enum TestProcessOutcome {
    Exited(ExitStatus),
    TimedOut,
}

fn terminate_process_group(child: &mut Child) -> Result<()> {
    let pgid = -(child.id() as libc::pid_t);
    if unsafe { libc::kill(pgid, libc::SIGKILL) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error).context("终止 gVisor 测试进程组失败");
        }
    }
    let _ = child.kill();
    child.wait().context("回收 gVisor 测试进程失败")?;
    Ok(())
}

fn wait_with_timeout(cmd: &mut Command, timeout: Duration) -> Result<TestProcessOutcome> {
    let mut child = cmd.spawn().context("启动 gVisor 测试进程失败")?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(TestProcessOutcome::Exited(status)),
            Ok(None) => {}
            Err(wait_error) => {
                terminate_process_group(&mut child).with_context(|| {
                    format!("等待 gVisor 测试进程失败后清理子进程: {wait_error}")
                })?;
                return Err(wait_error).context("等待 gVisor 测试进程失败");
            }
        }
        if started.elapsed() >= timeout {
            terminate_process_group(&mut child).context("清理超时 gVisor 测试进程失败")?;
            return Ok(TestProcessOutcome::TimedOut);
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn read_required_tests(path: &Path) -> Result<HashMap<String, RequiredTest>> {
    let file = File::open(path)
        .with_context(|| format!("required 测试清单不存在或不可读: {}", path.display()))?;
    let mut required = HashMap::new();
    for (index, line) in BufReader::new(file).lines().enumerate() {
        let line = line.with_context(|| format!("读取 required 清单第 {} 行失败", index + 1))?;
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() < 2 {
            anyhow::bail!(
                "required 清单第 {} 行必须是: binary case_count [allowed_skip ...]",
                index + 1
            );
        }
        let name = fields[0];
        if !name.ends_with("_test")
            || name.contains('/')
            || name.contains('\\')
            || name == "."
            || name == ".."
        {
            anyhow::bail!("required 清单第 {} 行包含非法二进制名: {}", index + 1, name);
        }
        let count = fields[1]
            .parse::<usize>()
            .with_context(|| format!("required 清单第 {} 行用例数非法", index + 1))?;
        if count == 0 {
            anyhow::bail!("required 清单第 {} 行用例数不能为 0", index + 1);
        }
        let mut allowed_skips = HashSet::new();
        for allowed in &fields[2..] {
            if !allowed.contains('.') || !allowed_skips.insert((*allowed).to_string()) {
                anyhow::bail!(
                    "required 清单第 {} 行包含非法或重复 allowed_skip: {}",
                    index + 1,
                    allowed
                );
            }
        }
        let spec = RequiredTest {
            case_count: count,
            allowed_skips,
        };
        if required.insert(name.to_string(), spec).is_some() {
            anyhow::bail!("required 清单包含重复二进制: {}", name);
        }
    }
    if required.is_empty() {
        anyhow::bail!("required 测试清单为空: {}", path.display());
    }
    Ok(required)
}

fn remove_stale_xml(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("无法删除旧 gtest XML: {}", path.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        read_required_tests, remove_stale_xml, wait_with_timeout, Config, TestProcessOutcome,
        TestRunner,
    };
    use std::{
        fs,
        os::unix::process::CommandExt,
        process::Command,
        time::Duration,
        time::{SystemTime, UNIX_EPOCH},
    };

    fn manifest(
        content: &str,
    ) -> anyhow::Result<std::collections::HashMap<String, super::RequiredTest>> {
        let path = std::env::temp_dir().join(format!(
            "gvisor-required-{}-{}.txt",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(&path, content).unwrap();
        let result = read_required_tests(&path);
        let _ = fs::remove_file(path);
        result
    }

    #[test]
    fn parses_required_manifest() {
        let required =
            manifest("# pinned\nmount_test 87\npivot_root_test 19 PivotRootTest.OnRootFS\n")
                .unwrap();
        assert_eq!(required["mount_test"].case_count, 87);
        assert!(required["mount_test"].allowed_skips.is_empty());
        assert_eq!(required["pivot_root_test"].case_count, 19);
        assert!(required["pivot_root_test"]
            .allowed_skips
            .contains("PivotRootTest.OnRootFS"));
    }

    #[test]
    fn rejects_duplicate_or_path_entries() {
        assert!(manifest("mount_test 87\nmount_test 1\n").is_err());
        assert!(manifest("../mount_test 87\n").is_err());
        assert!(manifest("mount_test 0\n").is_err());
        assert!(manifest("mount_test 87 invalid_skip\n").is_err());
        assert!(manifest("mount_test 87 Suite.Skip Suite.Skip\n").is_err());
    }

    #[test]
    fn disabled_required_enforcement_does_not_read_manifest() {
        let mut config = Config::default();
        config.enforce_required = false;
        config.required_tests_file = std::env::temp_dir().join(format!(
            "missing-gvisor-required-{}-{}.txt",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));

        let runner = TestRunner::new(config).unwrap();
        assert!(runner.required_tests.is_empty());
    }

    #[test]
    fn removes_stale_xml_before_a_new_run() {
        let path = std::env::temp_dir().join(format!(
            "gvisor-stale-{}-{}.xml",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(&path, "<old-success />").unwrap();
        remove_stale_xml(&path).unwrap();
        assert!(!path.exists());
        remove_stale_xml(&path).unwrap();
    }

    #[test]
    fn timeout_kills_the_test_process_group() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "sleep 5"]);
        unsafe {
            cmd.pre_exec(|| {
                if libc::setpgid(0, 0) != 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
        let outcome = wait_with_timeout(&mut cmd, Duration::from_millis(50)).unwrap();
        assert!(matches!(outcome, TestProcessOutcome::TimedOut));
    }
}
