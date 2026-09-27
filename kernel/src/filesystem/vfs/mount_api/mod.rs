//! File-descriptor-based mount API shared by the five creation and attach
//! syscalls. Topology changes remain owned by the mount and namespace layers.

pub mod context;
