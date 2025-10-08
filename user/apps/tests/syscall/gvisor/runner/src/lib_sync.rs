use anyhow::Result;
use chrono::{DateTime, Local};
use std::{
    collections::HashSet,
    fs::{self, File},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Instant,
};

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
    pub tests_dir: PathBuf,
    pub blocklists_dir: PathBuf,
    pub results_dir: PathBuf,
    pub temp_dir: PathBuf,
    pub extra_blocklist_dirs: Vec<PathBuf>,
    pub test_patterns: Vec<String>,
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
            tests_dir: script_dir.join("tests"),
            blocklists_dir: script_dir.join("blocklists"),
            results_dir: script_dir.join("results"),
            temp_dir: PathBuf::from(
                std::env::var("SYSCALL_TEST_WORKDIR")
                    .unwrap_or_else(|_| "/tmp/gvisor_tests".to_string()),
            ),
            extra_blocklist_dirs: Vec::new(),
            test_patterns: Vec::new(),
        }
    }
}

/// 颜色输出辅助函数（简化版）
pub fn print_colored(color: &str, prefix: &str, msg: &str) {
    match color {
        "green" => println!("\x1b[32m[{}]\x1b[0m {}", prefix, msg),
        "yellow" => println!("\x1b[33m[{}]\x1b[0m {}", prefix, msg),
        "red" => eprintln!("\x1b[31m[{}]\x1b[0m {}", prefix, msg),
        "blue" => println!("\x1b[34m[{}]\x1b[0m {}", prefix, msg),
        _ => println!("[{}] {}", prefix, msg),
    }
}

/// gvisor系统调用测试运行器
pub struct TestRunner {
    pub config: Config,
    pub stats: Arc<TestStats>,
}

impl TestRunner {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            stats: Arc::new(TestStats::default()),
        }
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

    /// 检查测试是否在白名单中
    fn is_test_whitelisted(&self, test_name: &str) -> bool {
        if !self.config.use_whitelist {
            return true;
        }

        match self.get_whitelist_tests() {
            Ok(whitelist) => whitelist.contains(test_name),
            Err(_) => false,
        }
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
        let mut candidate_tests = Vec::new();
        if self.config.use_whitelist {
            for test in &all_tests {
                if self.is_test_whitelisted(test) {
                    candidate_tests.push(test.clone());
                }
            }

            if candidate_tests.is_empty() {
                self.print_warn("没有测试通过白名单过滤");
                return Ok(Vec::new());
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
        if self.config.test_patterns.is_empty() {
            candidate_tests.sort();
            return Ok(candidate_tests);
        }

        // 根据模式过滤测试
        let mut filtered_tests = HashSet::new();
        for pattern in &self.config.test_patterns {
            for test in &candidate_tests {
                if test == pattern {
                    filtered_tests.insert(test.clone());
                }
            }
        }

        let mut result: Vec<_> = filtered_tests.into_iter().collect();
        result.sort();
        Ok(result)
    }

    /// 运行单个测试
    pub fn run_single_test(&self, test_name: &str) -> Result<bool> {
        println!("[DEBUG] 开始运行测试: {}", test_name);
        let test_path = self.config.tests_dir.join(test_name);
        println!("[DEBUG] 测试路径: {:?}", test_path);

        if !test_path.exists() || !test_path.is_file() {
            self.print_warn(&format!("测试不存在或不可执行: {}", test_name));
            return Ok(false);
        }

        self.print_test(&format!("运行测试用例: {}", test_name));

        // 获取blocklist
        let blocked_subtests = self.get_test_blocklist(test_name);

        // 结果输出文件（使用绝对路径，避免工作目录切换影响）
        let output_file = self
            .config
            .results_dir
            .join(format!("{}.output", test_name));

        println!("[DEBUG] 工作目录: {:?}", self.config.tests_dir);
        println!("[DEBUG] TEST_TMPDIR: {:?}", self.config.temp_dir);
        println!("[DEBUG] 直接执行: {:?}", test_path);
        if !blocked_subtests.is_empty() {
            println!("[DEBUG] gtest_filter: -{}", blocked_subtests.join(":"));
        }

        // 确保结果目录存在
        if let Err(e) = fs::create_dir_all(&self.config.results_dir) {
            self.print_error(&format!("创建结果目录失败: {}", e));
        }

        // 打开输出文件，并将 stdout/stderr 重定向到该文件
        let out = File::create(&output_file);
        if let Err(e) = out.as_ref() {
            self.print_error(&format!("创建输出文件失败: {:?}, 错误: {}", output_file, e));
        }
        let out = out?;
        let err = out.try_clone()?;

        // 构造并执行命令（不使用 shell，不捕获输出，不创建管道）
        let start_time = Instant::now();
        let mut cmd = Command::new(&test_path);
        if !blocked_subtests.is_empty() {
            cmd.arg(format!("--gtest_filter=-{}", blocked_subtests.join(":")));
        }

        let status = cmd
            .current_dir(&self.config.tests_dir)
            .env("TEST_TMPDIR", &self.config.temp_dir)
            .stdout(std::process::Stdio::from(out))
            .stderr(std::process::Stdio::from(err))
            .status();

        // 清理临时目录
        let _ = fs::remove_dir_all(&self.config.temp_dir);
        let _ = fs::create_dir_all(&self.config.temp_dir);

        let duration = start_time.elapsed();
        match status {
            Ok(s) if s.success() => {
                self.print_info(&format!(
                    "✓ {} 通过 ({:.2}s)",
                    test_name,
                    duration.as_secs_f64()
                ));
                // 将输出文件尾部打印一点，便于快速查看
                if let Ok(content) = fs::read_to_string(&output_file) {
                    let tail: String = content
                        .lines()
                        .rev()
                        .take(10)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .map(|s| format!("{}\n", s))
                        .collect();
                    if !tail.is_empty() {
                        println!("[DEBUG] 输出尾部: \n{}", tail);
                    }
                }
                Ok(true)
            }
            Ok(s) => {
                self.print_error(&format!(
                    "✗ {} 失败 ({:.2}s), 退出码: {:?}",
                    test_name,
                    duration.as_secs_f64(),
                    s.code()
                ));
                // 打印错误输出尾部
                if let Ok(content) = fs::read_to_string(&output_file) {
                    let tail: String = content
                        .lines()
                        .rev()
                        .take(20)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .map(|s| format!("{}\n", s))
                        .collect();
                    if !tail.is_empty() {
                        println!("[DEBUG] 错误输出尾部: \n{}", tail);
                    }
                }
                Ok(false)
            }
            Err(e) => {
                self.print_error(&format!("✗ {} 执行错误: {}", test_name, e));
                Ok(false)
            }
        }
    }

    /// 运行所有测试
    pub fn run_all_tests(&self) -> Result<()> {
        let test_list = self.get_test_list()?;

        if test_list.is_empty() {
            self.print_warn("没有找到匹配的测试用例");
            return Ok(());
        }

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

            println!("---");
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
        println!("{}", report);

        if failed > 0 {
            let failed_cases_file = self.config.results_dir.join("failed_cases.txt");
            if failed_cases_file.exists() {
                let failed_content = fs::read_to_string(&failed_cases_file)?;
                let failed_section = format!("失败的测试用例:\n{}", failed_content);
                file.write_all(failed_section.as_bytes())?;
                println!("{}", failed_section);
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
                        if self.is_test_whitelisted(&file_name_str) {
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
