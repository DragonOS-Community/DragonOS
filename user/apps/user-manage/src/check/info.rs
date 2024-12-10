#[derive(Debug, Default, Clone)]
/// useradd的信息
pub struct UAddInfo {
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

impl From<UAddInfo> for String {
    fn from(info: UAddInfo) -> Self {
        format!(
            "{}::{}:{}:{}:{}:{}\n",
            info.username, info.uid, info.gid, info.comment, info.home_dir, info.shell
        )
    }
}

#[derive(Debug, Default, Clone)]
/// userdel的信息
pub struct UDelInfo {
    pub username: String,
    pub home: Option<String>,
}

#[derive(Debug, Default, Clone)]
/// usermod的信息
pub struct UModInfo {
    pub username: String,
    pub groups: Option<Vec<String>>,
    pub new_comment: Option<String>,
    pub new_home: Option<String>,
    pub new_gid: Option<String>,
    pub new_group: Option<String>,
    pub new_name: Option<String>,
    pub new_shell: Option<String>,
    pub new_uid: Option<String>,
}

#[derive(Debug, Default, Clone)]
/// passwd的信息
pub struct PasswdInfo {
    pub username: String,
    pub new_password: String,
}

#[derive(Debug, Default, Clone)]
/// groupadd的信息
pub struct GAddInfo {
    pub groupname: String,
    pub gid: String,
    pub passwd: Option<String>,
}

impl GAddInfo {
    pub fn to_string_group(&self) -> String {
        let mut passwd = String::from("");
        if self.passwd.is_some() {
            passwd = "x".to_string();
        }
        format!("{}:{}:{}:\n", self.groupname, passwd, self.gid)
    }

    pub fn to_string_gshadow(&self) -> String {
        let mut passwd = String::from("!");
        if let Some(gpasswd) = &self.passwd {
            passwd = gpasswd.clone();
        }

        format!("{}:{}::\n", self.groupname, passwd)
    }
}

#[derive(Debug, Default, Clone)]
/// groupdel的信息
pub struct GDelInfo {
    pub groupname: String,
}

#[derive(Debug, Default, Clone)]
/// groupmod的信息
pub struct GModInfo {
    pub groupname: String,
    pub gid: String,
    pub new_groupname: Option<String>,
    pub new_gid: Option<String>,
}
