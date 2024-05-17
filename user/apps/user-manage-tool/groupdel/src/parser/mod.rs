pub struct GDelCommand {
    pub groupname: String,
}

pub struct Parser;

impl Parser {
    /// **解析命令行参数**
    ///
    /// ## 参数
    /// - `args`: 命令行参数
    ///
    /// ## 返回
    /// - `GDelCommand`: 解析后的groupdel命令
    pub fn parse(args: Vec<String>) -> GDelCommand {
        let groupname = args.last().unwrap().clone();
        GDelCommand { groupname }
    }
}
