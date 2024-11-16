use super::CgroupSubsysState;

struct MemCgroup {
    css: CgroupSubsysState,
    id: u32,
}
