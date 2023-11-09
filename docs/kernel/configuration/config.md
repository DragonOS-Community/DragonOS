# 内核编译配置说明

## 原理

&emsp;&emsp;在内核目录下，用kernel.config来设置内核编译配置信息，以类似解析toml文件的方式去解析该文件，然后接着去解析各模块下的d.config以获取feature的启用情况

## 示例

**kernel.config**

```toml
[[module.include]]
name = "init"
path = "src/init/"
enable = "y"
description = ""

[[module.include]]
name = "mm"
path = "src/mm/"
enable = "y"
description = ""
```


- **[[module.include]]:** 将模块加入到include列表中
- **name:** 模块名
- **path:** 模块路径，存放着d.config
- **enable:**
  - **y:** 启用，解析模块下的d.config
  - **n:** 不启用，不解析
- **description:** 模块的描述信息


**src/mm/d.config**

```toml
[module]
name = "mm"
description = ""

[[module.include]]
name = "allocator"
path = "src/mm/allocator/"
enable = "y"
description = ""

[[module.features]]
name = "mm_debug"
enable = "y"
description = ""
```


- **\[module\]:** 当前模块
  - **name:** 当前模块名称
  - **description:** 模块的描述信息
- **[[module.include]]:** 当前模块下所包含的模块，与kernel.config下的相同
- **[[module.features]]:** 当前模块下的feature
  - **name:** feature名
  - **enable:** 是否开启
    - **y:** 开启
    - **n:** 不开启
  - **description:** feature的描述信息


*以下是其它模块下的d.config：*

**src/mm/allocator/d.config**

```toml
[module]
name = "allocator"
description = ""

[[module.features]]
name = "allocator_debug"
enable = "y"
description = ""
```

**src/init/d.config**

```toml
[module]
name = "init"
description = ""

[[module.features]]
name = "init_debug"
enable = "y"
description = ""
```


上面所有已开启模块的d.config中的feature，会最终生成到内核目录下的D.config文件，即D.config是最终内核编译的配置，如下：


**D.config**

```
init_debug = y
allocator_debug = y
mm_debug = y
```