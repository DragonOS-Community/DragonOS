#![doc = "DragonOS-maintained static-keys backend interface."]
#![no_std]
#![allow(clippy::needless_doctest_main)]

mod arch;
pub mod code_manipulate;
mod os;

use code_manipulate::{CodePatchBackend, CodePatchTransaction};

/// Entries in the __static_keys section, used for record addresses to modify JMP/NOP.
///
/// The fields of this struct are all **relative address** instead of absolute address considering ASLR.
/// Specifically, it is the relative address between target address and the address of field that record it.
///
/// The relative addresses will be updated to absolute address after calling [`global_init`]. This
/// is because we want to sort the jump entries in place.
#[derive(Debug)]
struct JumpEntry {
    /// Address of the JMP/NOP instruction to be modified.
    code: usize,
    /// Address of the JMP destination
    target: usize,
    /// Address of associated static key.
    ///
    /// Since the static key has at least 8-byte alignment, the LSB bit of this address is used
    /// to record whether the likely branch is true branch or false branch in order to get right instruction
    /// to replace old one.
    key: usize,
}

impl JumpEntry {
    /// Update fields to be absolute address
    #[cfg(not(all(target_os = "windows", target_arch = "x86_64")))]
    fn make_relative_address_absolute(&mut self) {
        self.code = ((&raw const self.code) as usize).wrapping_add(self.code);
        self.target = ((&raw const self.target) as usize).wrapping_add(self.target);
        self.key = ((&raw const self.key) as usize).wrapping_add(self.key);
    }

    // For Win64, the relative address is truncated into 32bit.
    // See https://github.com/llvm/llvm-project/blob/862d837e483437b33f5588f89e62085de3a806b9/llvm/lib/Target/X86/MCTargetDesc/X86WinCOFFObjectWriter.cpp#L48-L51
    /// Update fields to be absolute address
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    fn make_relative_address_absolute(&mut self) {
        let code = (self.code as i32) as i64 as usize;
        self.code = ((&raw const self.code) as usize).wrapping_add(code);
        let target = (self.target as i32) as i64 as usize;
        self.target = ((&raw const self.target) as usize).wrapping_add(target);
        let key = (self.key as i32) as i64 as usize;
        self.key = ((&raw const self.key) as usize).wrapping_add(key);
    }

    /// Absolute address of the JMP/NOP instruction to be modified
    fn code_addr(&self) -> usize {
        self.code
    }

    /// Absolute address of the JMP destination
    fn target_addr(&self) -> usize {
        self.target
    }

    /// Absolute address of the associated static key
    fn key_addr(&self) -> usize {
        self.key & !1usize
    }

    /// Return `true` if the likely branch is true branch.
    fn likely_branch_is_true(&self) -> bool {
        (self.key & 1usize) != 0
    }

    /// Unique reference to associated key
    fn key_mut<M: CodePatchBackend, const S: bool>(&self) -> &'static mut GenericStaticKey<M, S> {
        unsafe { &mut *(self.key_addr() as *mut GenericStaticKey<M, S>) }
    }

    /// Whether this jump entry is dummy
    fn is_dummy(&self) -> bool {
        self.code == 0
    }

    /// Create a dummy jump entry
    #[allow(unused)]
    const fn dummy() -> Self {
        Self {
            code: 0,
            target: 0,
            key: 0,
        }
    }
}

/// Static key generic over code manipulator.
///
/// The `M: CodePatchBackend` is required since when toggling the static key, the instructions recorded
/// at associated jump entries need to be modified, which reside in `.text` section, which is a normally
/// non-writable memory region. As a result, we need to change the protection of such memory region.
///
/// The `const S: bool` indicates the initial status of this key. This value is determined
/// at compile time, and only affect the initial generation of branch layout. All subsequent
/// manually disabling and enabling will not be affected by the initial status. The struct
/// layout is also consistent with different initial status. As a result, it is safe
/// to assign arbitrary status to the static key generic when using.
pub struct GenericStaticKey<M: CodePatchBackend, const S: bool> {
    /// Whether current key is true or false
    ///
    /// This field is defined as `AtomicBool` to allow interior mutability of static variables to avoid
    /// creating mutable static.
    enabled: core::sync::atomic::AtomicBool,
    /// Start address of associated jump entries.
    ///
    /// The jump entries are sorted based on associated static key address in [`global_init`][Self::global_init]
    /// function. As a result, all jump entries associated with this static key are adjcent to each other.
    ///
    /// This value is 0 at static. After calling [`global_init`][Self::global_init], the value will be assigned
    /// correctly.
    entries: usize,
    /// Phantom data to hold `M`
    phantom: core::marker::PhantomData<M>,
}

/// Static key to hold data about current status and which jump entries are associated with this key.
///
/// For now, it is not encouraged to modify static key in a multi-thread application (which I don't think
/// is a common situation).
pub type StaticKey<const S: bool> = GenericStaticKey<crate::os::ArchCodeManipulator, S>;
/// A [`StaticKey`] with initial status `true`.
pub type StaticTrueKey = StaticKey<true>;
/// A [`StaticKey`] with initial status `false`.
pub type StaticFalseKey = StaticKey<false>;
/// A [`GenericStaticKey`] with initial status `true`.
pub type RawStaticTrueKey<M> = GenericStaticKey<M, true>;
/// A [`GenericStaticKey`] with initial status `false`.
pub type RawStaticFalseKey<M> = GenericStaticKey<M, false>;

// Insert a dummy static key here, and use this at global_init function. This is
// to avoid linker failure when there is no jump entries, and thus the __static_keys
// section is never defined.
//
// It should work if we just use global_asm to define a dummy jump entry in __static_keys,
// however, it seems a Rust bug to erase sections marked with "R" (retained). If we specify
// --print-gc-sections for linker options, it's strange that linker itself does not
// erase it. IT IS SO STRANGE.
static DUMMY_STATIC_KEY: GenericStaticKey<code_manipulate::DummyCodeManipulator, false> =
    GenericStaticKey::new(false);

impl<M: CodePatchBackend, const S: bool> GenericStaticKey<M, S> {
    /// Whether initial status is `true`
    #[inline(always)]
    pub const fn initial_enabled(&self) -> bool {
        S
    }

    /// Create a new static key with given default value.
    const fn new(enabled: bool) -> Self {
        Self {
            enabled: core::sync::atomic::AtomicBool::new(enabled),
            entries: 0,
            phantom: core::marker::PhantomData,
        }
    }

    /// Get pointer to the start of jump entries which associated with current static key
    fn entries(&self) -> *const JumpEntry {
        self.entries as *const _
    }

    /// Enable this static key (make the value to be `true`). Do nothing if current static key is already enabled.
    ///
    /// # Safety
    ///
    /// This method may be UB if called before [`global_init`] or called in parallel. Never call this method when
    /// there are multi-threads running. Spawn threads after this method is called. This method may manipulate
    /// code region memory protection, and if other threads are executing codes in the same code page, it may
    /// lead to unexpected behaviors.
    pub unsafe fn enable(&self) -> Result<(), M::Error> {
        unsafe { static_key_update(self, true) }
    }

    /// Disable this static key (make the value to be `false`). Do nothing if current static key is already disabled.
    ///
    /// # Safety
    ///
    /// This method may be UB if called before [`global_init`] or called in parallel. Never call this method when
    /// there are multi-threads running. Spawn threads after this method is called. This method may manipulate
    /// code region memory protection, and if other threads are executing codes in the same code page, it may
    /// lead to unexpected behaviors.
    pub unsafe fn disable(&self) -> Result<(), M::Error> {
        unsafe { static_key_update(self, false) }
    }

    /// Get the current status of this static key
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(core::sync::atomic::Ordering::Acquire)
    }
}

/// Count of jump entries in __static_keys section. Note that
/// there will be several dummy jump entries inside this section.
pub fn jump_entries_count() -> usize {
    let jump_entry_start_addr = &raw mut os::JUMP_ENTRY_START;
    let jump_entry_stop_addr = &raw mut os::JUMP_ENTRY_STOP;
    unsafe { jump_entry_stop_addr.offset_from(jump_entry_start_addr) as usize }
}

// ---------------------------- Create ----------------------------
/// Global state to make sure [`global_init`] is called only once
static GLOBAL_INIT_STATE: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
const UNINITIALIZED: usize = 0;
const INITIALIZING: usize = 1;
const INITIALIZED: usize = 2;

/// Initialize the static keys data. Always call this method at beginning of application, before using any static key related
/// functionalities.
///
/// This function should be called only once. If calling this method multiple times in multi-threads, only the first invocation
/// will take effect.
pub fn global_init() {
    // DUMMY_STATIC_KEY will never changed, and this will always be a NOP.
    // Doing this to make sure there are at least one jump entry.
    if static_branch_unlikely!(DUMMY_STATIC_KEY) {
        return;
    }

    // This logic is taken from log::set_logger_inner
    match GLOBAL_INIT_STATE.compare_exchange(
        UNINITIALIZED,
        INITIALIZING,
        core::sync::atomic::Ordering::Acquire,
        core::sync::atomic::Ordering::Relaxed,
    ) {
        Ok(UNINITIALIZED) => {
            global_init_inner();
            GLOBAL_INIT_STATE.store(INITIALIZED, core::sync::atomic::Ordering::Release);
            // Successful init
        }
        Err(INITIALIZING) => {
            while GLOBAL_INIT_STATE.load(core::sync::atomic::Ordering::Relaxed) == INITIALIZING {
                core::hint::spin_loop();
            }
            // Other has inited
        }
        _ => {
            // Other has inited
        }
    }
}

/// Inner function to [`global_init`]
fn global_init_inner() {
    let jump_entry_start_addr = &raw mut os::JUMP_ENTRY_START;
    let jump_entry_stop_addr = &raw mut os::JUMP_ENTRY_STOP;
    let jump_entry_len =
        unsafe { jump_entry_stop_addr.offset_from(jump_entry_start_addr) as usize };
    let jump_entries =
        unsafe { core::slice::from_raw_parts_mut(jump_entry_start_addr, jump_entry_len) };
    // Update jump entries to be absolute address
    for jump_entry in jump_entries.iter_mut() {
        if jump_entry.is_dummy() {
            continue;
        }
        jump_entry.make_relative_address_absolute();
    }
    // The jump entries are sorted by key address and code address
    jump_entries.sort_unstable_by_key(|jump_entry| (jump_entry.key_addr(), jump_entry.code_addr()));
    // Update associated static keys
    let mut last_key_addr = 0;
    for jump_entry in jump_entries {
        if jump_entry.is_dummy() {
            continue;
        }
        let key_addr = jump_entry.key_addr();
        if key_addr == last_key_addr {
            continue;
        }
        let entries_start_addr = jump_entry as *mut _ as usize;
        // The M and S generic is useless here
        let key = jump_entry.key_mut::<code_manipulate::DummyCodeManipulator, true>();
        // Here we assign associated static key with the start address of jump entries
        key.entries = entries_start_addr;
        last_key_addr = key_addr;
    }
}

/// Create a new static key with `false` as initial value.
///
/// This method should be called to initialize a static mut static key. It is UB to use this method
/// to create a static key on stack or heap, and use this static key to control branches.
///
/// Use [`define_static_key_false`] for short.
pub const fn new_static_false_key() -> StaticFalseKey {
    StaticFalseKey::new(false)
}

/// Create a new static key with `true` as initial value.
///
/// This method should be called to initialize a static mut static key. It is UB to use this method
/// to create a static key on stack or heap, and use this static key to control branches.
///
/// Use [`define_static_key_true`] for short.
pub const fn new_static_true_key() -> StaticTrueKey {
    StaticTrueKey::new(true)
}

/// Create a new static key generic over code manipulator with `false` as initial value.
///
/// This method should be called to initialize a static mut static key. It is UB to use this method
/// to create a static key on stack or heap, and use this static key to control branches.
///
/// Use [`define_static_key_false_generic`] for short.
pub const fn new_static_false_key_generic<M: CodePatchBackend>() -> RawStaticFalseKey<M> {
    RawStaticFalseKey::<M>::new(false)
}

/// Create a new static key generic over code manipulator with `true` as initial value.
///
/// This method should be called to initialize a static mut static key. It is UB to use this method
/// to create a static key on stack or heap, and use this static key to control branches.
///
/// Use [`define_static_key_true_generic`] for short.
pub const fn new_static_true_key_generic<M: CodePatchBackend>() -> RawStaticTrueKey<M> {
    RawStaticTrueKey::<M>::new(true)
}

/// Define a static key with `false` as initial value.
///
/// This macro will define a static mut variable without documentations and visibility modifiers.
/// Use [`new_static_false_key`] for customization.
///
/// # Usage
///
/// ```rust
/// use static_keys::define_static_key_false;
///
/// define_static_key_false!(MY_FALSE_STATIC_KEY);
/// ```
#[macro_export]
macro_rules! define_static_key_false {
    ($key: ident) => {
        #[used]
        static $key: $crate::StaticFalseKey = $crate::new_static_false_key();
    };
}

/// Define a static key with `true` as initial value.
///
/// This macro will define a static mut variable without documentations and visibility modifiers.
/// Use [`new_static_true_key`] for customization.
///
/// # Usage
///
/// ```rust
/// use static_keys::define_static_key_true;
///
/// define_static_key_true!(MY_TRUE_STATIC_KEY);
/// ```
#[macro_export]
macro_rules! define_static_key_true {
    ($key: ident) => {
        #[used]
        static $key: $crate::StaticTrueKey = $crate::new_static_true_key();
    };
}

/// Define a static key generic over code manipulator with `false` as initial value.
///
/// This macro will define a static mut variable without documentations and visibility modifiers.
/// Use [`new_static_false_key_generic`] for customization.
/// # Usage
/// ```rust ignore
/// use static_keys::{define_static_key_false_generic};
/// define_static_key_false_generic!(MY_FALSE_STATIC_KEY, DummyCodeManipulator);
/// ```
#[macro_export]
macro_rules! define_static_key_false_generic {
    ($key: ident, $manipulator: ty) => {
        #[used]
        static $key: $crate::RawStaticFalseKey<$manipulator> =
            $crate::new_static_false_key_generic::<$manipulator>();
    };
}

/// Define a static key generic over code manipulator with `true` as initial value.
///
/// This macro will define a static mut variable without documentations and visibility modifiers.
/// Use [`new_static_true_key_generic`] for customization.
///
/// # Usage
/// ```rust ignore
/// use static_keys::{define_static_key_true_generic};
///
/// define_static_key_true_generic!(MY_TRUE_STATIC_KEY, DummyCodeManipulator);
/// ```
#[macro_export]
macro_rules! define_static_key_true_generic {
    ($key: ident, $manipulator: ty) => {
        #[used]
        static $key: $crate::RawStaticTrueKey<$manipulator> =
            $crate::new_static_true_key_generic::<$manipulator>();
    };
}

// ---------------------------- Update ----------------------------
/// The internal method used for [`GenericStaticKey::enable`] and [`GenericStaticKey::disable`].
///
/// This method will update instructions recorded in each jump entries that associated with thie static key
///
/// # Safety
///
/// This method may be UB if called before [`global_init`] or called in parallel. Never call this method when
/// there are multi-threads running. Spawn threads after this method is called. This method may manipulate
/// code region memory protection, and if other threads are executing codes in the same code page, it may
/// lead to unexpected behaviors.
unsafe fn static_key_update<M: CodePatchBackend, const S: bool>(
    key: &GenericStaticKey<M, S>,
    enabled: bool,
) -> Result<(), M::Error> {
    let mut transaction = M::begin()?;
    if key.enabled.load(core::sync::atomic::Ordering::Acquire) == enabled {
        return transaction.commit();
    }
    let jump_entry_stop_addr = &raw const os::JUMP_ENTRY_STOP;
    let mut jump_entry_addr = key.entries();
    while !jump_entry_addr.is_null() {
        if jump_entry_addr >= jump_entry_stop_addr {
            break;
        }
        let jump_entry = unsafe { &*jump_entry_addr };
        // Not the same key
        if key as *const _ as usize != jump_entry.key_addr() {
            break;
        }

        let replacement = arch::arch_jump_entry_instruction(
            jump_label_type(jump_entry, enabled),
            jump_entry,
        );
        let expected = arch::arch_jump_entry_instruction(
            jump_label_type(jump_entry, !enabled),
            jump_entry,
        );
        unsafe {
            transaction.queue(
                jump_entry.code_addr() as *mut _,
                &expected,
                &replacement,
            )?;
            jump_entry_addr = jump_entry_addr.add(1);
        };
    }
    transaction.commit()?;
    key.enabled
        .store(enabled, core::sync::atomic::Ordering::Release);
    Ok(())
}

/// Type of the instructions to be modified
#[derive(Debug)]
enum JumpLabelType {
    /// 5 byte NOP
    Nop = 0,
    /// 5 byte JMP
    Jmp = 1,
}

/// Update instruction recorded in a single jump entry. This is where magic happens
///
/// # Safety
///
/// This method may be UB if called in parallel. Never call this method when
/// there are multi-threads running. Spawn threads after this method is called. This method may manipulate
/// code region memory protection, and if other threads are executing codes in the same code page, it may
/// lead to unexpected behaviors.
fn jump_label_type(jump_entry: &JumpEntry, enabled: bool) -> JumpLabelType {
    if enabled ^ jump_entry.likely_branch_is_true() {
        JumpLabelType::Jmp
    } else {
        JumpLabelType::Nop
    }
}

// ---------------------------- Use ----------------------------
/// With given branch as likely branch, initialize the instruction here as JMP instruction
#[doc(hidden)]
#[macro_export]
macro_rules! static_key_init_jmp_with_given_branch_likely {
    ($key:path, $branch:expr) => {'my_label: {
        // This is an ugly workaround for https://github.com/rust-lang/rust/issues/128177
        #[cfg(not(all(target_os = "windows", any(target_arch = "x86", target_arch = "x86_64"))))]
        ::core::arch::asm!(
            $crate::arch_static_key_init_jmp_asm_template!(),
            label {
                break 'my_label !$branch;
            },
            sym $key,
            const $branch as usize,
        );
        #[cfg(all(target_os = "windows", any(target_arch = "x86", target_arch = "x86_64")))]
        ::core::arch::asm!(
            $crate::arch_static_key_init_jmp_asm_template!(),
            label {
                break 'my_label !$branch;
            },
            sym $key,
            const $branch as usize,
            options(att_syntax),
        );

        // This branch will be adjcent to the NOP/JMP instruction
        break 'my_label $branch;
    }};
}

/// With given branch as likely branch, initialize the instruction here as NOP instruction
#[doc(hidden)]
#[macro_export]
macro_rules! static_key_init_nop_with_given_branch_likely {
    ($key:path, $branch:expr) => {'my_label: {
        // This is an ugly workaround for https://github.com/rust-lang/rust/issues/128177
        #[cfg(not(all(target_os = "windows", any(target_arch = "x86", target_arch = "x86_64"))))]
        ::core::arch::asm!(
            $crate::arch_static_key_init_nop_asm_template!(),
            label {
                break 'my_label !$branch;
            },
            sym $key,
            const $branch as usize,
        );
        #[cfg(all(target_os = "windows", any(target_arch = "x86", target_arch = "x86_64")))]
        ::core::arch::asm!(
            $crate::arch_static_key_init_nop_asm_template!(),
            label {
                break 'my_label !$branch;
            },
            sym $key,
            const $branch as usize,
            options(att_syntax),
        );

        // This branch will be adjcent to the NOP/JMP instruction
        break 'my_label $branch;
    }};
}

/// Use this in a `if` condition, just like the common [`likely`][core::intrinsics::likely]
/// and [`unlikely`][core::intrinsics::unlikely] intrinsics
#[macro_export]
macro_rules! static_branch_unlikely {
    ($key:path) => {{
        unsafe {
            if $key.initial_enabled() {
                $crate::static_key_init_jmp_with_given_branch_likely! { $key, false }
            } else {
                $crate::static_key_init_nop_with_given_branch_likely! { $key, false }
            }
        }
    }};
}

/// Use this in a `if` condition, just like the common [`likely`][core::intrinsics::likely]
/// and [`unlikely`][core::intrinsics::unlikely] intrinsics
#[macro_export]
macro_rules! static_branch_likely {
    ($key:path) => {{
        unsafe {
            if $key.initial_enabled() {
                $crate::static_key_init_nop_with_given_branch_likely! { $key, true }
            } else {
                $crate::static_key_init_jmp_with_given_branch_likely! { $key, true }
            }
        }
    }};
}
