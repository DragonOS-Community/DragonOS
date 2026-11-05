use clap::{Parser, Subcommand, ValueEnum};

#[derive(Debug, Subcommand, Clone, PartialEq, Eq)]
pub enum UserCommand {
    Build,
    Clean(UserCleanCommand),
    Install,
}

#[derive(Debug, Parser, Clone, PartialEq, Eq)]
pub struct UserCleanCommand {
    /// 清理级别
    #[clap(long, default_value = "all")]
    pub level: UserCleanLevel,
    /// 要清理的task
    #[clap(long)]
    pub task: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum UserCleanLevel {
    /// 清理所有用户程序构建缓存
    All,
    /// 只在用户程序源码目录下清理
    InSrc,
    /// 只清理用户程序输出目录
    Output,
}

impl Into<dadk_config::user::UserCleanLevel> for UserCleanLevel {
    fn into(self) -> dadk_config::user::UserCleanLevel {
        match self {
            UserCleanLevel::All => dadk_config::user::UserCleanLevel::All,
            UserCleanLevel::InSrc => dadk_config::user::UserCleanLevel::InSrc,
            UserCleanLevel::Output => dadk_config::user::UserCleanLevel::Output,
        }
    }
}

impl Into<dadk_user::context::Action> for UserCommand {
    fn into(self) -> dadk_user::context::Action {
        match self {
            UserCommand::Build => dadk_user::context::Action::Build,
            UserCommand::Install => dadk_user::context::Action::Install,
            UserCommand::Clean(args) => dadk_user::context::Action::Clean(args.level.into()),
        }
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn test_user_clean_level_from_str() {
        // Test valid cases
        assert_eq!(
            UserCleanLevel::from_str("all", true).unwrap(),
            UserCleanLevel::All
        );
        assert_eq!(
            UserCleanLevel::from_str("in-src", true).unwrap(),
            UserCleanLevel::InSrc
        );
        assert_eq!(
            UserCleanLevel::from_str("output", true).unwrap(),
            UserCleanLevel::Output
        );

        // Test invalid case
        assert!(UserCleanLevel::from_str("invalid", true).is_err());
    }
}
