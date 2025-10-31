use anyhow::Result;
use clap::{Arg, Command};
use std::path::PathBuf;

mod lib_sync;
use lib_sync::{Config, TestRunner};

fn main() -> Result<()> {
    env_logger::Builder::from_default_env()
        .filter_level(log::LevelFilter::Info)
        .init();
    let app = Command::new("gvisor-test-runner")
        .version("0.1.0")
        .about("gvisor系统调用测试运行脚本 - Rust版本")
        .arg(
            Arg::new("help")
                .short('h')
                .long("help")
                .action(clap::ArgAction::Help)
                .help("显示此帮助信息"),
        )
        .arg(
            Arg::new("list")
                .short('l')
                .long("list")
                .action(clap::ArgAction::SetTrue)
                .help("列出所有可用的测试用例"),
        )
        .arg(
            Arg::new("verbose")
                .short('v')
                .long("verbose")
                .action(clap::ArgAction::SetTrue)
                .help("详细输出模式"),
        )
        .arg(
            Arg::new("timeout")
                .short('t')
                .long("timeout")
                .value_name("SEC")
                .help("设置单个测试的超时时间（默认：300秒）")
                .value_parser(clap::value_parser!(u64)),
        )
        .arg(
            Arg::new("parallel")
                .short('j')
                .long("parallel")
                .value_name("NUM")
                .help("并行运行的测试数量（默认：1）")
                .value_parser(clap::value_parser!(usize)),
        )
        .arg(
            Arg::new("no-blocklist")
                .long("no-blocklist")
                .action(clap::ArgAction::SetTrue)
                .help("忽略所有blocklist文件"),
        )
        .arg(
            Arg::new("extra-blocklist")
                .long("extra-blocklist")
                .value_name("DIR")
                .action(clap::ArgAction::Append)
                .help("指定额外的blocklist目录"),
        )
        .arg(
            Arg::new("no-whitelist")
                .long("no-whitelist")
                .action(clap::ArgAction::SetTrue)
                .help("禁用白名单模式，运行所有测试程序"),
        )
        .arg(
            Arg::new("whitelist")
                .long("whitelist")
                .value_name("FILE")
                .help("指定白名单文件路径（默认：whitelist.txt）"),
        )
        .arg(
            Arg::new("test-patterns")
                .value_name("PATTERN")
                .action(clap::ArgAction::Append)
                .help("测试名称模式"),
        )
        .arg(
            Arg::new("stdout")
                .long("stdout")
                .action(clap::ArgAction::SetTrue)
                .help("将测试输出直接显示到控制台，而不是保存到文件"),
        );

    let matches = app.get_matches();

    // 解析配置
    let mut config = Config::default();

    config.verbose = matches.get_flag("verbose");

    if let Some(timeout) = matches.get_one::<u64>("timeout") {
        config.timeout = *timeout;
    }

    if let Some(parallel) = matches.get_one::<usize>("parallel") {
        config.parallel = *parallel;
    }

    config.use_blocklist = !matches.get_flag("no-blocklist");
    config.use_whitelist = !matches.get_flag("no-whitelist");

    if let Some(whitelist_file) = matches.get_one::<String>("whitelist") {
        config.whitelist_file = PathBuf::from(whitelist_file);
    }

    if let Some(extra_dirs) = matches.get_many::<String>("extra-blocklist") {
        config.extra_blocklist_dirs = extra_dirs.map(|s| PathBuf::from(s)).collect();
    }

    if let Some(patterns) = matches.get_many::<String>("test-patterns") {
        config.test_patterns = patterns.cloned().collect();
    }

    // 设置输出方式
    config.output_to_stdout = matches.get_flag("stdout");

    // 创建测试运行器
    let runner = TestRunner::new(config);

    // 处理特殊命令
    if matches.get_flag("list") {
        runner.list_tests()?;
        return Ok(());
    }

    log::info!("==============================");
    log::info!("  gvisor系统调用测试运行器");
    log::info!("==============================");

    // 检查测试套件
    runner.check_test_suite()?;

    // 设置目录
    runner.setup_directories()?;

    log::info!("开始运行gvisor系统调用测试");

    // 显示运行配置
    if runner.config.use_whitelist {
        log::info!("白名单模式已启用: {:?}", runner.config.whitelist_file);
    }
    if !runner.config.use_blocklist {
        log::info!("黑名单已禁用");
    }

    // 运行测试
    runner.run_all_tests()?;

    // 生成报告
    runner.generate_report()?;

    // 显示结果
    runner.show_results();

    // 返回适当的退出码
    let (_, _, failed, _) = runner.stats.get_totals();
    if failed > 0 {
        std::process::exit(1);
    }

    Ok(())
}
