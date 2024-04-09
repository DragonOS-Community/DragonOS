use super::{MountFS, MountFSInode};
use crate::{filesystem::vfs::utils::Keyable, libs::rwlock::RwLock};
use alloc::{
    collections::BTreeMap, string::String, sync::{Arc, Weak}
};
use path_base::{clean_path::Clean, Path, PathBuf};

#[derive(PartialEq, Eq, Debug)]
pub struct MountPath(PathBuf);

impl From<&str> for MountPath {
    fn from(value: &str) -> Self {
        Self(PathBuf::from(value).clean())
    }
}

impl From<&Path> for MountPath {
    fn from(value: &Path) -> Self {
        Self(value.clean())
    }
}

impl From<PathBuf> for MountPath {
    fn from(value: PathBuf) -> Self {
        Self(value.clean())
    }
}

impl AsRef<Path> for MountPath {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl PartialOrd for MountPath {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for MountPath {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        let self_dep = self.0.components().count();
        let othe_dep = other.0.components().count();
        if self_dep == othe_dep {
            self.0.cmp(&other.0)
        } else {
            othe_dep.cmp(&self_dep)
        }
    }
}

// 维护一个挂载点的记录，以支持特定于文件系统的索引
type MountListType = Option<Arc<RwLock<BTreeMap<MountPath, Weak<MountFS>>>>>;
pub struct MountList(MountListType);
static mut __MOUNTS_LIST: MountList = MountList(None);

impl MountList {
    /// 初始化挂载点
    pub fn init() {
        unsafe {
            __MOUNTS_LIST = MountList(Some(Arc::new(RwLock::new(BTreeMap::new()))));
        }
    }

    fn instance() -> &'static Arc<RwLock<BTreeMap<MountPath, Weak<MountFS>>>> {
        unsafe {
            if __MOUNTS_LIST.0.is_none() {
                MountList::init();
            }
            return __MOUNTS_LIST.0.as_ref().unwrap();
        }
    }

    /// 在 **路径`path`** 下挂载 **文件系统`fs`**
    pub fn insert<T: AsRef<Path>>(path: T, fs: &Arc<MountFS>) {
        MountList::instance()
            .write()
            .insert(MountPath::from(path.as_ref()), Arc::downgrade(fs));
    }

    /// 获取挂载点信息，返回
    ///
    /// - `最近的挂载点`
    /// - `挂载点下的路径`
    /// - `文件系统`
    /// # None
    /// 未找到挂载点
    pub fn get<T: AsRef<Path>>(path: T) -> Option<(PathBuf, PathBuf, Arc<MountFS>)> {
        MountList::instance()
            .upgradeable_read()
            .iter()
            .filter_map(|(key, value)| {
                let strkey = key.as_ref();
                if let Some(fs) = value.upgrade() {
                    if let Ok(rest) = path.as_ref().strip_prefix(strkey) {
                        return Some((strkey.to_path_buf(), rest.to_path_buf(), fs.clone()));
                    }
                }
                None
            })
            .next()
    }
}

#[derive(Debug)]
pub(super) struct MountNameCmp (pub Weak<MountFSInode>);

impl Keyable for MountNameCmp {
    fn key(&self) -> Arc<String> {
        if let Some(src) = self.0.upgrade() {
            return src.name.clone();
        }
        return Arc::new(String::new());
    }
}
