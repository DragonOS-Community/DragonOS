# dunitest userspace test framework

dunitest is DragonOS's userspace unit-test framework. It runs Google Test C++ cases and writes structured reports.

## Status

- The runner discovers and executes binaries under `bin/`
- Default timeout is 60 seconds and can be overridden with `--timeout-sec`
- Filter order: white list -> block list -> `--pattern`
- Summary counts use gtest case counts (not the number of test programs)
- The runner returns non-zero on failure or timeout

## Layout

```text
user/apps/tests/dunitest/
├── runner/                 # Rust test runner
├── suites/                 # Test sources (one directory per suite)
├── bin/                    # Build outputs (auto-discovered)
├── whitelist.txt           # Default white list
├── scripts/run_tests.sh    # In-guest entry
└── Makefile
```

## Rules

1. Sources live in `suites/<suite>/*.cc`
2. Binaries are `bin/<suite>/<case>_test`
3. Runner case names are `<suite>/<case>` (`_test` suffix stripped)

Examples:

- Binary: `bin/demo/gtest_demo_test`
- Case name: `demo/gtest_demo`
- White-list entry: `demo/gtest_demo`

## Adding a case

### Prefer the `normal` suite

- Put general functional tests in `suites/normal/`
- Example: `suites/normal/capability.cc`
- White-list entry: `normal/capability`

### 1. Add gtest source

```text
suites/normal/capability.cc
```

### 2. Register the suite in the Makefile

Edit `SUITES` in `user/apps/tests/dunitest/Makefile`:

```makefile
# Add new suite directories here
SUITES = demo normal
```

### 3. Build and run (parallel-safe)

From the repo root:

```bash
make test-dunit-local
```

Or in the dunitest directory:

```bash
make run -j$(nproc)
```

Example build log:

```text
compile: suites/normal/capability.cc -> bin/normal/capability_test
```

### 4. Add a white-list entry

```text
demo/gtest_demo
normal/capability
```

## Runner flags

```text
dunitest-runner [OPTIONS]

  --bin-dir <PATH>       test binary directory (default: bin)
  --timeout-sec <SEC>    per-case timeout (default: 60)
  --whitelist <PATH>     white list path (default: whitelist.txt)
  --blocklist <PATH>     block list path (default: blocklist.txt)
  --results-dir <PATH>   report directory (default: results)
  --list                 list cases only
  --verbose              verbose output
  --pattern <PATTERN>    substring filter (repeatable)
```

## Reports

After a run, `results/` contains:

- `test_report.txt`: text report
- `summary.json`: JSON summary
- `failed_cases.txt`: failed/timed-out cases
- `<case>.log`: per-case logs

Summary rules:

- totals/passed/failed/skipped use gtest case counts
- if a program emits no gtest stats, the runner falls back to program-level counts

## Install

Run `make install` in `user/apps/tests/dunitest/`.
