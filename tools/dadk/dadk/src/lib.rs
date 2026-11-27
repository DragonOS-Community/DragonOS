use context::build_exec_context;

mod actions;
mod console;
mod context;
mod utils;

extern crate anyhow;

pub fn dadk_main() {
    // dadk_user_main();
    let exec_ctx = build_exec_context().expect("Failed to build execution context");
    log::debug!("Execution context: {:?}", exec_ctx);
    actions::run(exec_ctx);
}
