use crate::arch::kvm::VmcsFields::{
    GUEST_CS_ACCESS_RIGHTS, GUEST_CS_BASE, GUEST_CS_LIMIT, GUEST_CS_SELECTOR,
};
use crate::arch::kvm::VmcsFields::{
    GUEST_DS_ACCESS_RIGHTS, GUEST_DS_BASE, GUEST_DS_LIMIT, GUEST_DS_SELECTOR,
};
use crate::arch::kvm::VmcsFields::{
    GUEST_ES_ACCESS_RIGHTS, GUEST_ES_BASE, GUEST_ES_LIMIT, GUEST_ES_SELECTOR,
};
use crate::arch::kvm::VmcsFields::{
    GUEST_FS_ACCESS_RIGHTS, GUEST_FS_BASE, GUEST_FS_LIMIT, GUEST_FS_SELECTOR,
};
use crate::arch::kvm::VmcsFields::{
    GUEST_GS_ACCESS_RIGHTS, GUEST_GS_BASE, GUEST_GS_LIMIT, GUEST_GS_SELECTOR,
};
use crate::arch::kvm::VmcsFields::{
    GUEST_LDTR_ACCESS_RIGHTS, GUEST_LDTR_BASE, GUEST_LDTR_LIMIT, GUEST_LDTR_SELECTOR,
};
use crate::arch::kvm::VmcsFields::{
    GUEST_SS_ACCESS_RIGHTS, GUEST_SS_BASE, GUEST_SS_LIMIT, GUEST_SS_SELECTOR,
};
use crate::arch::kvm::VmcsFields::{
    GUEST_TR_ACCESS_RIGHTS, GUEST_TR_BASE, GUEST_TR_LIMIT, GUEST_TR_SELECTOR,
};
use crate::syscall::SystemError;

use super::vmx_asm_wrapper::vmx_vmwrite;

// pub const TSS_IOPB_BASE_OFFSET: usize = 0x66;
// pub const TSS_BASE_SIZE: usize = 0x68;
// pub const TSS_IOPB_SIZE: usize = 65536 / 8;
// pub const TSS_REDIRECTION_SIZE: usize = 256 / 8;
// pub const RMODE_TSS_SIZE: usize = TSS_BASE_SIZE + TSS_REDIRECTION_SIZE + TSS_IOPB_SIZE + 1;

#[derive(Debug)]
pub struct KvmVmxSegmentField {
    selector: u32,
    base: u32,
    limit: u32,
    access_rights: u32,
}

macro_rules! VMX_SEGMENT_FIELD {
    ($struct_name: ident) => {
        KvmVmxSegmentField {
            selector: concat_idents!(GUEST_, $struct_name, _SELECTOR) as u32,
            base: concat_idents!(GUEST_, $struct_name, _BASE) as u32,
            limit: concat_idents!(GUEST_, $struct_name, _LIMIT) as u32,
            access_rights: concat_idents!(GUEST_, $struct_name, _ACCESS_RIGHTS) as u32,
        }
    };
}
#[derive(FromPrimitive)]
pub enum Sreg {
    ES = 0,
    CS = 1,
    SS = 2,
    DS = 3,
    FS = 4,
    GS = 5,
    TR = 6,
    LDTR = 7,
}

static KVM_VMX_SEGMENT_FIELDS: [KvmVmxSegmentField; 8] = [
    VMX_SEGMENT_FIELD!(ES),
    VMX_SEGMENT_FIELD!(CS),
    VMX_SEGMENT_FIELD!(SS),
    VMX_SEGMENT_FIELD!(DS),
    VMX_SEGMENT_FIELD!(FS),
    VMX_SEGMENT_FIELD!(GS),
    VMX_SEGMENT_FIELD!(TR),
    VMX_SEGMENT_FIELD!(LDTR),
];

pub fn seg_setup(seg: usize) -> Result<(), SystemError> {
    let seg_field = &KVM_VMX_SEGMENT_FIELDS[seg];
    let mut access_rigt = 0x0093;
    if seg == Sreg::CS as usize {
        access_rigt |= 0x08;
    }
    // setup segment fields
    vmx_vmwrite(seg_field.selector, 0)?;
    vmx_vmwrite(seg_field.base, 0)?;
    vmx_vmwrite(seg_field.limit, 0x0000_FFFF)?;
    vmx_vmwrite(seg_field.access_rights, access_rigt)?;

    Ok(())
}
