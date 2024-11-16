use std::io::Error;

use log::debug;
use nix::{
    sched::{sched_getaffinity, sched_setaffinity, CpuSet},
    unistd::getpid,
};

use crate::Test;

fn test_cpumask() -> std::io::Result<()> {
    let origin_mask = sched_getaffinity(getpid())?;
    debug!("origin_mask: {:?}", origin_mask);

    let mut new_mask = CpuSet::new();
    new_mask.set(0)?;
    sched_setaffinity(getpid(), &new_mask)?;
    if sched_getaffinity(getpid())? != new_mask {
        return Err(Error::other("sched_setaffinity failed"));
    }

    debug!("new_mask: {:?}", new_mask);

    Ok(())
}

impl Test {
    pub fn test_cpumask() -> std::io::Result<()> {
        test_cpumask()
    }
}
