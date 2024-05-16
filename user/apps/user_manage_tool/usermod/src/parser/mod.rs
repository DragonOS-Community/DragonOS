use crate::error::ErrorHandler;
use std::collections::HashMap;

#[derive(Debug, PartialEq, Eq, Hash)]
pub enum CmdOption {
    /// 添加到其它组
    Append,
    /// 修改用户额外信息
    Comment,
    /// 修改用户home目录
    Home,
    /// 修改用户组
    Group,
    /// 修改用户名称
    Name,
    /// 修改终端程序
    Shell,
    /// 修改用户id
    Uid,
    /// 无效选项
    Invalid,
}

impl From<String> for CmdOption {
    fn from(value: String) -> Self {
        match value.as_str() {
            "-a" => Self::Append,
            "-c" => Self::Comment,
            "-d" => Self::Home,
            "-g" => Self::Group,
            "-s" => Self::Shell,
            "-u" => Self::Uid,
            "-l" => Self::Name,
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
            CmdOption::Name => "-l",
            CmdOption::Append => "-a",
            CmdOption::Invalid => "Invalid option",
        }
    }
}

/// 解析器
#[derive(Debug)]
pub struct Parser;

impl Parser {
    /// **解析命令行参数**
    ///
    /// ## 参数
    /// - `args`: 命令行参数
    ///
    /// ## 返回
    /// - `UModCommand`: 解析后的usermod命令
    pub fn parse(args: Vec<String>) -> UModCommand {
        let username = args.last().unwrap().clone();
        let options = &args[1..args.len() - 1];
        let mut option = Vec::new();
        let mut option_arg = Vec::new();
        let mut is_option = true;

        let mut idx = 0;
        loop {
            if idx >= options.len() {
                break;
            }

            let item = &options[idx];
            if is_option {
                if item == "-a" {
                    idx += 1;
                    if idx >= options.len() || options[idx] != "-G" {
                        ErrorHandler::error_handle(
                            "Invalid option: -a -G <group1,group2,...>".to_string(),
                            crate::error::ExitStatus::InvalidCmdSyntax,
                        );
                    }
                }
                option.push(item.clone())
            } else {
                option_arg.push(item.clone());
            }

            idx += 1;
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

        UModCommand { username, options }
    }
}

/// useradd命令
#[derive(Debug)]
pub struct UModCommand {
    /// 用户名
    pub username: String,
    /// 选项
    pub options: HashMap<CmdOption, String>,
}
