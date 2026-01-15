//! KVM Clock implementation for DragonOS
//!
//! This module implements the KVM paravirtualized clock source, which provides
//! a stable time reference for virtual machines running on KVM.
//!
//! # References
//! - Linux kernel: arch/x86/kernel/kvmclock.c
//! - KVM documentation: https://www.kernel.org/doc/html/latest/virt/kvm/x86.html

use alloc::boxed::Box;
use alloc::string::ToString;
use alloc::sync::{Arc, Weak};

use crate::arch::MMArch;
use crate::libs::spinlock::SpinLock;
use crate::mm::{MemoryManagementArch, VirtAddr};
use crate::time::clocksource::{
    Clocksource, ClocksourceData, ClocksourceFlags, ClocksourceMask, CycleNum,
};
use log::{info, warn};
use system_error::SystemError;

/// KVM MSR for system time
/// The guest writes the physical address of a struct pvclock_vcpu_time_info
/// to this MSR, and the host updates it periodically.
pub const MSR_KVM_SYSTEM_TIME: u32 = 0x12;

/// KVM MSR for wall clock
/// The guest writes the physical address of a struct pvclock_wall_clock
/// to this MSR, and the host updates it periodically.
pub const MSR_KVM_WALL_CLOCK: u32 = 0x11;

/// The guest sets this bit in the MSR to request that the host
/// stop updating the system time structure.
pub const KVM_MSR_ENABLED: u64 = 0x01;

/// KVM clock frequency in Hz (1 GHz)
pub const KVM_CLOCK_FREQ: u32 = 1000000000;

/// Global KVM clock instance
static mut KVM_CLOCK_INSTANCE: Option<Arc<KvmClock>> = None;

/// PVCLOCK vCPU time information structure
/// This structure is shared between the guest and the host.
/// The host updates the tsc_timestamp, system_time, and tsc_to_system_mul
/// fields periodically.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct PvclockVcpuTimeInfo {
    /// Version number, used for synchronization
    version: u32,
    /// Padding for alignment
    pad0: u32,
    /// TSC value at the time of the last update (64-bit)
    tsc_timestamp: u64,
    /// System time in nanoseconds at the time of the last update (64-bit)
    system_time: u64,
    /// Multiplier to convert TSC to nanoseconds (64-bit)
    tsc_to_system_mul: u64,
    /// Shift to apply to the TSC before multiplying (signed 32-bit)
    tsc_shift: i32,
    /// Flags
    flags: u8,
    /// Padding to match the structure size
    pad: [u8; 2],
}

/// PVCLOCK wall clock structure
/// This structure is shared between the guest and the host.
/// It provides a real-time (wall clock) reference.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct PvclockWallClock {
    /// Version number, used for synchronization
    version: u32,
    /// Padding for alignment
    pad0: u32,
    /// Seconds since epoch
    sec: u32,
    /// Nanoseconds within the second
    nsec: u32,
    /// Padding for alignment to match the structure size
    pad: [u32; 9],
}

/// The per-CPU time information structure
/// This is placed in a fixed memory location that the host can access.
#[repr(C, align(64))]
pub struct KvmClockData {
    /// The PVCLOCK time info structure
    time_info: PvclockVcpuTimeInfo,
    /// Padding to ensure alignment
    _pad: [u8; 64 - core::mem::size_of::<PvclockVcpuTimeInfo>()],
}

/// KVM Clock structure
#[derive(Debug)]
pub struct KvmClock {
    /// Inner data protected by a spinlock
    inner: SpinLock<KvmClockInner>,
}

/// Inner data for KVM Clock
#[derive(Debug)]
struct KvmClockInner {
    /// Clocksource data
    data: ClocksourceData,
    /// Weak reference to self
    self_ref: Weak<KvmClock>,
    /// Whether KVM clock is enabled
    enabled: bool,
    /// The physical address of the clock data (as seen by the host)
    clock_data_pa: u64,
    /// Pointer to the clock data
    clock_data: *mut KvmClockData,
}

unsafe impl Send for KvmClock {}
unsafe impl Sync for KvmClock {}

impl KvmClock {
    /// Create a new KVM clock instance
    pub fn new() -> Arc<Self> {
        let data = ClocksourceData {
            name: "kvm-clock".to_string(),
            // Rating higher than TSC (300) but lower than HPET (500)
            rating: 400,
            // 64-bit mask for nanoseconds
            mask: ClocksourceMask::new(64),
            mult: 0,
            shift: 0,
            max_idle_ns: Default::default(),
            flags: ClocksourceFlags::CLOCK_SOURCE_IS_CONTINUOUS,
            watchdog_last: CycleNum::new(0),
            cs_last: CycleNum::new(0),
            uncertainty_margin: 0,
            maxadj: 0,
            cycle_last: CycleNum::new(0),
        };

        // Allocate memory for the clock data
        // Note: This should be placed in a memory region accessible by the host
        // For now, we use a static allocation
        let clock_data = Box::leak(Box::new(KvmClockData {
            time_info: PvclockVcpuTimeInfo {
                version: 0,
                pad0: 0,
                tsc_timestamp: 0,
                system_time: 0,
                tsc_to_system_mul: 0,
                tsc_shift: 0,
                flags: 0,
                pad: [0; 2],
            },
            _pad: [0; 64 - core::mem::size_of::<PvclockVcpuTimeInfo>()],
        }));

        // Get the physical address of the clock data
        // We need to translate the virtual address to a physical address that the host can access
        let clock_data_virt = VirtAddr::new(clock_data as *const _ as usize);
        let clock_data_pa = unsafe {
            match MMArch::virt_2_phys(clock_data_virt) {
                Some(pa) => pa.data() as u64,
                None => {
                    warn!("Failed to translate clock_data virtual address to physical address");
                    0
                }
            }
        };

        info!("KvmClock::new: clock_data_virt={:#x}, clock_data_pa={:#x}",
              clock_data_virt.data(), clock_data_pa);

        let kvm_clock = Arc::new(KvmClock {
            inner: SpinLock::new(KvmClockInner {
                data,
                self_ref: Weak::default(),
                enabled: false,
                clock_data_pa,
                clock_data,
            }),
        });

        // Set the self reference
        kvm_clock.inner.lock().self_ref = Arc::downgrade(&kvm_clock);

        kvm_clock
    }

    /// Check if running on KVM
    pub fn is_kvm() -> bool {
        // Check CPUID for KVM signature
        // KVM uses "KVMKVMKVM" or "KVMKVMVM" in the hypervisor signature (ebx, ecx, edx)
        let cpuid = unsafe { core::arch::x86_64::__cpuid(0x40000000) };

        let ebx = cpuid.ebx;
        let ecx = cpuid.ecx;
        let edx = cpuid.edx;

        // Debug: print CPUID values
        info!(
            "KVM detection: EAX={:#x}, EBX={:#x}, ECX={:#x}, EDX={:#x}",
            cpuid.eax, ebx, ecx, edx
        );

        // KVM signature patterns found in different virtualization implementations
        // The signature "KVM" should appear in ebx, ecx, edx registers

        // Pattern 1: EBX="KVMV", ECX="VMKV", EDX has "M" -> "KVMKVMVM"
        let pattern1 = ebx == 0x4b4d564b && ecx == 0x564b4d56 && (edx & 0xFF) == 0x4d;

        // Pattern 2: EBX starts with "KVM" (most permissive, works with different implementations)
        let pattern2 = (ebx & 0xFFFFFF) == 0x4b564d; // First 3 bytes are "KVM" (little endian)

        // Pattern 3: Check for "KVM" in ebx (byte-by-byte for clarity)
        let pattern3 = ((ebx & 0xFF) == 0x4b) &&       // First byte is 'K'
                       ((ebx >> 8) & 0xFF) == 0x4d &&  // Second byte is 'M'
                       ((ebx >> 16) & 0xFF) == 0x56;  // Third byte is 'V'

        let result = pattern1 || pattern2 || pattern3;

        info!(
            "KVM detection result: {} (pattern1={}, pattern2={}, pattern3={})",
            result, pattern1, pattern2, pattern3
        );

        result
    }

    /// Initialize the KVM clock
    pub fn init(&self) -> Result<(), SystemError> {
        let mut inner = self.inner.lock_irqsave();

        // Check if we're running on KVM
        if !Self::is_kvm() {
            info!("Not running on KVM, skipping KVM clock initialization");
            return Err(SystemError::ENODEV);
        }

        // Check if we have a valid physical address
        if inner.clock_data_pa == 0 {
            warn!("KVM clock: invalid physical address (0), cannot initialize");
            return Err(SystemError::EINVAL);
        }

        info!("Initializing KVM clock");

        // Set up the MSR to point to our clock data
        let msr_value = inner.clock_data_pa | KVM_MSR_ENABLED;

        info!("KVM clock: writing MSR {:#x} with value {:#x}",
              MSR_KVM_SYSTEM_TIME, msr_value);

        // Write to the MSR
        unsafe {
            core::arch::asm!(
                "wrmsr",
                in("ecx") MSR_KVM_SYSTEM_TIME,
                in("eax") (msr_value as u32),
                in("edx") ((msr_value >> 32) as u32),
            );
        }

        // Verify the MSR write by reading it back
        let mut eax: u32 = 0;
        let mut edx: u32 = 0;
        unsafe {
            core::arch::asm!(
                "rdmsr",
                in("ecx") MSR_KVM_SYSTEM_TIME,
                lateout("eax") eax,
                lateout("edx") edx,
            );
        }
        let msr_readback = ((edx as u64) << 32) | (eax as u64);
        info!("KVM clock: MSR readback: {:#x}", msr_readback);

        inner.enabled = true;
        info!("KVM clock initialized successfully");
        info!("  Clock data PA: {:#x}", inner.clock_data_pa);

        Ok(())
    }

    /// Read the current TSC value
    #[inline]
    fn read_tsc() -> u64 {
        unsafe { core::arch::x86_64::_rdtsc() as u64 }
    }

    /// Read the PVCLOCK time info with proper synchronization
    fn read_pvclock(clock_data: *const KvmClockData) -> u64 {
        let mut last_version: u32;
        let mut version: u32;
        let mut tsc_timestamp: u64;
        let mut system_time: u64;
        let mut tsc_to_system_mul: u64;
        let mut tsc_shift: i32;

        // Add a timeout to prevent infinite loops
        let max_retries = 100000;
        let mut retries = 0;

        loop {
            // Read the version
            unsafe {
                version = (*clock_data).time_info.version;
            }

            // Memory barrier
            core::sync::atomic::fence(core::sync::atomic::Ordering::Acquire);

            // Read the time values
            unsafe {
                tsc_timestamp = (*clock_data).time_info.tsc_timestamp;
                system_time = (*clock_data).time_info.system_time;
                tsc_to_system_mul = (*clock_data).time_info.tsc_to_system_mul;
                tsc_shift = (*clock_data).time_info.tsc_shift;
            }

            // Memory barrier
            core::sync::atomic::fence(core::sync::atomic::Ordering::Acquire);

            // Read the version again
            unsafe {
                last_version = (*clock_data).time_info.version;
            }

            // Check if the version is odd (being updated) or changed
            // Also check that version is non-zero (host has initialized the data)
            if version == last_version && version % 2 == 0 && version != 0 {
                break;
            }

            // If the version is odd or changed, the data was being updated
            // Try again
            retries += 1;
            if retries >= max_retries {
                // Give up after too many retries
                warn!("KVM clock: timeout waiting for stable version (version={})", version);
                return 0;
            }
        }

        // Version is still 0 - host hasn't initialized the clock yet
        if version == 0 {
            warn!("KVM clock: host not initialized (version=0)");
            return 0;
        }

        // If tsc_to_system_mul is 0, the host hasn't initialized the clock yet
        if tsc_to_system_mul == 0 {
            warn!("KVM clock: host not providing clock data (tsc_to_system_mul=0, version={})",
                  version);
            warn!("  tsc_timestamp={}, system_time={}, tsc_shift={}",
                  tsc_timestamp, system_time, tsc_shift);
            // Return 0 to indicate the clock isn't working
            return 0;
        }

        // Calculate the current system time
        let tsc_now = Self::read_tsc();
        let tsc_delta = tsc_now.wrapping_sub(tsc_timestamp);

        // Apply scale and shift
        // tsc_to_system_mul is a 64-bit fixed-point number:
        // - high 32 bits: integer part
        // - low 32 bits: fractional part
        let scaled = if tsc_shift >= 0 {
            (tsc_delta << (tsc_shift as u32)).saturating_mul(tsc_to_system_mul)
        } else {
            (tsc_delta >> ((-tsc_shift) as u32)).saturating_mul(tsc_to_system_mul)
        };

        // Extract the nanoseconds by taking the high 32 bits
        let delta_ns = scaled >> 32;

        system_time.wrapping_add(delta_ns)
    }

    /// Read the wall clock time
    pub fn read_wall_clock() -> Option<(u32, u32)> {
        if !Self::is_kvm() {
            return None;
        }

        // Allocate memory for the wall clock data
        let wall_clock_data = Box::leak(Box::new(PvclockWallClock {
            version: 0,
            pad0: 0,
            sec: 0,
            nsec: 0,
            pad: [0; 9],
        }));

        let wall_clock_pa = wall_clock_data as *const _ as u64;

        // Write to the MSR
        unsafe {
            core::arch::asm!(
                "wrmsr",
                in("ecx") MSR_KVM_WALL_CLOCK,
                in("eax") (wall_clock_pa as u32),
                in("edx") ((wall_clock_pa >> 32) as u32),
            );
        }

        // Memory barrier
        core::sync::atomic::fence(core::sync::atomic::Ordering::Acquire);

        // Read the wall clock data
        let mut version: u32;
        let mut last_version: u32;
        let mut sec: u32;
        let mut nsec: u32;

        loop {
            unsafe {
                version = (*wall_clock_data).version;
            }

            core::sync::atomic::fence(core::sync::atomic::Ordering::Acquire);

            unsafe {
                sec = (*wall_clock_data).sec;
                nsec = (*wall_clock_data).nsec;
            }

            core::sync::atomic::fence(core::sync::atomic::Ordering::Acquire);

            unsafe {
                last_version = (*wall_clock_data).version;
            }

            if version == last_version && version % 2 == 0 {
                break;
            }
        }

        Some((sec, nsec))
    }
}

impl Clocksource for KvmClock {
    fn read(&self) -> CycleNum {
        let inner = self.inner.lock_irqsave();

        if !inner.enabled {
            // Fallback to TSC if not enabled
            return CycleNum::new(Self::read_tsc());
        }

        let ns = Self::read_pvclock(inner.clock_data);
        CycleNum::new(ns)
    }

    fn clocksource_data(&self) -> ClocksourceData {
        self.inner.lock_irqsave().data.clone()
    }

    fn clocksource(&self) -> Arc<dyn Clocksource> {
        self.inner.lock_irqsave().self_ref.upgrade().unwrap()
    }

    fn update_clocksource_data(&self, data: ClocksourceData) -> Result<(), SystemError> {
        let mut inner = self.inner.lock_irqsave();
        let d = &mut inner.data;
        d.set_name(data.name);
        d.set_rating(data.rating);
        d.set_mask(data.mask);
        d.set_mult(data.mult);
        d.set_shift(data.shift);
        d.set_max_idle_ns(data.max_idle_ns);
        d.set_flags(data.flags);
        d.watchdog_last = data.watchdog_last;
        d.cs_last = data.cs_last;
        d.set_uncertainty_margin(data.uncertainty_margin);
        d.set_maxadj(data.maxadj);
        d.cycle_last = data.cycle_last;
        Ok(())
    }

    fn enable(&self) -> Result<i32, SystemError> {
        self.init()?;
        Ok(0)
    }
}

/// Get the global KVM clock instance
pub fn kvm_clock() -> Option<Arc<KvmClock>> {
    unsafe { KVM_CLOCK_INSTANCE.as_ref().map(|c| c.clone()) }
}

/// Initialize the KVM clock and register it as a clock source
pub fn init_kvm_clocksource() -> Result<(), SystemError> {
    info!("Checking for KVM clock support...");

    // Check if we're running on KVM
    if !KvmClock::is_kvm() {
        info!("Not running on KVM, skipping KVM clock");
        return Err(SystemError::ENODEV);
    }

    // Create the KVM clock
    let kvm_clock = KvmClock::new();

    // Try to initialize it
    if let Err(e) = kvm_clock.init() {
        warn!("Failed to initialize KVM clock: {:?}", e);
        return Err(e);
    }

    // Store the instance
    unsafe {
        KVM_CLOCK_INSTANCE = Some(kvm_clock.clone());
    }

    // Register the clock source
    // KVM clock provides nanoseconds directly at 1 GHz
    match (kvm_clock as Arc<dyn Clocksource>).register(1, KVM_CLOCK_FREQ) {
        Ok(_) => {
            info!("KVM clock registered successfully");
            Ok(())
        }
        Err(e) => {
            warn!("Failed to register KVM clock: {:?}", e);
            Err(e)
        }
    }
}
