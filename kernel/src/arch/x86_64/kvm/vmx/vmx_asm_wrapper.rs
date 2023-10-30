use super::vmcs::VmcsFields;
use crate::kdebug;
use crate::syscall::SystemError;
use core::arch::asm;
use x86;
/// Enable VMX operation.
pub fn vmxon(vmxon_pa: u64) -> Result<(), SystemError> {
    match unsafe { x86::bits64::vmx::vmxon(vmxon_pa) } {
        Ok(_) => Ok(()),
        Err(e) => {
            kdebug!("vmxon fail: {:?}", e);
            Err(SystemError::EVMXONFailed)
        }
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
            kdebug!("vmcs_field: {:x}", vmcs_field);
            Err(SystemError::EVMWRITEFailed)
        }
    }
}

/// vmread the current VMCS.
pub fn vmx_vmread(vmcs_field: u32) -> Result<u64, SystemError> {
    match unsafe { x86::bits64::vmx::vmread(vmcs_field) } {
        Ok(value) => Ok(value),
        Err(e) => {
            kdebug!("vmx_read fail: {:?}", e);
            Err(SystemError::EVMREADFailed)
        }
    }
}

pub fn vmx_vmptrld(vmcs_pa: u64) -> Result<(), SystemError> {
    match unsafe { x86::bits64::vmx::vmptrld(vmcs_pa) } {
        Ok(_) => Ok(()),
        Err(_) => Err(SystemError::EVMPRTLDFailed),
    }
}

pub fn vmx_vmlaunch() -> Result<(), SystemError> {
    let host_rsp = VmcsFields::HOST_RSP as u32;
    let host_rip = VmcsFields::HOST_RIP as u32;
    unsafe {
        asm!(
            "push    rbp",
            "push    rcx",
            "push    rdx",
            "push    rsi",
            "push    rdi",
            "vmwrite {0:r}, rsp",
            "lea rax, 1f[rip]",
            "vmwrite {1:r}, rax",
            "vmlaunch",
            "1:",
            "pop    rdi",
            "pop    rsi",
            "pop    rdx",
            "pop    rcx",
            "pop    rbp",
            "call vmx_return",
            in(reg) host_rsp,
            in(reg) host_rip,
            clobber_abi("C"),
        )
    }
    Ok(())
    // match unsafe { x86::bits64::vmx::vmlaunch() } {
    //     Ok(_) => Ok(()),
    //     Err(e) => {
    //         kdebug!("vmx_launch fail: {:?}", e);
    //         Err(SystemError::EVMLAUNCHFailed)
    //     },
    // }
}

pub fn vmx_vmclear(vmcs_pa: u64) -> Result<(), SystemError> {
    match unsafe { x86::bits64::vmx::vmclear(vmcs_pa) } {
        Ok(_) => Ok(()),
        Err(_) => Err(SystemError::EVMPRTLDFailed),
    }
}
