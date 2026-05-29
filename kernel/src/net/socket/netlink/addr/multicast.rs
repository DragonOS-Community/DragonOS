#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GroupIdSet(u64);

impl GroupIdSet {
    pub const fn new_empty() -> Self {
        Self(0)
    }

    pub const fn new(groups: u32) -> Self {
        Self(groups as u64)
    }

    pub const fn new_u64(groups: u64) -> Self {
        Self(groups)
    }

    pub fn from_group_id(group_id: u32) -> Option<Self> {
        if group_id == 0 || group_id > 64 {
            return None;
        }

        Some(Self(1u64 << (group_id - 1)))
    }

    pub const fn ids_iter(&self) -> GroupIdIter {
        GroupIdIter::new(self)
    }

    pub fn add_groups(&mut self, groups: GroupIdSet) {
        self.0 |= groups.0;
    }

    pub fn drop_groups(&mut self, groups: GroupIdSet) {
        self.0 &= !groups.0;
    }

    pub fn set_groups(&mut self, new_groups: u32) {
        self.0 = new_groups as u64;
    }

    pub fn clear(&mut self) {
        self.0 = 0;
    }

    pub fn is_empty(&self) -> bool {
        self.0 == 0
    }

    pub fn as_u32(&self) -> u32 {
        self.0 as u32
    }

    pub fn as_u64(&self) -> u64 {
        self.0
    }
}

pub struct GroupIdIter {
    groups: u64,
}

impl GroupIdIter {
    const fn new(groups: &GroupIdSet) -> Self {
        Self { groups: groups.0 }
    }
}

impl Iterator for GroupIdIter {
    type Item = u32;

    fn next(&mut self) -> Option<Self::Item> {
        if self.groups > 0 {
            let group_id = self.groups.trailing_zeros();
            self.groups &= self.groups - 1;
            return Some(group_id);
        }

        None
    }
}
