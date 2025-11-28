//! 内核异常表(Exception Table)
//! 用于处理在系统调用上下文中访问用户空间内存时的页错误

/// 异常表条目
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ExceptionTableEntry {
    /// 可能触发异常的指令地址(相对于表项地址的偏移)
    pub insn_offset: i64,
    /// 修复代码地址(相对于表项地址的偏移)
    pub fixup_offset: i64,
}

impl ExceptionTableEntry {
    /// 获取指令的绝对地址
    pub fn insn_addr(&self) -> usize {
        let self_addr = self as *const Self as usize;
        (self_addr as i64 + self.insn_offset) as usize
    }

    /// 获取修复代码的绝对地址
    pub fn fixup_addr(&self) -> usize {
        let self_addr = self as *const Self as usize;
        (self_addr as i64 + self.fixup_offset) as usize
    }
}

extern "C" {
    // 链接器脚本中定义的异常表边界
    static __start___ex_table: ExceptionTableEntry;
    static __stop___ex_table: ExceptionTableEntry;
}

/// 异常表管理器
pub struct ExceptionTableManager;

impl ExceptionTableManager {
    /// 在异常表中搜索给定地址的修复代码
    ///
    /// ## 参数
    /// - `fault_addr`: 触发异常的指令地址
    ///
    /// ## 返回值
    /// - `Some(fixup_addr)`: 找到对应的修复地址
    /// - `None`: 未找到(说明不是预期的用户空间访问错误)
    pub fn search_exception_table(fault_addr: usize) -> Option<usize> {
        unsafe {
            let start = &__start___ex_table as *const ExceptionTableEntry;
            let end = &__stop___ex_table as *const ExceptionTableEntry;

            let count =
                ((end as usize) - (start as usize)) / core::mem::size_of::<ExceptionTableEntry>();

            if count == 0 {
                return None;
            }

            let table = core::slice::from_raw_parts(start, count);

            // 二分查找(表在编译时已排序)
            Self::binary_search(table, fault_addr)
        }
    }

    fn binary_search(table: &[ExceptionTableEntry], fault_addr: usize) -> Option<usize> {
        let mut left = 0;
        let mut right = table.len();

        while left < right {
            let mid = left + (right - left) / 2;
            let entry = &table[mid];
            let insn_addr = entry.insn_addr();

            if insn_addr == fault_addr {
                return Some(entry.fixup_addr());
            } else if insn_addr < fault_addr {
                left = mid + 1;
            } else {
                right = mid;
            }
        }

        None
    }
}
