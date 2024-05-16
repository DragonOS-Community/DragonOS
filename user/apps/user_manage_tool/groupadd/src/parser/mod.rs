use crate::error::ErrorHandler;
use std::collections::HashMap;

#[derive(Debug, PartialEq, Eq, Hash)]
pub enum CmdOption {
    Gid,
    Passwd,
    Invalid,
}

impl From<String> for CmdOption {
    fn from(s: String) -> Self {
        match s.as_str() {
            "-g" => CmdOption::Gid,
            "-p" => CmdOption::Passwd,
            _ => CmdOption::Invalid,
        }
    }
}

impl From<CmdOption> for &str {
    fn from(s: CmdOption) -> Self {
        match s {
            CmdOption::Gid => "-g",
            CmdOption::Passwd => "-p",
            CmdOption::Invalid => "Invalid option",
        }
    }
}

pub struct GAddCommand {
    pub groupname: String,
    pub options: HashMap<CmdOption, String>,
}

pub struct Parser;

impl Parser {
    /// **解析命令行参数**
    ///
    /// ## 参数
    /// - `args`: 命令行参数
    ///
    /// ## 返回
    /// - `GAddCommand`: 解析后的groupadd命令
    pub fn parse(args: Vec<String>) -> GAddCommand {
        let groupname = args.last().unwrap().clone();
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

        GAddCommand { groupname, options }
    }
}
