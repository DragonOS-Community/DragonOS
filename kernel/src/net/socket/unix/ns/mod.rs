use alloc::collections::BTreeMap;

use hashbrown::HashMap;
use system_error::SystemError;

use crate::net::socket::unix::UnixEndpoint;
use crate::net::socket::Socket;
use crate::{filesystem::vfs::InodeId, libs::rwlock::RwLock};

use alloc::string::String;
use alloc::sync::Arc;

lazy_static! {
    // and unnamed unix socket, they don't have a path, so we don't store them in the map.
    // they will be removed when the socket is closed.
    pub static ref ABS_UNIX_MAP: RwLock<HashMap<AbstractUnixPath, Arc<dyn Socket>>> = RwLock::new(HashMap::new());
    pub static ref INO_UNIX_MAP: RwLock<BTreeMap<InodeId, Arc<dyn Socket>>> = RwLock::new(BTreeMap::new());
}

#[derive(Debug, Clone, Eq, Hash, PartialEq)]
pub struct AbstractUnixPath(String);

impl AbstractUnixPath {
    pub fn new(path: String) -> Self {
        Self(path)
    }

    pub fn sun_path(&self, path: &mut [u8]) {
        let mut path_bytes = self.0.as_bytes().to_vec();
        if path_bytes.len() > 108 {
            path_bytes.truncate(108);
        }
        path_bytes.push(0); // null-terminate
        path[0] = 0; // abstract socket starts with a null byte
        path[1..path_bytes.len()].copy_from_slice(&path_bytes);
    }
}

pub(super) struct UnixSockMap<S: Socket> {
    /// A map of abstract unix sockets, they don't have a path, so we don't store them in the map.
    /// They will be removed when the socket is closed.
    pub abs_unix_map: RwLock<HashMap<AbstractUnixPath, Arc<S>>>,
    /// A map of inode unix sockets, they have a path, so we store them in the map.
    pub ino_unix_map: RwLock<BTreeMap<String, Arc<S>>>,
}

impl<S: Socket> UnixSockMap<S> {
    pub fn new() -> Self {
        Self {
            abs_unix_map: RwLock::new(HashMap::new()),
            ino_unix_map: RwLock::new(BTreeMap::new()),
        }
    }

    /// Try to insert a socket into the map. If the socket already exists, return EEXIST.
    pub fn try_insert(&self, endpoint: UnixEndpoint, socket: Arc<S>) -> Result<(), SystemError> {
        use UnixEndpoint::*;
        match endpoint {
            File(path) => {
                use alloc::collections::btree_map::Entry::*;
                match self.ino_unix_map.write().entry(path) {
                    Vacant(vacant_entry) => {
                        vacant_entry.insert(socket);
                        return Ok(());
                    }
                    Occupied(_) => {
                        return Err(SystemError::EEXIST);
                    }
                }
            }
            Abstract(path) => {
                use hashbrown::hash_map::Entry::*;
                match self.abs_unix_map.write().entry(path) {
                    Vacant(vacant_entry) => {
                        vacant_entry.insert(socket);
                        return Ok(());
                    }
                    Occupied(_) => {
                        return Err(SystemError::EEXIST);
                    }
                }
            }
        }
    }

    pub fn get(&self, endpoint: &UnixEndpoint) -> Option<Arc<S>> {
        use UnixEndpoint::*;
        match endpoint {
            File(ino) => self.ino_unix_map.read().get(ino).cloned(),
            Abstract(path) => self.abs_unix_map.read().get(path).cloned(),
        }
    }
}
