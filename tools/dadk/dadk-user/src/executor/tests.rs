use std::path::PathBuf;
use test_base::test_context::{self as test_context, test_context};

use crate::{
    context::{
        DadkExecuteContextTestBuildRiscV64V1, DadkExecuteContextTestBuildX86_64V1, TestContextExt,
    },
    executor::Executor,
    parser::Parser,
    scheduler::{SchedEntities, Scheduler},
};

use super::create_global_env_list;

fn setup_executor<T: TestContextExt>(config_file: PathBuf, ctx: &T) -> Executor {
    let task =
        Parser::new(ctx.base_context().config_v2_dir(), None).parse_config_file(&config_file);
    assert!(task.is_ok(), "parse error: {:?}", task);
    let scheduler = Scheduler::new(
        ctx.execute_context().self_ref().unwrap(),
        ctx.base_context().fake_dragonos_sysroot(),
        *ctx.execute_context().action(),
        vec![],
    );

    assert!(scheduler.is_ok(), "Create scheduler error: {:?}", scheduler);

    let mut scheduler = scheduler.unwrap();

    let entity = scheduler.add_task(config_file, task.unwrap());

    assert!(entity.is_ok(), "Add task error: {:?}", entity);
    let entity = entity.unwrap();
    let executor = Executor::new(
        entity.clone(),
        *ctx.execute_context().action(),
        ctx.base_context().fake_dragonos_sysroot(),
    );

    assert!(executor.is_ok(), "Create executor error: {:?}", executor);

    let executor = executor.unwrap();
    return executor;
}

/// 测试能否正确设置本地环境变量
#[test_context(DadkExecuteContextTestBuildX86_64V1)]
#[test]
fn set_local_env(ctx: &DadkExecuteContextTestBuildX86_64V1) {
    let config_file_path = ctx
        .base_context()
        .config_v2_dir()
        .join("app_normal_with_env_0_2_0.toml");
    let mut executor = setup_executor(config_file_path, ctx);

    let r = executor.prepare_local_env();
    assert!(r.is_ok(), "Prepare local env error: {:?}", r);
    assert_ne!(executor.local_envs.envs.len(), 0);

    assert!(executor.local_envs.get("DADK_CURRENT_BUILD_DIR").is_some());
    assert!(executor.local_envs.get("CC").is_some());
    assert_eq!(executor.local_envs.get("CC").unwrap().value, "abc-gcc");

    let x = executor.execute();
    assert!(x.is_ok(), "Execute error: {:?}", x);
}

/// 测试执行错误时，能否感知到错误
#[test_context(DadkExecuteContextTestBuildX86_64V1)]
#[test]
fn execute_should_capture_error(ctx: &DadkExecuteContextTestBuildX86_64V1) {
    let config_file_path = ctx
        .base_context()
        .config_v2_dir()
        .join("app_normal_with_env_fail_0_2_0.toml");
    let mut executor = setup_executor(config_file_path, ctx);

    let r = executor.prepare_local_env();
    assert!(r.is_ok(), "Prepare local env error: {:?}", r);
    assert_ne!(executor.local_envs.envs.len(), 0);

    assert!(executor.local_envs.get("DADK_CURRENT_BUILD_DIR").is_some());
    assert!(executor.local_envs.get("CC").is_some());
    assert_eq!(executor.local_envs.get("CC").unwrap().value, "abc-gcc1");

    let x = executor.execute();
    assert!(x.is_err(), "Executor cannot catch error when build error");
}

/// 测试能否正确设置ARCH全局环境变量为x86_64
#[test_context(DadkExecuteContextTestBuildX86_64V1)]
#[test]
fn check_arch_env_x86_64(ctx: &DadkExecuteContextTestBuildX86_64V1) {
    let entities = SchedEntities::new();
    let env_list = create_global_env_list(&entities, &ctx.execute_context().self_ref().unwrap());
    assert!(
        env_list.is_ok(),
        "Create global env list error: {:?}",
        env_list
    );
    let env_list = env_list.unwrap();
    assert!(env_list.get("ARCH").is_some());
    assert_eq!(env_list.get("ARCH").unwrap().value, "x86_64");
}

/// 测试能否正确设置ARCH全局环境变量为riscv64
#[test_context(DadkExecuteContextTestBuildRiscV64V1)]
#[test]
fn check_arch_env_riscv64(ctx: &DadkExecuteContextTestBuildRiscV64V1) {
    let entities = SchedEntities::new();
    let env_list = create_global_env_list(&entities, &ctx.execute_context().self_ref().unwrap());
    assert!(
        env_list.is_ok(),
        "Create global env list error: {:?}",
        env_list
    );
    let env_list = env_list.unwrap();
    assert!(env_list.get("ARCH").is_some());
    assert_eq!(env_list.get("ARCH").unwrap().value, "riscv64");
}
