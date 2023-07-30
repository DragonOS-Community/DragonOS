use crate::syscall::SystemError;
use x86;
use crate::{kdebug};

/// Enable VMX operation.
pub fn vmxon(vmxon_pa: u64) -> Result<(), SystemError> {
    match unsafe { x86::bits64::vmx::vmxon(vmxon_pa) } {
        Ok(_) => Ok(()),
        Err(e) => {
            kdebug!("vmxon fail: {:?}", e);
            Err(SystemError::EVMXONFailed)
        },
    }
}

/// Disable VMX operation.
pub fn vmxoff() -> Result<(), SystemError> {
    match unsafe { x86::bits64::vmx::vmxoff() } {
        Ok(_) => Ok(()),
        Err(_) => Err(SystemError::EVMXOFFFailed),
    }
}

/// vmrite the current VMCS.
pub fn vmx_vmwrite(vmcs_field: u32, value: u64) -> Result<(), SystemError> {
    match unsafe { x86::bits64::vmx::vmwrite(vmcs_field, value) } {
        Ok(_) => Ok(()),
        Err(e) => {
            kdebug!("vmx_write fail: {:?}", e);
            Err(SystemError::EVMWRITEFailed)
        },
    }
}

/// vmread the current VMCS.
pub fn vmx_vmread(vmcs_field: u32) -> Result<u64, SystemError> {
    match unsafe { x86::bits64::vmx::vmread(vmcs_field) } {
        Ok(value) => Ok(value),
        Err(e) => {
            kdebug!("vmx_read fail: {:?}", e);
            Err(SystemError::EVMREADFailed)
        },
    }
}

pub fn vmx_vmptrld(vmcs_pa: u64)-> Result<(), SystemError> {
    match unsafe { x86::bits64::vmx::vmptrld(vmcs_pa) } {
        Ok(_) => Ok(()),
        Err(_) => Err(SystemError::EVMPRTLDFailed),
    }
}

pub fn vmx_vmlaunch()-> Result<(), SystemError> {
    match unsafe { x86::bits64::vmx::vmlaunch() } {
        Ok(_) => Ok(()),
        Err(e) => {
            kdebug!("vmx_launch fail: {:?}", e);
            Err(SystemError::EVMREADFailed)
        },
    }
}

pub fn vmx_vmclear(vmcs_pa: u64)-> Result<(), SystemError> {
    match unsafe { x86::bits64::vmx::vmclear(vmcs_pa) } {
        Ok(_) => Ok(()),
        Err(_) => Err(SystemError::EVMPRTLDFailed),
    }
}