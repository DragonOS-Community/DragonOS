---
name: dragonos-gvisor-test-analysis
description: Analyzes DragonOS gVisor test failures by comparing with Linux/gvisor reference implementations. Outputs structured fix documents in table format (3+ failures) or detailed format (1-2 failures) with code snippets. Use when user mentions gVisor test failures, specific test cases, or asks for bug analysis/fix plans.
version: 0.1.0
allowed-tools: Read, Grep, Glob, Bash
---

# DragonOS gVisor Test Failure Analyzer

## Purpose

Analyzes DragonOS test failures from gVisor test suite by referencing Linux kernel and gVisor implementations. Outputs fix documents that identify root causes and provide actionable fix plans.

## Output Format Selection

- **1-2 test failures** → Detailed format with code snippets and line-by-line comparison
- **3+ test failures** → Table format with root cause grouping for quick overview

## Reference Paths

```
gVisor tests:  ../gvisor/test/syscalls/linux/
Linux kernel:  ../linux/kernel/
DragonOS:      kernel/src/
```

## Workflow

### Step 1: Parse Test Failures

Extract from user input:
- All failed test names (format: `TestSuite.TestCase`)
- GTEST output messages like:
  ```
  [  RUN     ] WaitTest.Wait4Rusage
  [  FAILED  ] WaitTest.Wait4Rusage (0 ms)
  ```
- Error patterns or panic messages
- Stack traces if present

### Step 2: Choose Format

Count unique test failures:
- ≤ 2: Use **Single Test Format** (see FORMAT.md)
- ≥ 3: Use **Batch Format** (see FORMAT.md)

### Step 3: Locate Test Code

Find gVisor test implementation:
```
Use Glob to find: ../gvisor/test/syscalls/linux/*<syscall>*.cc
Use Grep to find: TEST.*<test_name>
```

### Step 4: Trace System Call Path

Map the call chain:
```
Test → Syscall → DragonOS Implementation → Bug
```

Find DragonOS implementation:
```
Grep for: fn sys_<syscall_name> or syscall!(<syscall_name>)
```

### Step 5: Compare with Linux Reference

Find Linux reference implementation:
```
Grep in ../linux: SYSCALL_DEFINE.*<syscall_name>
```

### Step 6: Generate Fix Document

Follow the appropriate format in `references/FORMAT.md`:
- **Single Test Format** (1-2 failures): Detailed analysis with code snippets
- **Batch Format** (3+ failures): Table format grouped by root cause

## Examples

**Input (single test)**:
```
分析一下 WaitTest.Wait4Rusage 为什么失败
```

**Output**: Detailed format with code from `exit.c:wait4` and `process/exit.rs` (see FORMAT.md for structure)

---

**Input (multiple tests)**:
```
wait_test 失败了20个测试用例，包括 Wait4Rusage, WaitidRusage, WaitAnyChildTest.*,请对比 Linux 和 gvisor 的实现，分析失败原因并给出修复方案
```

**Output**: Table format with ~20 rows grouped by 4-5 root causes (see FORMAT.md for structure)

## Notes

- Always cite `file:line` for code references
- Code snippets should be minimal (5-10 lines max)
- For batch format, group tests by root cause first, not by test suite
- Cascading failures: note which test is the root cause
- If Linux/gvisor differ, explain your choice and rationale
- Consider DragonOS architecture constraints when proposing fixes
