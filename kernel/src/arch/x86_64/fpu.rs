use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

/// XSAVE area 的最大大小
/// 对于 AVX: 约 832 字节 (512 + 64 header + 256 YMM)
/// 对于 AVX-512: 可能超过 2KB
/// 我们目前只支持到 AVX，因此使用 1024 字节足够
/// 这样可以减少每个进程的内存占用
const MAX_XSAVE_SIZE: usize = 1024;

/// 全局变量：是否使用 XSAVE（而不是 FXSAVE）
static USE_XSAVE: AtomicBool = AtomicBool::new(false);

/// 全局变量：XSAVE 特性掩码
static XSAVE_FEATURE_MASK: AtomicU64 = AtomicU64::new(0);

/// 全局变量：XSAVE area 大小
static XSAVE_AREA_SIZE: AtomicUsize = AtomicUsize::new(512);

/// XSAVE 特性位
pub const XFEATURE_X87: u64 = 1 << 0;
pub const XFEATURE_SSE: u64 = 1 << 1;
pub const XFEATURE_AVX: u64 = 1 << 2;

/// FPU/SSE/AVX 状态保存结构
/// 使用 XSAVE 指令保存时需要 64 字节对齐
#[repr(C, align(64))]
#[derive(Debug)]
pub struct FpState {
    /// XSAVE/FXSAVE area
    /// 前 512 字节是 FXSAVE 兼容的格式
    /// 512 字节之后是扩展状态组件（AVX 等）
    data: [u8; MAX_XSAVE_SIZE],
}

impl Clone for FpState {
    fn clone(&self) -> Self {
        *self
    }
}

impl Copy for FpState {}

impl Default for FpState {
    fn default() -> Self {
        let mut state = Self {
            data: [0u8; MAX_XSAVE_SIZE],
        };
        state.init_default();
        state
    }
}

impl FpState {
    /// 初始化 XSAVE 支持检测（在内核启动时调用一次）
    pub fn init_xsave_support() {
        use raw_cpuid::CpuId;

        let cpuid = CpuId::new();

        // 检查 XSAVE 支持
        let has_xsave = cpuid
            .get_feature_info()
            .map(|f| f.has_xsave())
            .unwrap_or(false);

        let has_osxsave = cpuid
            .get_feature_info()
            .map(|f| f.has_oxsave())
            .unwrap_or(false);

        if !has_xsave || !has_osxsave {
            log::info!(
                "XSAVE not available, using FXSAVE (xsave={}, osxsave={})",
                has_xsave,
                has_osxsave
            );
            USE_XSAVE.store(false, Ordering::SeqCst);
            XSAVE_AREA_SIZE.store(512, Ordering::SeqCst);
            return;
        }

        // 获取 XCR0 当前值（head.S 会在支持 XSAVE 时设置；通常为 0x3 或 0x7，取决于 AVX 支持）
        let xcr0: u64;
        unsafe {
            let lo: u32;
            let hi: u32;
            core::arch::asm!(
                "xgetbv",
                in("ecx") 0u32,
                out("eax") lo,
                out("edx") hi,
                options(nomem, nostack)
            );
            xcr0 = ((hi as u64) << 32) | (lo as u64);
        }

        // 使用 raw_cpuid 获取 XSAVE area 大小
        let xsave_size = cpuid
            .get_extended_state_info()
            .map(|info| info.xsave_area_size_enabled_features() as usize)
            .unwrap_or(576); // 默认 AVX 最小需求

        let actual_size = xsave_size.clamp(512, MAX_XSAVE_SIZE);

        USE_XSAVE.store(true, Ordering::SeqCst);
        XSAVE_FEATURE_MASK.store(xcr0, Ordering::SeqCst);
        XSAVE_AREA_SIZE.store(actual_size, Ordering::SeqCst);

        log::info!(
            "XSAVE enabled: XCR0=0x{:x}, area_size={} bytes, features: x87={}, SSE={}, AVX={}",
            xcr0,
            actual_size,
            (xcr0 & XFEATURE_X87) != 0,
            (xcr0 & XFEATURE_SSE) != 0,
            (xcr0 & XFEATURE_AVX) != 0,
        );
    }

    /// 创建新的 FpState
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    /// 初始化为默认的 FPU 状态
    fn init_default(&mut self) {
        // FCW (FPU Control Word) = 0x037F
        self.data[0] = 0x7F;
        self.data[1] = 0x03;

        // MXCSR = 0x1F80 (SSE control register) - 位于偏移 24
        self.data[24] = 0x80;
        self.data[25] = 0x1F;

        // 如果使用 XSAVE，需要设置 XSTATE_BV header
        if USE_XSAVE.load(Ordering::Relaxed) {
            let mask = XSAVE_FEATURE_MASK.load(Ordering::Relaxed);
            // XSTATE_BV 在偏移 512 处
            self.data[512..520].copy_from_slice(&mask.to_le_bytes());
        }
    }

    /// 保存当前 CPU 的 FPU/SSE/AVX 状态
    #[inline]
    pub fn save(&mut self) {
        if USE_XSAVE.load(Ordering::Relaxed) {
            self.xsave();
        } else {
            self.fxsave();
        }
    }

    /// 恢复 FPU/SSE/AVX 状态到当前 CPU
    #[inline]
    pub fn restore(&self) {
        if USE_XSAVE.load(Ordering::Relaxed) {
            self.xrstor();
        } else {
            self.fxrstor();
        }
    }

    /// 使用 FXSAVE64 指令保存（仅 SSE）
    #[inline]
    fn fxsave(&mut self) {
        unsafe {
            core::arch::asm!(
                "fxsave64 [{}]",
                in(reg) self.data.as_mut_ptr(),
                options(nostack)
            );
        }
    }

    /// 使用 FXRSTOR64 指令恢复（仅 SSE）
    #[inline(never)]
    fn fxrstor(&self) {
        unsafe {
            core::arch::asm!(
                "fxrstor64 [{}]",
                in(reg) self.data.as_ptr(),
                // FXRSTOR 会修改 x87/SSE 状态（含 XMM 寄存器等）。
                // 声明 ABI clobber 以防止编译器假设向量寄存器值在该 asm 前后保持不变。
                clobber_abi("C"),
                options(nostack, preserves_flags, readonly)
            );
        }
    }

    /// 使用 XSAVE64 指令保存扩展状态（包括 AVX）
    #[inline]
    fn xsave(&mut self) {
        let mask = XSAVE_FEATURE_MASK.load(Ordering::Relaxed);
        unsafe {
            core::arch::asm!(
                "xsave64 [{}]",
                in(reg) self.data.as_mut_ptr(),
                in("eax") mask as u32,
                in("edx") (mask >> 32) as u32,
                options(nostack)
            );
        }
    }

    /// 使用 XRSTOR64 指令恢复扩展状态（包括 AVX）
    #[inline(never)]
    fn xrstor(&self) {
        let mask = XSAVE_FEATURE_MASK.load(Ordering::Relaxed);
        unsafe {
            core::arch::asm!(
                "xrstor64 [{}]",
                in(reg) self.data.as_ptr(),
                in("eax") mask as u32,
                in("edx") (mask >> 32) as u32,
                // XRSTOR 会修改 x87/SSE/AVX 状态（含 XMM/YMM 寄存器等）。
                // 声明 ABI clobber 以防止编译器假设向量寄存器值在该 asm 前后保持不变。
                clobber_abi("C"),
                options(nostack, preserves_flags, readonly)
            );
        }
    }

    /// 清空浮点寄存器并恢复默认状态
    #[allow(dead_code)]
    pub fn clear(&mut self) {
        self.data.fill(0);
        self.init_default();
        self.restore();
    }

    /// 获取底层数据的引用（用于信号处理等）
    pub fn as_bytes(&self) -> &[u8] {
        let size = XSAVE_AREA_SIZE.load(Ordering::Relaxed);
        &self.data[..size]
    }

    /// 获取底层数据的可变引用（用于信号处理等）
    pub fn as_bytes_mut(&mut self) -> &mut [u8] {
        let size = XSAVE_AREA_SIZE.load(Ordering::Relaxed);
        &mut self.data[..size]
    }

    /// 获取 FXSAVE 兼容区域（前 512 字节）的引用
    /// 用于与旧代码兼容
    pub fn legacy_region(&self) -> &[u8; 512] {
        self.data[..512].try_into().unwrap()
    }

    /// 获取 FXSAVE 兼容区域的可变引用
    pub fn legacy_region_mut(&mut self) -> &mut [u8; 512] {
        (&mut self.data[..512]).try_into().unwrap()
    }

    /// 返回是否使用 XSAVE
    pub fn is_xsave_enabled() -> bool {
        USE_XSAVE.load(Ordering::Relaxed)
    }

    /// 返回 XSAVE area 大小
    pub fn xsave_area_size() -> usize {
        XSAVE_AREA_SIZE.load(Ordering::Relaxed)
    }
}
