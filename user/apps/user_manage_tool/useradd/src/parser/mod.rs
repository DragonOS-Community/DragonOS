use crate::error::ErrorHandler;
use std::collections::HashMap;

#[derive(Debug, PartialEq, Eq, Hash)]
pub enum CmdOption {
    /// 用户额外信息
    Comment,
    /// 用户home目录
    Home,
    /// 用户所在组的组名
    Group,
    /// 终端程序
    Shell,
    /// 用户id
    Uid,
    /// 无效选项
    Invalid,
}

impl From<String> for CmdOption {
    fn from(value: String) -> Self {
        match value.as_str() {
            "-c" => Self::Comment,
            "-d" => Self::Home,
            "-g" => Self::Group,
            "-s" => Self::Shell,
            "-u" => Self::Uid,
            _ => Self::Invalid,
        }
    }
}

impl From<CmdOption> for &str {
    fn from(option: CmdOption) -> Self {
        match option {
            CmdOption::Comment => "-c",
            CmdOption::Home => "-d",
            CmdOption::Group => "-g",
            CmdOption::Shell => "-s",
            CmdOption::Uid => "-u",
            CmdOption::Invalid => "Invalid option",
        }
    }
}

/// useradd命令
#[derive(Debug)]
pub struct UAddCommand {
    /// 用户名
    pub username: String,
    /// 选项
    pub options: HashMap<CmdOption, String>,
}

/// 解析器
#[derive(Debug)]
pub struct Parser;

impl Parser {
    /// **解析命令行参数**
    ///
    /// 参数
    /// `args`：命令行参数列表
    ///
    /// 返回值
    /// `UAddCommand`: 解析后的useradd命令
    pub fn parse(args: Vec<String>) -> UAddCommand {
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

        // 一个选项对应一个参数
        let mut options = HashMap::new();
        for (idx, op) in option.iter().enumerate() {
            let op: CmdOption = op.clone().into();
            if op == CmdOption::Invalid {
                ErrorHandler::error_handle(
                    "Invalid option".to_string(),
                    crate::error::ExitStatus::InvalidCmdSyntax,
                );
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

        UAddCommand { username, options }
    }
}
