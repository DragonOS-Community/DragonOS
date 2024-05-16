use std::process::exit;

#[derive(Debug)]
pub enum ExitStatus {
    Success = 0,
    PasswdFile = 1,
    InvalidCmdSyntax = 2,
    InvalidArg = 3,
    _UidInUse = 4,
    _GroupNotExist = 6,
    _UsernameInUse = 9,
    GroupFile = 10,
    _CreateHomeFail = 12,
    _UpdateSELInuxMapFail = 14,
    PermissionDenied = -1,
    ShadowFile = -2,
    GshadowFile = -3,
}

pub struct ErrorHandler;

impl ErrorHandler {
    /// **错误处理函数**
    ///
    /// ## 参数
    ///
    /// - `error`错误信息
    /// - `exit_status` - 退出状态码
    pub fn error_handle(s: String, exit_status: ExitStatus) {
        eprintln!("{s}");
        exit(exit_status as i32);
    }
}
