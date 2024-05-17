use std::collections::HashMap;

use crate::error::ErrorHandler;

#[derive(Debug, PartialEq, Eq, Hash, Clone)]
pub enum CmdOption {
    Gid,
    Group,
    Invalid,
}

impl From<&String> for CmdOption {
    fn from(s: &String) -> Self {
        match s.as_str() {
            "-g" => CmdOption::Gid,
            "-n" => CmdOption::Group,
            _ => CmdOption::Invalid,
        }
    }
}

impl From<CmdOption> for &str {
    fn from(s: CmdOption) -> Self {
        match s {
            CmdOption::Gid => "-g",
            CmdOption::Group => "-n",
            CmdOption::Invalid => "",
        }
    }
}

#[derive(Debug)]
pub struct GModCommand {
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
    /// - `GModCommand`: 解析后的groupmod命令
    pub fn parse(args: Vec<String>) -> GModCommand {
        let groupname = args.last().unwrap().clone();
        let args = &args[1..args.len() - 1];
        let mut options = HashMap::new();
        let mut option_vec: Vec<CmdOption> = Vec::new();
        let mut arg_vec: Vec<String> = Vec::new();
        let mut is_option = true;
        for arg in args {
            if is_option {
                option_vec.push(arg.into());
            } else {
                arg_vec.push(arg.clone());
            }
            is_option = !is_option;
        }

        for i in 0..option_vec.len() {
            let op = option_vec[i].clone();
            if op == CmdOption::Invalid {
                ErrorHandler::error_handle(
                    "Invalid option".to_string(),
                    crate::error::ExitStatus::InvalidCmdSyntax,
                );
            }
            if let Some(arg) = arg_vec.get(i) {
                options.insert(op, arg.clone());
            } else {
                let op: &str = op.into();
                ErrorHandler::error_handle(
                    format!("Invalid arg of option: [{}]", op),
                    crate::error::ExitStatus::InvalidArg,
                );
            }
        }

        GModCommand { groupname, options }
    }
}
