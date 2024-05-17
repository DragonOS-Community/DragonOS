/// 用户信息
#[derive(Debug, Default, Clone)]
pub struct UserInfo {
    /// 用户名
    pub username: String,
    pub uid: String,
    pub gid: String,
    /// 所在组的组名
    pub group: String,
    /// 用户描述信息
    pub comment: String,
    /// 主目录
    pub home_dir: String,
    /// 终端程序名
    pub shell: String,
}

impl From<UserInfo> for String {
    fn from(userinfo: UserInfo) -> Self {
        format!(
            "{}::{}:{}:{}:{}:{}\n",
            userinfo.username,
            userinfo.uid,
            userinfo.gid,
            userinfo.comment,
            userinfo.home_dir,
            userinfo.shell
        )
    }
}
