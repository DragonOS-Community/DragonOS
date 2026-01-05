use crate::{
    driver::base::device::device_number::DeviceNumber,
    filesystem::{
        procfs::template::Common,
        vfs::{
            file::FileFlags, utils::DName, vcore::generate_inode_id, FilePrivateData, FileSystem,
            FileType, IndexNode, InodeFlags, InodeId, InodeMode, Metadata,
        },
    },
    libs::{rwlock::RwLock, spinlock::SpinLockGuard},
    time::PosixTimeSpec,
};
use alloc::collections::BTreeMap;
use alloc::fmt::Debug;
use alloc::string::String;
use alloc::string::ToString;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use inherit_methods_macro::inherit_methods;
use system_error::SystemError;

#[derive(Debug)]
pub struct ProcDir<Ops: DirOps> {
    inner: Ops,
    self_ref: Weak<ProcDir<Ops>>,
    parent: Option<Weak<dyn IndexNode>>,
    cached_children: RwLock<BTreeMap<String, Arc<dyn IndexNode>>>,
    common: Common,
    // 没用到？
    // fdata: InodeInfo,
}

impl<Ops: DirOps> ProcDir<Ops> {
    pub(super) fn new_with_data(
        dir: Ops,
        fs: Weak<dyn FileSystem>,
        parent: Option<Weak<dyn IndexNode>>,
        is_volatile: bool,
        mode: InodeMode,
        data: usize,
    ) -> Arc<Self> {
        let common = {
            // let ino = generate_inode_id();
            // let metadata = Metadata::new_dir(ino, mode, super::BLOCK_SIZE);
            let metadata = Metadata {
                dev_id: 0,
                inode_id: generate_inode_id(),
                size: 0,
                blk_size: 0,
                blocks: 0,
                atime: PosixTimeSpec::default(),
                mtime: PosixTimeSpec::default(),
                ctime: PosixTimeSpec::default(),
                btime: PosixTimeSpec::default(),
                file_type: FileType::Dir,
                mode,
                flags: InodeFlags::empty(),
                nlinks: 1,
                uid: 0,
                gid: 0,
                raw_dev: DeviceNumber::from(data as u32),
            };
            Common::new(metadata, fs, is_volatile)
        };

        Arc::new_cyclic(|weak_self| Self {
            inner: dir,
            self_ref: weak_self.clone(),
            parent,
            cached_children: RwLock::new(BTreeMap::new()),
            common,
        })
    }

    pub fn self_ref(&self) -> Option<Arc<ProcDir<Ops>>> {
        self.self_ref.upgrade()
    }

    pub fn self_ref_weak(&self) -> &Weak<ProcDir<Ops>> {
        &self.self_ref
    }

    pub fn parent(&self) -> Option<Arc<dyn IndexNode>> {
        self.parent.as_ref().and_then(|p| p.upgrade())
    }

    pub fn cached_children(&self) -> &RwLock<BTreeMap<String, Arc<dyn IndexNode>>> {
        &self.cached_children
    }
}

#[inherit_methods(from = "self.common")]
impl<Ops: DirOps + 'static> IndexNode for ProcDir<Ops> {
    fn fs(&self) -> Arc<dyn FileSystem>;
    fn as_any_ref(&self) -> &dyn core::any::Any;
    fn metadata(&self) -> Result<Metadata, SystemError>;
    fn set_metadata(&self, metadata: &Metadata) -> Result<(), SystemError>;

    fn read_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &mut [u8],
        _data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        Err(SystemError::EISDIR)
    }

    fn write_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &[u8],
        _data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        Err(SystemError::EISDIR)
    }

    fn list(&self) -> Result<Vec<String>, SystemError> {
        let mut keys = Vec::new();
        keys.push(String::from("."));
        keys.push(String::from(".."));

        // 先填充子节点，然后获取读锁读取keys
        self.inner.populate_children(self);
        {
            let cached_children = self.cached_children.read();
            keys.extend(cached_children.keys().cloned());
        }

        return Ok(keys);
    }

    fn find(&self, name: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
        match name {
            "" | "." => {
                return Ok(self.self_ref().ok_or(SystemError::ENOENT)?);
            }

            ".." => {
                // 如果有父节点，返回父节点；否则返回自身（根目录的 .. 指向自己）
                return Ok(self
                    .parent()
                    .unwrap_or_else(|| self.self_ref().expect("self_ref should be valid")));
            }

            name => {
                //todo 先忽略fd目录的处理
                // 先查缓存（使用作用域来确保锁及时释放）
                {
                    let cached_children = self.cached_children.read();
                    if let Some(inode) = cached_children.get(name) {
                        if self.inner.validate_child(inode.as_ref()) {
                            return Ok(inode.clone());
                        }
                    }
                } // 读锁在这里释放

                // 缓存未命中，调用 DirOps::lookup_child 创建
                self.inner.lookup_child(self, name)
            }
        }
    }

    fn get_entry_name(&self, ino: InodeId) -> Result<String, SystemError> {
        match ino.into() {
            0 => {
                return Ok(String::from("."));
            }
            1 => {
                return Ok(String::from(".."));
            }
            ino => {
                // 暴力遍历所有的children，判断inode id是否相同
                // TODO: 优化这里，这个地方性能很差！
                let mut key: Vec<String> = self
                    .cached_children
                    .read()
                    .iter()
                    .filter_map(|(k, v)| {
                        if v.metadata().unwrap().inode_id.into() == ino {
                            Some(k.to_string())
                        } else {
                            None
                        }
                    })
                    .collect();

                match key.len() {
                    0 => {
                        return Err(SystemError::ENOENT);
                    }
                    1 => {
                        return Ok(key.remove(0));
                    }
                    _ => panic!(
                        "Procfs get_entry_name: key.len()={key_len}>1, current inode_id={inode_id:?}, to find={to_find:?}",
                        key_len = key.len(),
                        inode_id = self.metadata().unwrap().inode_id,
                        to_find = ino
                    ),
                }
            }
        }
    }

    fn dname(&self) -> Result<DName, SystemError> {
        Ok(self.common.dname.clone())
    }

    fn open(
        &self,
        _data: SpinLockGuard<FilePrivateData>,
        _flags: &FileFlags,
    ) -> Result<(), SystemError> {
        return Ok(());
    }

    fn close(&self, _data: SpinLockGuard<FilePrivateData>) -> Result<(), SystemError> {
        return Ok(());
    }

    fn link(&self, _name: &str, _other: &Arc<dyn IndexNode>) -> Result<(), SystemError> {
        todo!("procfs dir link not implemented");
    }
}

pub trait DirOps: Sync + Send + Sized + Debug {
    fn lookup_child(
        &self,
        dir: &ProcDir<Self>,
        name: &str,
    ) -> Result<Arc<dyn IndexNode>, SystemError>;

    /// 填充子节点到缓存中
    /// 该方法内部获取写锁进行填充，完成后释放写锁
    fn populate_children(&self, dir: &ProcDir<Self>);

    #[must_use]
    fn validate_child(&self, _child: &dyn IndexNode) -> bool {
        true
    }
}

pub fn lookup_child_from_table<Fp, F>(
    name: &str,
    cached_children: &mut BTreeMap<String, Arc<dyn IndexNode>>,
    table: &[(&str, Fp)],
    constructor_adaptor: F,
) -> Option<Arc<dyn IndexNode>>
where
    Fp: Copy,
    F: FnOnce(Fp) -> Arc<dyn IndexNode>,
{
    for (child_name, child_constructor) in table.iter() {
        if *child_name == name {
            return Some(
                cached_children
                    .entry(String::from(name))
                    .or_insert_with(|| (constructor_adaptor)(*child_constructor))
                    .clone(),
            );
        }
    }

    None
}

pub fn populate_children_from_table<Fp, F>(
    cached_children: &mut BTreeMap<String, Arc<dyn IndexNode>>,
    table: &[(&str, Fp)],
    constructor_adaptor: F,
) where
    Fp: Copy,
    F: Fn(Fp) -> Arc<dyn IndexNode>,
{
    for (child_name, child_constructor) in table.iter() {
        cached_children
            .entry(String::from(*child_name))
            .or_insert_with(|| (constructor_adaptor)(*child_constructor));
    }
}
