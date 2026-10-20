# 应用程序黑名单

## 功能概述

DADK 支持应用程序黑名单功能，允许用户指定不希望编译和安装的应用程序。当黑名单中的应用程序被检测到时，DADK 会自动跳过这些应用程序的构建和安装过程。

## 配置文件

### 1. 创建黑名单配置文件

在项目根目录的 `config/` 目录下创建 `app-blocklist.toml` 文件：

```toml
# 应用程序黑名单配置文件
# 路径: config/app-blocklist.toml

# 是否启用严格模式（可选）
# strict = false  # 非严格模式：只警告不跳过
# strict = true   # 严格模式：跳过并警告（默认）
strict = true

# 是否在日志中显示被跳过的应用（可选）
log_skipped = true

# 被屏蔽的应用程序列表

[[blocked_apps]]
name = "openssl@1.1.1"
reason = "存在安全漏洞，请使用3.x版本"


[[blocked_apps]]
name = "deprecated-old-app"
```

### 2. 配置文件路径（可选）

如果需要自定义黑名单配置文件的路径，可以在 `dadk-manifest.toml` 中指定：

```toml
[metadata]
arch = "x86_64"
rootfs-config = "config/rootfs.toml"
boot-config = "config/boot.toml"
hypervisor-config = "config/hypervisor.toml"
sysroot-dir = "bin/sysroot"
cache-root-dir = "bin/dadk_cache"
app-blocklist-config = "config/my-blocklist.toml"  # 自定义路径
```

## 使用示例

### 基本用法

创建黑名单配置文件 `config/app-blocklist.toml`：

```toml
strict = true
log_skipped = true

[[blocked_apps]]
name = "busybox"
reason = "Skipping busybox and test applications"

[[blocked_apps]]
name = "test-app"
```

### 输出示例

当黑名单中有应用程序时，DADK 会输出类似以下日志：

```
[INFO] Found 2 applications in blocklist
[WARN] Skipping blocked application 'busybox' (config: config/busybox.toml)
[WARN] Skipping blocked application 'test-app' (config: config/test-app.toml)  
[INFO] Skipped 2 blocked applications: busybox, test-app
[DEBUG] Blocklist reasons:
busybox: Skipping busybox and test applications
```

## 高级功能

### 1. 模式匹配

黑名单支持通配符模式匹配：

```toml
strict = true
log_skipped = true

[[blocked_apps]]
name = "test-*"
reason = "所有测试应用"

[[blocked_apps]]
name = "deprecated-*"
reason = "已弃用的应用"

[[blocked_apps]]
name = "nginx-*"
reason = "所有nginx相关应用"
```

### 2. 版本匹配

支持指定特定版本的应用。匹配优先级为：精确匹配 > 版本匹配 > 模式匹配：

```toml
strict = true
log_skipped = true

[[blocked_apps]]
name = "openssl@1.1.1"
reason = "存在安全漏洞的版本"

[[blocked_apps]]
name = "libfoo@2.*"
reason = "不支持的2.x版本"
```

### 3. 非严格模式

如果只想记录警告但不跳过应用程序的构建和安装，可以设置 `strict = false`。注意：即使在非严格模式下，应用仍然会被检测为"被屏蔽"，只是不会实际跳过构建：

```toml
strict = false
log_skipped = true

[[blocked_apps]]
name = "deprecated-app"
reason = "应该被替换的旧应用"
```

### 4. 静默模式

如果不想显示被跳过的应用程序，可以设置 `log_skipped = false`：

```toml
strict = true
log_skipped = false

[[blocked_apps]]
name = "internal-tools"
reason = "内部工具，不显示在日志中"
```

## 注意事项

1. **依赖关系**：如果其他应用程序依赖被屏蔽的应用程序，构建过程可能会失败。在使用黑名单功能前，请仔细检查应用程序间的依赖关系，确保没有其他应用依赖被屏蔽的应用。

2. **配置文件格式**：确保 `app-blocklist.toml` 是有效的 TOML 格式。必须使用数组表格格式 `[[blocked_apps]]`，每个应用程序一个条目，这样可以为每个应用独立设置reason。

3. **文件路径**：黑名单配置文件路径可以是相对路径或绝对路径。相对路径相对于工作目录。完整的模板配置文件请参考：[app-blocklist.toml](https://github.com/DragonOS-Community/DADK/blob/main/dadk-config/templates/config/app-blocklist.toml)

4. **模板文件**：DADK提供了完整的模板配置文件，位于 `dadk-config/templates/config/app-blocklist.toml`，其中包含了详细的注释和各种使用场景的示例。

4. **模板文件**：DADK提供了完整的模板配置文件，位于 `dadk-config/templates/config/app-blocklist.toml`，其中包含了详细的注释和各种使用场景的示例。

5. **字段顺序**：建议将 `strict` 和 `log_skipped` 等全局配置放在文件开头，`[[blocked_apps]]` 数组放在后面。

## 故障排除

### 问题：黑名单配置文件加载失败

如果看到以下警告：
```
[WARN] Failed to load app blocklist config: ..., using empty config
```

请检查：
- 配置文件路径是否正确
- 文件是否存在
- TOML 格式是否正确

### 问题：应用程序未被跳过

请检查：
- 应用程序名称是否准确匹配（区分大小写）
- `strict` 是否设置为 `true`
- 配置文件是否被正确加载

### 问题：构建失败，提示依赖缺失

如果其他应用程序依赖被屏蔽的应用程序，需要：
- 从黑名单中移除该应用
- 或修改依赖应用程序的配置，移除对被屏蔽应用的依赖
