use dadk_config::common::target_arch::TargetArch;
use test_base::{
    global::BaseGlobalTestContext,
    test_context::{self as test_context, test_context},
};

use crate::{
    context::{
        DadkExecuteContextTestBuildRiscV64V1, DadkExecuteContextTestBuildX86_64V1, TestContextExt,
    },
    parser::Parser,
};

use super::*;

/// 不应在x86_64上运行仅限riscv64的任务
#[test_context(DadkExecuteContextTestBuildX86_64V1)]
#[test]
fn should_not_run_task_only_riscv64_on_x86_64(ctx: &DadkExecuteContextTestBuildX86_64V1) {
    let config_file = ctx
        .base_context()
        .config_v2_dir()
        .join("app_target_arch_riscv64_only_0_2_0.toml");
    let task =
        Parser::new(ctx.base_context().config_v2_dir(), None).parse_config_file(&config_file);
    assert!(task.is_ok(), "parse error: {:?}", task);
    let task = task.unwrap();
    assert!(
        task.target_arch.len() == 1,
        "target_arch length error: {:?}",
        task
    );
    assert!(
        task.target_arch[0] == TargetArch::RiscV64,
        "target_arch should be riscv64: {:?}",
        task
    );

    let scheduler = Scheduler::new(
        ctx.execute_context().self_ref().unwrap(),
        ctx.base_context().fake_dragonos_sysroot(),
        *ctx.execute_context().action(),
        vec![],
    );

    assert!(scheduler.is_ok(), "Create scheduler error: {:?}", scheduler);

    let mut scheduler = scheduler.unwrap();

    let entity = scheduler.add_task(config_file, task);
    assert!(
        entity.is_err(),
        "Add task should return error: {:?}",
        entity
    );
}

/// 不应在riscv64上运行仅限x86_64的任务
#[test_context(DadkExecuteContextTestBuildRiscV64V1)]
#[test]
fn should_not_run_task_only_x86_64_on_riscv64(ctx: &DadkExecuteContextTestBuildRiscV64V1) {
    let config_file = ctx
        .base_context()
        .config_v2_dir()
        .join("app_target_arch_x86_64_only_0_2_0.toml");
    let task =
        Parser::new(ctx.base_context().config_v2_dir(), None).parse_config_file(&config_file);
    assert!(task.is_ok(), "parse error: {:?}", task);
    let task = task.unwrap();
    assert!(
        task.target_arch.len() == 1,
        "target_arch length error: {:?}",
        task
    );
    assert!(
        task.target_arch[0] == TargetArch::X86_64,
        "target_arch should be x86_64: {:?}",
        task
    );

    let scheduler = Scheduler::new(
        ctx.execute_context().self_ref().unwrap(),
        ctx.base_context().fake_dragonos_sysroot(),
        *ctx.execute_context().action(),
        vec![],
    );

    assert!(scheduler.is_ok(), "Create scheduler error: {:?}", scheduler);

    let mut scheduler = scheduler.unwrap();

    let entity = scheduler.add_task(config_file, task);
    assert!(
        entity.is_err(),
        "Add task should return error: {:?}",
        entity
    );
}

/// 应在x86_64上运行包含x86_64的任务
#[test_context(DadkExecuteContextTestBuildX86_64V1)]
#[test]
fn should_run_task_include_x86_64_on_x86_64(ctx: &DadkExecuteContextTestBuildX86_64V1) {
    let config_file = ctx
        .base_context()
        .config_v2_dir()
        .join("app_all_target_arch_0_2_0.toml");
    let task =
        Parser::new(ctx.base_context().config_v2_dir(), None).parse_config_file(&config_file);
    assert!(task.is_ok(), "parse error: {:?}", task);
    let task = task.unwrap();

    assert!(
        task.target_arch.contains(&TargetArch::X86_64),
        "Cannot find target_arch x86_64: {:?}",
        task
    );

    let scheduler = Scheduler::new(
        ctx.execute_context().self_ref().unwrap(),
        ctx.base_context().fake_dragonos_sysroot(),
        *ctx.execute_context().action(),
        vec![],
    );

    assert!(scheduler.is_ok(), "Create scheduler error: {:?}", scheduler);

    let mut scheduler = scheduler.unwrap();

    let entity = scheduler.add_task(config_file, task);
    assert!(entity.is_ok(), "Add task should return ok: {:?}", entity);
}

/// 应在riscv64上运行包含riscv64的任务
#[test_context(DadkExecuteContextTestBuildRiscV64V1)]
#[test]
fn should_run_task_include_riscv64_on_riscv64(ctx: &DadkExecuteContextTestBuildRiscV64V1) {
    let config_file = ctx
        .base_context()
        .config_v2_dir()
        .join("app_all_target_arch_0_2_0.toml");
    let task =
        Parser::new(ctx.base_context().config_v2_dir(), None).parse_config_file(&config_file);
    assert!(task.is_ok(), "parse error: {:?}", task);
    let task = task.unwrap();

    assert!(
        task.target_arch.contains(&TargetArch::RiscV64),
        "Cannot find target_arch riscv64: {:?}",
        task
    );

    let scheduler = Scheduler::new(
        ctx.execute_context().self_ref().unwrap(),
        ctx.base_context().fake_dragonos_sysroot(),
        *ctx.execute_context().action(),
        vec![],
    );

    assert!(scheduler.is_ok(), "Create scheduler error: {:?}", scheduler);

    let mut scheduler = scheduler.unwrap();

    let entity = scheduler.add_task(config_file, task);
    assert!(entity.is_ok(), "Add task should return ok: {:?}", entity);
}

/// 确保文件 app_all_target_arch_0_2_0.toml 包含了所有的目标架构
#[test_context(BaseGlobalTestContext)]
#[test]
fn ensure_all_target_arch_testcase_v1(ctx: &BaseGlobalTestContext) {
    let config_file = ctx.config_v2_dir().join("app_all_target_arch_0_2_0.toml");
    let task = Parser::new(ctx.config_v2_dir(), None).parse_config_file(&config_file);
    assert!(task.is_ok(), "parse error: {:?}", task);
    let task = task.unwrap();

    for a in TargetArch::EXPECTED.iter() {
        let target_arch = TargetArch::try_from(*a).unwrap();
        assert!(
            task.target_arch.contains(&target_arch),
            "Cannot find target_arch '{:?}' in task: {:?}",
            a,
            task
        );
    }
}
