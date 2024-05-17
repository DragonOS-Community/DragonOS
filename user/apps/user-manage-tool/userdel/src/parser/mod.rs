use crate::error::ErrorHandler;
use std::collections::HashMap;

#[derive(Debug, PartialEq, Eq, Hash)]
/// 用户选项
pub enum CmdOption {
    /// 移除主目录
    Remove,
    /// 无效选项
    Invalid,
}

impl From<String> for CmdOption {
    fn from(value: String) -> Self {
        match value.as_str() {
            "-r" => Self::Remove,
            _ => Self::Invalid,
        }
    }
}

impl From<CmdOption> for &str {
    fn from(option: CmdOption) -> Self {
        match option {
            CmdOption::Remove => "-r",
            CmdOption::Invalid => "Invalid option",
        }
    }
}

#[derive(Debug)]
pub struct UDelCommand {
    /// 用户名
    pub username: String,
    /// 选项
    pub options: HashMap<CmdOption, String>,
}

#[derive(Debug)]
pub struct Parser;

impl Parser {
    /// **解析命令行参数**
    ///
    /// ## 参数
    /// - `args`: 命令行参数
    ///
    /// ## 返回
    /// - `UDelCommand`: 解析后的userdel命令
    pub fn parse(args: Vec<String>) -> UDelCommand {
        let username = args.last().unwrap().clone();
        let options = &args[1..args.len() - 1];
        let mut option = Vec::new();
        let mut option_arg = Vec::new();
        let mut is_option = true;
        for item in options {
            if is_option {
                option.push(item.clone());
            } else {
                option_arg.push(item.clone());
            }
            is_option = !is_option;
        }

        let mut options = HashMap::new();
        for (idx, op) in option.iter().enumerate() {
            let op: CmdOption = op.clone().into();
            if op == CmdOption::Invalid {
                ErrorHandler::error_handle(
                    "Invalid option".to_string(),
                    crate::error::ExitStatus::InvalidCmdSyntax,
                );
            } else if op == CmdOption::Remove {
                // -r 不需要参数
                continue;
            }
            if let Some(arg) = option_arg.get(idx) {
                options.insert(op, arg.clone());
            } else {
                let op: &str = op.into();
                ErrorHandler::error_handle(
                    format!("Invalid arg of option: [{}]", op),
                    crate::error::ExitStatus::InvalidArg,
                );
            }
        }

        UDelCommand { username, options }
    }
}
