use std::{collections::BTreeMap, fmt::Debug, fs::File, io::Write, path::PathBuf};

use log::error;

use crate::constant::CMD_ARGS;

/// 日志集合
///
/// 所有的日志都会被存到这个集合中, 以便于进行各种操作
///
/// 日志集合的后端可以在日志插入前后做一些操作（需要实现[`LogSetBackend`]）
#[derive(Debug)]
#[allow(dead_code)]
pub struct LogSet<K, V> {
    inner: BTreeMap<K, V>,
    backend: Box<dyn LogSetBackend<K, V>>,
    name: String,
    file_path: PathBuf,
    log_file: Option<File>,
}

#[allow(dead_code)]
impl<K: Ord, V: Clone + PartialEq + Debug> LogSet<K, V> {
    pub fn new(name: String, backend: Option<Box<dyn LogSetBackend<K, V>>>) -> Self {
        let mut file_path = CMD_ARGS.read().unwrap().as_ref().unwrap().log_dir.clone();
        file_path.push(format!("{}-{}.log", name, std::process::id()));

        let log_file = File::create(&file_path).expect("Failed to create log file.");

        Self {
            inner: BTreeMap::new(),
            backend: backend.unwrap_or_else(|| Box::new(DefaultBackend::new())),
            name,
            file_path,
            log_file: Some(log_file),
        }
    }

    pub fn insert(&mut self, key: K, value: V) {
        let cloned_value = value.clone();
        self.backend.before_insert(&self.name, &value);

        let prev = self.inner.insert(key, value);
        if let Some(prev) = prev {
            if prev.ne(&cloned_value) {
                error!(
                    "LogSet::insert(): prev != cloned_value: prev: {:?}, cloned_value: {:?}",
                    prev, cloned_value
                );
            }
        } else {
            self.log_file
                .as_mut()
                .map(|f| writeln!(f, "{:?}", cloned_value).ok());
        }

        self.backend.after_insert(&self.name, &cloned_value);
    }

    pub fn file_path(&self) -> &PathBuf {
        &self.file_path
    }

    pub fn len(&self) -> usize {
        self.inner.len()
    }

    pub fn get(&self, key: &K) -> Option<&V> {
        self.inner.get(key)
    }

    pub fn get_mut(&mut self, key: &K) -> Option<&mut V> {
        self.inner.get_mut(key)
    }

    pub fn remove(&mut self, key: &K) -> Option<V> {
        self.inner.remove(key)
    }

    pub fn clear(&mut self) {
        self.inner.clear();
    }

    pub fn iter(&self) -> impl Iterator<Item = (&K, &V)> {
        self.inner.iter()
    }

    pub fn contains_key(&self, key: &K) -> bool {
        self.inner.contains_key(key)
    }
}

/// 日志集合的后端, 用于在日志插入前后做一些操作
pub trait LogSetBackend<K, V>: Debug {
    fn before_insert(&mut self, _log_set_name: &str, _log: &V) {}

    fn after_insert(&mut self, _log_set_name: &str, _log: &V) {}
}

#[derive(Debug)]
struct DefaultBackend(());

impl DefaultBackend {
    pub const fn new() -> Self {
        Self(())
    }
}

impl<K, V> LogSetBackend<K, V> for DefaultBackend {
    fn before_insert(&mut self, _log_set_name: &str, _log: &V) {}

    fn after_insert(&mut self, _log_set_name: &str, _log: &V) {}
}
