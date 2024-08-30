use super::cgroup::CgroupSubsysState;

struct MemCgroup {
    css: CgroupSubsysState,
    id: u32,
}
