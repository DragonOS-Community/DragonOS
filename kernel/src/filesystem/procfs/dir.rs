use alloc::string::String;
use system_error::SystemError;

pub use super::process_info::ProcessId;


#[derive(Debug, Clone, Copy)]
pub enum ProcDirType {
    Root,
    ProcessDir(ProcessId),
    SysDir,              
    SysKernelDir,        
    SysVmDir,           
    SysFsDir,           
    SysNetDir,           
    SysNetCoreDir,       
    SysNetIpv4Dir,       
    ProcessNsDir(ProcessId),  
}

pub fn handle_dir_operation(_dir_type: ProcDirType) -> Result<String, SystemError> {
    Err(SystemError::EISDIR)
}
