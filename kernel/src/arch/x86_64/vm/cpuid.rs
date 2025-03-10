use alloc::vec::Vec;

#[derive(Debug, Default, Clone, Copy)]
#[allow(dead_code)]
pub struct KvmCpuidEntry2 {
    pub function: u32,
    pub index: u32,
    pub flags: KvmCpuidFlag,
    pub eax: u32,
    pub ebx: u32,
    pub ecx: u32,
    pub edx: u32,
    padding: [u32; 3],
}

impl KvmCpuidEntry2 {
    pub fn find(
        entries: &Vec<KvmCpuidEntry2>,
        function: u32,
        index: Option<u32>,
    ) -> Option<KvmCpuidEntry2> {
        for e in entries {
            if e.function != function {
                continue;
            }

            if !e
                .flags
                .contains(KvmCpuidFlag::KVM_CPUID_FLAG_SIGNIFCANT_INDEX)
                || Some(e.index) == index
            {
                return Some(*e);
            }

            if index.is_none() {
                return Some(*e);
            }
        }

        None
    }
}

bitflags! {
    pub struct KvmCpuidFlag: u32 {
        /// 表示CPUID函数的输入索引值是重要的，它会影响CPUID函数的行为或返回值
        const KVM_CPUID_FLAG_SIGNIFCANT_INDEX = 1 << 0;
        /// 表示CPUID函数是有状态的，即它的行为可能受到先前CPUID函数调用的影响
        const KVM_CPUID_FLAG_STATEFUL_FUNC = 1 << 1;
        /// 表示CPUID函数的状态应该在下一次CPUID函数调用中读取
        const KVM_CPUID_FLAG_STATE_READ_NEXT = 1 << 2;
    }
}

impl Default for KvmCpuidFlag {
    fn default() -> Self {
        Self::empty()
    }
}
