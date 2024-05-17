/// passwd命令
#[derive(Debug, Default)]
pub struct PwdCommand {
    pub username: Option<String>,
}

pub struct Parser;

impl Parser {
    /// **解析命令行参数**
    ///
    /// ## 参数
    /// - `args`: 命令行参数
    ///
    /// ## 返回
    /// - `PwdCommand`: 解析后的passwd命令
    pub fn parse(args: Vec<String>) -> PwdCommand {
        let mut cmd = PwdCommand::default();
        if args.len() > 1 {
            cmd.username = Some(args[1].clone());
        }

        cmd
    }
}
