:::{note}
**AI Translation Notice**

This document was automatically translated by `Qwen/Qwen3-8B` model, for reference only.

- Source document: kernel/core_api/atomic.md

- Translation time: 2025-05-19 01:41:25

- Translation model: `Qwen/Qwen3-8B`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

# Atomic Variables

## Introduction

&emsp;&emsp;DragonOS implements atomic variables of type `atomic_t`. Atomic variables are implemented using architecture-specific atomic operation instructions. The specific implementation is located in `kernel/common/atomic.h`.

## API

&emsp;&emsp; Note that all the following APIs are atomic operations.

### `inline void atomic_add(atomic_t *ato, long val)`

#### Description

&emsp;&emsp; Atomically adds a specified value to the atomic variable.

#### Parameters

**ato**

&emsp;&emsp; The atomic variable object.

**val**

&emsp;&emsp; The value to be added to the variable.

### `inline void atomic_sub(atomic_t *ato, long val)`

#### Description

&emsp;&emsp; Atomically subtracts a specified value from the atomic variable.

#### Parameters

**ato**

&emsp;&emsp; The atomic variable object.

**val**

&emsp;&emsp; The value to be subtracted from the variable.

### `void atomic_inc(atomic_t *ato)`

#### Description

&emsp;&emsp; Atomically increments the atomic variable by 1.

#### Parameters

**ato**

&emsp;&emsp; The atomic variable object.

### `void atomic_dec(atomic_t *ato)`

#### Description

&emsp;&emsp; Atomically decrements the atomic variable by 1.

#### Parameters

**ato**

&emsp;&emsp; The atomic variable object.

### `inline void atomic_set_mask(atomic_t *ato, long mask)`

#### Description

&emsp;&emsp; Performs a bitwise OR operation between the atomic variable and the mask variable.

#### Parameters

**ato**

&emsp;&emsp; The atomic variable object.

**mask**

&emsp;&emsp; The variable used for the OR operation with the atomic variable.

### `inline void atomic_clear_mask(atomic_t *ato, long mask)`

#### Description

&emsp;&emsp; Performs a bitwise AND operation between the atomic variable and the mask variable.

#### Parameters

**ato**

&emsp;&emsp; The atomic variable object.

**mask**

&emsp;&emsp; The variable used for the AND operation with the atomic variable.
