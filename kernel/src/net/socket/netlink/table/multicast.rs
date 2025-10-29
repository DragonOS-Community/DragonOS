use alloc::collections::BTreeSet;

#[derive(Debug)]
pub struct MulticastGroup {
    // portnumber: u32,
    members: BTreeSet<u32>,
}

impl MulticastGroup {
    pub const fn new() -> Self {
        Self {
            members: BTreeSet::new(),
        }
    }

    pub fn add_member(&mut self, port_num: u32) {
        self.members.insert(port_num);
    }

    pub fn remove_member(&mut self, port_num: u32) {
        self.members.remove(&port_num);
    }

    pub fn members(&self) -> &BTreeSet<u32> {
        &self.members
    }
}

/// Uevent that can be sent to multicast groups.
pub trait MulticastMessage: Clone {}
