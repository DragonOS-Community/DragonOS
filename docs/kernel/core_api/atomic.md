# Atomic Variables

## Introduction

DragonOS implements atomic variables of type `atomic_t`. Atomic variables are implemented using architecture-specific atomic operation instructions. The specific implementation is located in `kernel/common/atomic.h`.

## API

 Note that all the following APIs are atomic operations.

### `inline void atomic_add(atomic_t *ato, long val)`

#### Description

 Atomically adds a specified value to the atomic variable.

#### Parameters

**ato**

 The atomic variable object.

**val**

 The value to be added to the variable.

### `inline void atomic_sub(atomic_t *ato, long val)`

#### Description

 Atomically subtracts a specified value from the atomic variable.

#### Parameters

**ato**

 The atomic variable object.

**val**

 The value to be subtracted from the variable.

### `void atomic_inc(atomic_t *ato)`

#### Description

 Atomically increments the atomic variable by 1.

#### Parameters

**ato**

 The atomic variable object.

### `void atomic_dec(atomic_t *ato)`

#### Description

 Atomically decrements the atomic variable by 1.

#### Parameters

**ato**

 The atomic variable object.

### `inline void atomic_set_mask(atomic_t *ato, long mask)`

#### Description

 Performs a bitwise OR operation between the atomic variable and the mask variable.

#### Parameters

**ato**

 The atomic variable object.

**mask**

 The variable used for the OR operation with the atomic variable.

### `inline void atomic_clear_mask(atomic_t *ato, long mask)`

#### Description

 Performs a bitwise AND operation between the atomic variable and the mask variable.

#### Parameters

**ato**

 The atomic variable object.

**mask**

 The variable used for the AND operation with the atomic variable.
