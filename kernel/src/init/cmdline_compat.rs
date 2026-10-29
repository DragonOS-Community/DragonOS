//! Linux-compatible boot parameters that DragonOS currently consumes as no-ops.
//!
//! These registrations prevent Linux-recognized kernel parameters from being
//! forwarded to PID 1 as unknown init argv/env entries. They intentionally do
//! not implement subsystem behavior that DragonOS does not have yet.

// Linux/x86 uses this to skip an IO-APIC timer check. DragonOS currently has
// no equivalent check path, so the parameter is consumed for boot compatibility.
#[cfg(target_arch = "x86_64")]
kernel_cmdline_param_arg!(NO_TIMER_CHECK_PARAM, no_timer_check, false, false);

// Linux/x86 uses this to disable SMP alternatives replacement. DragonOS does
// not implement that Linux alternatives path yet.
#[cfg(target_arch = "x86_64")]
kernel_cmdline_param_arg!(NOREPLACE_SMP_PARAM, "noreplace-smp", false, false);

// DragonOS does not have panic timeout reboot handling yet.
kernel_cmdline_param_kv!(PANIC_TIMEOUT_PARAM, panic, "");

// Audit, md raid autodetect, early printk target selection, mitigation policy,
// and high-resolution timer mode are not implemented as Linux-compatible
// runtime controls yet. Register them so Linux-known boot parameters do not
// leak into init env.
kernel_cmdline_param_kv!(AUDIT_PARAM, audit, "");
kernel_cmdline_param_kv!(RAID_PARAM, raid, "");
kernel_cmdline_param_kv!(EARLY_PRINTK_PARAM, earlyprintk, "");
kernel_cmdline_param_kv!(MITIGATIONS_PARAM, mitigations, "");
kernel_cmdline_param_kv!(HIGHRES_PARAM, highres, "");

// TSC policy options are Linux/x86-specific. DragonOS currently consumes them
// for compatibility without changing TSC watchdog/reliability state.
#[cfg(target_arch = "x86_64")]
kernel_cmdline_param_kv!(TSC_PARAM, tsc, "");

// Linux handles printk.devkmsg as a kernel printk setting. DragonOS has no
// equivalent /dev/kmsg write policy control yet, but explicit registration
// documents that this is a kernel-recognized dotted parameter.
kernel_cmdline_param_kv!(PRINTK_DEVKMSG_PARAM, "printk.devkmsg", "");
