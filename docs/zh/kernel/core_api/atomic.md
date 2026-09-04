# 原子变量

## 简介

DragonOS实现了原子变量，类型为atomic_t. 原子变量是基于具体体系结构的原子操作指令实现的。具体实现在`kernel/common/atomic.h`中。

## API

 请注意，以下API均为原子操作。

### `inline void atomic_add(atomic_t *ato, long val)`

#### 描述

原子变量增加指定值

#### 参数

**ato**

原子变量对象

**val**

变量要增加的值

### `inline void atomic_sub(atomic_t *ato, long val)`

#### 描述

原子变量减去指定值

#### 参数

**ato**

原子变量对象

**val**

变量要被减去的值

### `void atomic_inc(atomic_t *ato)`

#### 描述

原子变量自增1

#### 参数

**ato**

原子变量对象


### `void atomic_dec(atomic_t *ato)`

#### 描述

原子变量自减1

#### 参数

**ato**

原子变量对象

### `inline void atomic_set_mask(atomic_t *ato, long mask)`

#### 描述

将原子变量的值与mask变量进行or运算

#### 参数

**ato**

原子变量对象

**mask**

与原子变量进行or运算的变量

### `inline void atomic_clear_mask(atomic_t *ato, long mask)`

#### 描述

将原子变量的值与mask变量进行and运算

#### 参数

**ato**

原子变量对象

**mask**

与原子变量进行and运算的变量

