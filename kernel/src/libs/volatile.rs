macro_rules! volatile_read {
    ($data: expr) => {
        unsafe { core::ptr::read_volatile(core::ptr::addr_of!($data)) }
    };
}

macro_rules! volatile_write {
    ($data: expr, $value: expr) => {
        unsafe { core::ptr::write_volatile(core::ptr::addr_of_mut!($data), $value) }
    };
}

/// @brief: 用于volatile设置某些bits
/// @param val: 设置这些位
/// @param flag: true表示设置这些位为1; false表示设置这些位为0;
macro_rules! volatile_set_bit {
    ($data: expr, $val: expr, $flag: expr) => {
        volatile_write!(
            $data,
            match $flag {
                true => core::ptr::read_volatile(core::ptr::addr_of!($data)) | $val,
                false => core::ptr::read_volatile(core::ptr::addr_of!($data)) & (!$val),
            }
        )
    };
}

/// @param data: volatile变量
/// @param bits: 置1的位才有效，表示写这些位
/// @param val: 要写的值
/// 比如: 写 x 的 2至8bit， 为 10, 可以这么写 volatile_write_bit(x, (1<<8)-(1<<2), 10<<2);    
macro_rules! volatile_write_bit {
    ($data: expr, $bits: expr, $val: expr) => {
        volatile_set_bit!($data, $bits, false);
        volatile_set_bit!($data, ($val) & ($bits), true);
    };
}

/// 以下代码来自于virtio-drivers 0.2.0
/// 在对已经MMIO映射对虚拟地址的寄存器的操作中，我们经常遇到有的寄存器是只读或可读写的
/// 那么我们就可以使用结构体ReadOnly WriteOnly Volatile对其进行区分
/// 例：
/// #[repr(C)]
/// struct CommonCfg {
///     device_feature_select: Volatile<u32>,
///     device_feature: ReadOnly<u32>,
///     driver_feature_select: Volatile<u32>,
///     driver_feature: Volatile<u32>,
///     msix_config: Volatile<u16>,
///     num_queues: ReadOnly<u16>,
///     device_status: Volatile<u8>,
///     config_generation: ReadOnly<u8>,
///     queue_select: Volatile<u16>,
///     queue_size: Volatile<u16>,
///     queue_msix_vector: Volatile<u16>,
///     queue_enable: Volatile<u16>,
///     queue_notify_off: Volatile<u16>,
///     queue_desc: Volatile<u64>,
///     queue_driver: Volatile<u64>,
///     queue_device: Volatile<u64>,
/// }
///
/// 对CommonCfg里面的某个寄存器进行读写：
/// volwrite!(self.common_cfg, queue_enable, 0);
///
/// 这样做不仅使代码的可读性提高了，也避免了对只读寄存器进行写入的误操作
/// 只读寄存器
#[derive(Default)]
#[repr(transparent)]
pub struct ReadOnly<T: Copy>(T);

#[allow(dead_code)]
impl<T: Copy> ReadOnly<T> {
    /// Construct a new instance for testing.
    pub fn new(value: T) -> Self {
        Self(value)
    }
}

/// 只写寄存器
#[derive(Default)]
#[repr(transparent)]
pub struct WriteOnly<T: Copy>(T);

/// 写读寄存器
#[derive(Default)]
#[repr(transparent)]
pub struct Volatile<T: Copy>(T);

#[allow(dead_code)]
impl<T: Copy> Volatile<T> {
    /// Construct a new instance for testing.
    pub fn new(value: T) -> Self {
        Self(value)
    }
}

/// A trait implemented by MMIO registers which may be read from.
pub trait VolatileReadable<T> {
    /// Performs a volatile read from the MMIO register.
    unsafe fn vread(self) -> T;
}

impl<T: Copy> VolatileReadable<T> for *const ReadOnly<T> {
    unsafe fn vread(self) -> T {
        self.read_volatile().0
    }
}

impl<T: Copy> VolatileReadable<T> for *const Volatile<T> {
    unsafe fn vread(self) -> T {
        self.read_volatile().0
    }
}

/// A trait implemented by MMIO registers which may be written to.
pub trait VolatileWritable<T> {
    /// Performs a volatile write to the MMIO register.
    unsafe fn vwrite(self, value: T);
}

impl<T: Copy> VolatileWritable<T> for *mut WriteOnly<T> {
    unsafe fn vwrite(self, value: T) {
        (self as *mut T).write_volatile(value)
    }
}

impl<T: Copy> VolatileWritable<T> for *mut Volatile<T> {
    unsafe fn vwrite(self, value: T) {
        (self as *mut T).write_volatile(value)
    }
}

/// Performs a volatile read from the given field of pointer to a struct representing an MMIO region.
///
/// # Usage
/// ```compile_fail
/// # use core::ptr::NonNull;
/// # use virtio_drivers::volatile::{ReadOnly, volread};
/// struct MmioDevice {
///   field: ReadOnly<u32>,
/// }
///
/// let device: NonNull<MmioDevice> = NonNull::new(0x1234 as *mut MmioDevice).unwrap();
/// let value = unsafe { volread!(device, field) };
/// ```
macro_rules! volread {
    ($nonnull:expr, $field:ident) => {
        crate::libs::volatile::VolatileReadable::vread(core::ptr::addr_of!(
            (*$nonnull.as_ptr()).$field
        ))
    };
}

/// Performs a volatile write to the given field of pointer to a struct representing an MMIO region.
///
/// # Usage
/// ```compile_fail
/// # use core::ptr::NonNull;
/// # use virtio_drivers::volatile::{WriteOnly, volread};
/// struct MmioDevice {
///   field: WriteOnly<u32>,
/// }
///
/// let device: NonNull<MmioDevice> = NonNull::new(0x1234 as *mut MmioDevice).unwrap();
/// unsafe { volwrite!(device, field, 42); }
/// ```
macro_rules! volwrite {
    ($nonnull:expr, $field:ident, $value:expr) => {
        crate::libs::volatile::VolatileWritable::vwrite(
            core::ptr::addr_of_mut!((*$nonnull.as_ptr()).$field),
            $value,
        )
    };
}

pub(crate) use volread;
pub(crate) use volwrite;
