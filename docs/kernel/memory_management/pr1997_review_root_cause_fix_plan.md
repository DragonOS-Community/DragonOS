# PR #1997 review root-cause fix plan

## Scope

This document records the root-cause analysis and implementation plan for the
unresolved review threads on PR #1997. The goal is to fix real regressions
without adding workaround behavior or weakening Linux compatibility.

## Evidence reviewed

- Linux 6.6.139 `arch/x86/kernel/signal_64.c` saves the current altstack into
  `ucontext.uc_stack` during signal-frame setup, and restores it from
  `rt_sigreturn`.
- Linux 6.6.139 `kernel/signal.c` calls `sas_ss_reset(current)` after a signal
  has been successfully delivered when `SS_AUTODISARM` is set.
- Linux 6.6.139 `do_sigaltstack()` accepts new `ss_flags` modes `0`,
  `SS_DISABLE`, and `SS_ONSTACK`, with `SS_AUTODISARM` treated as an extra flag.
- A host Linux probe confirmed that `sigaltstack()` accepts `SS_ONSTACK`,
  `SS_ONSTACK | SS_AUTODISARM`, and `SS_DISABLE | SS_AUTODISARM` as new
  `ss_flags`.
- DragonOS currently writes `UserUContext::uc_stack` as zero, does not restore
  it in `sys_rt_sigreturn`, and does not reset the task altstack after
  successful `SS_AUTODISARM` signal delivery.
- DragonOS `sys_sendto` currently allocates and copies `len` bytes from the user
  buffer before checking whether `fd` refers to a socket or whether the
  destination address is semantically valid for that socket.

## Review thread verdicts

### 1. `SS_AUTODISARM` bypasses altstack range detection

Verdict: real issue.

Root cause:

- `SS_AUTODISARM` needs the Linux save/reset/restore loop: save the current
  altstack state into `uc_stack`, reset the task altstack after successful signal
  delivery, and restore the saved altstack from `uc_stack` in `rt_sigreturn`.
- DragonOS currently has only the stack-selection behavior where
  `on_sig_stack()` returns false under `SS_AUTODISARM`; without reset and
  restore, nested `SA_ONSTACK` signal delivery can reuse the same altstack top
  and overwrite the active handler frame.

Fix plan:

1. Keep `X86SigStack::on_sig_stack(sp)` Linux-compatible: it should return false
   when `SS_AUTODISARM` is set.
2. Add a separate pure range helper, for example `contains_sp(sp)`, for cases
   that need raw altstack containment. Use the Linux down-growing boundary:
   `sp > ss_sp && sp - ss_sp <= ss_size`.
3. Save the original altstack triple into `UserUContext::uc_stack`:
   `ss_sp = stack.sp`, `ss_flags = stack.flags.bits()`, and
   `ss_size = stack.size`. Do not synthesize dynamic `SS_ONSTACK` in the signal
   frame; Linux uses dynamic `SS_ONSTACK` only for `sigaltstack(NULL, old_ss)`.
   Keep the kernel-side altstack size as `usize`, matching Linux `size_t`, so
   restoring `uc_stack.ss_size` cannot silently truncate large user values.
4. After the signal frame, fpstate, and siginfo have all been successfully
   written, reset the PCB altstack to disabled if the saved altstack had
   `SS_AUTODISARM`. Failed frame setup must not reset the altstack.
5. In `sys_rt_sigreturn`, copy only the needed `UserUContext` through protected
   user access, then restore the PCB altstack from `uc_stack`. User-copy failure
   should follow bad-frame behavior; validation errors such as `EINVAL`,
   `ENOMEM`, or `EPERM` should be ignored like Linux `restore_altstack()`.
6. Add x86 altstack overflow checking when the final signal frame address is
   supposed to live on the altstack. If the final frame would fall outside the
   registered altstack range, return a bad user address so setup fails with
   SIGSEGV instead of writing below the registered altstack.
7. Keep all user memory access outside `sig_altstack_mut()` guards. The only
   work under the irqsave lock should be reading or assigning the small PCB
   altstack fields.

Safety checks:

- No user copy while holding `sig_altstack` locks, so the change must not add a
  sleep-in-atomic or deadlock path.
- No new global locks or wait loops are introduced.
- The fix is root-cause aligned with Linux save/reset/restore semantics; it is
  not a workaround that merely suppresses nested delivery.

### 2. Reject `SS_ONSTACK` when installing a new sigaltstack

Verdict: not a real issue.

Reason:

- Linux 6.6.139 `do_sigaltstack()` explicitly accepts `SS_ONSTACK` as an
  allowed `ss_mode`, and a host Linux probe accepted `SS_ONSTACK` and
  `SS_ONSTACK | SS_AUTODISARM`.
- Changing DragonOS to reject these inputs would reduce Linux compatibility.

Action:

- Do not implement the requested rejection.
- Keep DragonOS validation aligned with Linux: valid base modes are `0`,
  `SS_DISABLE`, and `SS_ONSTACK`, with `SS_AUTODISARM` as an extra flag.

### 3. `sendto` allocates and copies before fd/socket validation

Verdict: real issue.

Root cause:

- `SysSendtoHandle::handle` performs `vec![0u8; len]` and copies the whole user
  buffer before checking `fd`, socket type, destination address length, and
  protocol-level destination-address validity.
- This can turn an otherwise cheap `EBADF` or socket semantic error into a large
  kernel allocation/copy attempt. It is a resource-consumption regression.

Fix plan:

1. Preserve Linux error ordering by keeping the lightweight payload range check
   before fd lookup, but do not allocate or copy the payload at that point.
2. Move destination-address range/copy/parse after fd/socket lookup, because
   Linux checks the socket fd before importing the destination address.
3. Extract a small helper, for example
   `prepare_send_common(fd, flags, addr, addrlen) -> PreparedSend`, that:
   - performs one fd-table lookup and obtains the same `Arc<File>`;
   - reads `O_NONBLOCK` and verifies `FileType::Socket` from that file;
   - obtains the socket inode `Arc` from that same file, then releases the
     fd-table guard;
   - validates and converts the optional destination address;
   - returns the socket inode `Arc`, computed `PMSG`, and optional endpoint.
4. Use this helper in `sendto` only for this PR. Do not reuse it in `sendmsg`
   here: `sendmsg` has separate Linux ordering around `msg_name`, `msg_namelen`,
   iovec import, and socket `send_msg()` fast paths, and should be fixed in a
   dedicated change.
5. After preflight, allocate and copy the send payload, then send through the
   prepared socket reference.

Safety checks:

- The fd-table read lock must not be held while copying user payload or while
  sending.
- No socket internal lock should be held across user payload copy.
- The returned socket inode `Arc` keeps the underlying socket alive if another
  thread closes the fd after preflight.
- This is a semantic ordering fix, not a workaround cap on user length. Full
  Linux-style iterator sending would be a larger follow-up, especially because
  datagram sends cannot be naively split without changing message atomicity.

## Validation plan

1. Add or extend dunitest coverage for:
   - `SS_AUTODISARM` signal delivery and `sigreturn` restoration;
   - Linux-compatible acceptance of `SS_ONSTACK` and
     `SS_ONSTACK | SS_AUTODISARM`;
   - `sendto` on an invalid fd with a huge accessible payload returns `EBADF`
     without needing to allocate/copy the payload.
2. Run `make fmt`.
3. Run `make kernel`.
4. Run focused dunitest cases for signal and sendto/socket behavior.
5. Re-check PR CI after pushing.
