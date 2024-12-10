use std::process::exit;

#[derive(Debug)]
pub enum ExitStatus {
    Success = 0,
    PasswdFile = 1,
    InvalidCmdSyntax = 2,
    InvalidArg = 3,
    UidInUse = 4,
    GroupNotExist = 6,
    UsernameInUse = 9,
    GroupFile = 10,
    CreateHomeFail = 12,
    PermissionDenied = -1,
    ShadowFile = -2,
    GshadowFile = -3,
    GroupaddFail = -4,
}

pub struct ErrorHandler;

impl ErrorHandler {
    /// **错误处理函数**
    ///
    /// ## 参数
    ///
    /// - `error`错误信息
    /// - `exit_status` - 退出状态码
    pub fn error_handle(error: String, exit_status: ExitStatus) {
        eprintln!("{error}");
        exit(exit_status as i32);
    }
}
