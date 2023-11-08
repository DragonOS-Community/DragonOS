use std::sync::RwLock;

use crate::command::CommandLineArgs;

/// 启动时的命令行参数
pub static CMD_ARGS: RwLock<Option<CommandLineArgs>> = RwLock::new(None);
