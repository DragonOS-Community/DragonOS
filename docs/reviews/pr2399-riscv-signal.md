# PR #2399: RISC-V signal and fault review

## Reference and baseline

The target is Linux 6.6.139, specifically `arch/riscv/kernel/{traps,signal}.c`,
`arch/riscv/mm/fault.c`, `include/uapi/asm-generic/signal.h`, and
`kernel/signal.c` (`get_signal`, `signal_delivered`, `__save_altstack`).

The three original PR commits were rebased onto origin/master
`7f9bfaaaff5cd7f1ddd5f84f83d4e6a27af26999`. The rebased, unmodified PR failed
`make ARCH=riscv64 ROOTFS_MANIFEST=default NPROCS=4 kernel`: its fault message
omitted the new creator/data accounts, and its retry waiter assumed an untyped
errno. Consequently, guest reproduction on that baseline was unavailable;
pre-fix evidence comes from the compile errors and the fault/delivery call paths.

## Review decisions

| Report | Finding and change |
| --- | --- |
| File-mapping errors become SIGSEGV | Confirmed. Preserve terminal fault classification and deliver SIGBUS/BUS_ADRERR for SIGBUS/HWPOISON reasons. Handle read/instruction faults through the same MM boundary, with the corresponding access flags. |
| Access/misalignment traps should use badaddr | Not applicable to the target ABI. Linux 6.6.139 RISC-V `DO_ERROR_INFO` and misalignment handlers explicitly use `regs->epc`. Retain that behavior. Page faults use the exact `stval` address. |
| Missing SA_RESETHAND | Confirmed. Select and reset the shared disposition under one sighand write lock, retaining the snapshot for this delivery. Both implemented architectures use the shared operation. |
| Nested uc_stack needs SS_ONSTACK | Not applicable to Linux 6.6.139. `__save_altstack` copies raw `sas_ss_flags`; only a `sigaltstack` query computes SS_ONSTACK from the interrupted SP. Retain raw flags and test both interfaces. |

## Additional root causes and reviewed design

- RV64 `rt_sigaction` has no restorer field: its mask begins at byte 16 and its
  size is 24 bytes. Encode the wire flags as an unsigned machine word, with
  explicit internal conversions and fully initialized output (no padded flags
  bytes or reads of the output buffer). Correct the layout and provide an eager, independent RX
  sigreturn mapping in each new user mm. Fork inherits the mapping, exec builds
  a new one, and teardown clears it. Initialize through a pinned physical alias
  under the unpublished mm's write lock, then use the existing cross-hart icache
  protocol. There is no executable-stack trampoline or fake vDSO metadata.
- Commit the blocked mask, RESTORE_SIG_MASK removal, and SS_AUTODISARM reset only
  after writing the signal frame successfully. The one-shot disposition is
  consumed earlier, matching Linux `get_signal` even if frame setup fails.
- Integrate current master's typed admission failures and retry waits. Release
  MM/VMA guards before waits, reclaim, and OOM. Local charge failures use the
  existing memcg recovery and never invoke the global killer. Physical OOM uses
  existing global recovery/OOM policy. Interrupted charge recovery uses the
  shared accounting helper and does not fabricate an OOM/minor fault event.
- Guest validation also reproduced an eager-mapping protection bug: an access
  to a new PROT_NONE anonymous mapping succeeded instead of raising SIGSEGV.
  The non-demand-paging flag builder retained a valid readable default PTE.
  Use the existing RISC-V protection map for eager mappings too; retain Linux's
  invalid-but-resident G/R PROT_NONE leaf encoding. Do not enable unrelated lazy
  allocation policies. Test permission restoration, fork inheritance, retained
  contents and read-only write faults as well as the initial access denial.
  Also retain a read-only PTE when mprotect restores private write permissions:
  eager allocation does not imply exclusive ownership after fork. User store
  faults and the existing copyout prefault path must establish COW first.
- Classify MAPERR/ACCERR while holding the MM lock rather than looking up a VMA
  again after an error, which would race unmap/protection changes.

The implementation and final changes received independent subagent reviews of
Linux semantics, ABI layout, locking, accounting, fork/exec lifetime, FP state,
frame-copy initialization, error paths, and restart handling. The review found
and corrected the interrupted-charge accounting issue before submission.

## Regression coverage

`normal/riscv_signal_semantics` covers ordinary libc delivery/return, RV raw
sigaction layout and mask roundtrip, reset disposition before entering a handler,
second-delivery default termination, nested alternate-stack flags versus query
flags, populated and unpopulated truncated-file reads/writes, and exact
SIGSEGV/SIGBUS address/code. Fault addresses include an offset within a page.
Each case is isolated in a child with bounded parent and child timeouts.

## Reproduction evidence

- In QEMU, before the protection-map correction, the PROT_NONE read test exited
  at checkpoint 51 (the forbidden access succeeded). The other ten initial
  signal/fault cases passed after creating `/tmp` on the minimal test disk.
- With the protection map fixed but before the private-remap COW correction,
  the strengthened 14-case suite passed 13 cases and failed fork isolation at
  checkpoint 89: the parent's private byte changed after the child's store.
  This confirms the ownership bug independently of signal delivery.

## Final validation

- `make ARCH=riscv64 ROOTFS_MANIFEST=default NPROCS=4 kernel`: passed.
- `make -C kernel ARCH=x86_64 NPROCS=4 check`: passed, including the shared
  sighand and raw sigaction changes.
- Targeted dunitest build and host Linux run: 13 passed, RV64-only raw ABI case
  skipped on x86_64.
- Static RV64 dunitest cross-build: passed.
- QEMU 8.2.2 / OpenSBI, current RISC-V kernel, static BusyBox 1.35.0 and test
  binary: external BusyBox fork/exec smoke passed; all 14 regression cases
  passed, including both user-store and kernel-copyout fork isolation.
- Modified Rust files passed rustfmt check; smoke script passed `bash -n`;
  `git diff --check` passed.

The local minimal FAT test disk needed `/tmp` created for mkstemp. Local guest
binaries used the available static GNU RISC-V toolchain; the normal smoke
script's musl build, U-Boot and EDK2 paths remain for CI verification.
