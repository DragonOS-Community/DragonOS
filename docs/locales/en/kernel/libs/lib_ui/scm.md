:::{note}
**AI Translation Notice**

This document was automatically translated by `Qwen/Qwen3-8B` model, for reference only.

- Source document: kernel/libs/lib_ui/scm.md

- Translation time: 2025-05-19 01:41:31

- Translation model: `Qwen/Qwen3-8B`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

# Screen Manager (SCM)

:::{note}
Author: Zhou Hanjie <2625553453@qq.com>
:::
&emsp;&emsp;The Screen Manager is used to control all UI frameworks. All frameworks must be registered with the Screen Manager before they can be used. Then, SCM controls which UI framework is currently in use.

## traits

### ScmUiFramework
&emsp;&emsp;Each UI framework that is to be registered with SCM must implement the methods defined in this trait, as follows:
```rust
pub trait ScmUiFramework: Sync + Send + Debug {
    // 安装ui框架的回调函数
    fn install(&self) -> Result<i32, SystemError> {
        return Err(SystemError::ENOSYS);
    }
    // 卸载ui框架的回调函数
    fn uninstall(&self) -> Result<i32, SystemError> {
        return Err(SystemError::ENOSYS);
    }
    // 启用ui框架的回调函数
    fn enable(&self) -> Result<i32, SystemError> {
        return Err(SystemError::ENOSYS);
    }
    // 禁用ui框架的回调函数
    fn disable(&self) -> Result<i32, SystemError> {
        return Err(SystemError::ENOSYS);
    }
    // 改变ui框架的帧缓冲区的回调函数
    fn change(&self, _buf: ScmBufferInfo) -> Result<i32, SystemError> {
        return Err(SystemError::ENOSYS);
    }
    /// @brief 获取ScmUiFramework的元数据
    /// @return 成功：Ok(ScmUiFramework的元数据)
    ///         失败：Err(错误码)
    fn metadata(&self) -> Result<ScmUiFrameworkMetadata, SystemError> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(SystemError::ENOSYS);
    }
}
```
## Main APIs
### scm_init() - Initialize the screen management module
#### Prototype
```rust
pub extern "C" fn scm_init()
```
#### Description
&emsp;&emsp;scm_init() is mainly used to initialize some global variables used by SCM, such as the flag indicating whether double buffering is used, and some global variables used by textui when it is not initialized.

### scm_reinit() - Reinitialize the screen management module after the memory management unit is initialized
#### Prototype
```rust
pub extern "C" fn scm_reinit() -> i32
```
#### Description
&emsp;&emsp;scm_reinit() is used to reprocess the frame buffer issues after the memory management unit has been initialized.

### scm_enable_double_buffer() - Enable double buffering
#### Prototype
```rust
pub extern "C" fn scm_enable_double_buffer() -> i32
```
#### Description
&emsp;&emsp;scm_enable_double_buffer() is used to enable double buffering for outputting information to the window. After enabling, the information output to the window is temporarily stored in a buffer, and then this buffer's content is output to the window's frame buffer at regular intervals, rendering it to the window.

### scm_framework_enable() - Enable a specific UI framework and render its frame buffer to the screen
#### Prototype
```rust
pub fn scm_framework_enable(framework: Arc<dyn ScmUiFramework>) -> Result<i32, SystemError>
```
#### Description
&emsp;&emsp;scm_framework_enable is used to enable a specific UI framework and render its frame buffer to the screen.

### scm_register() - Register a UI framework with the screen manager
#### Prototype
```rust
pub fn scm_register(framework: Arc<dyn ScmUiFramework>) -> Result<i32, SystemError> 
```
#### Description
&emsp;&emsp;scm_register is used to register a UI framework with SCM. It mainly calls the callback functions of the UI framework to install and activate it.
