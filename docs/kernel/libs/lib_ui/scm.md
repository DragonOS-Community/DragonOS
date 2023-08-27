# 屏幕管理器（SCM）

:::{note}
作者: 周瀚杰 <2625553453@qq.com>
:::
&emsp;&emsp;屏幕管理器用来管理控制所有ui框架，所有框架都必须先在屏幕管理器中注册才可使用，然后scm控制当前是哪个ui框架在使用

## traits

### ScmUiFramework
&emsp;&emsp;每个要注册到scm中的ui框架都必须实现这个trait中的方法，具体定义如下：
```rust
pub trait ScmUiFramework: Sync + Send + Debug {
    // 安装ui框架的回调函数
    fn install(&self) -> Result<i32, SystemError> {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    // 卸载ui框架的回调函数
    fn uninstall(&self) -> Result<i32, SystemError> {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    // 启用ui框架的回调函数
    fn enable(&self) -> Result<i32, SystemError> {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    // 禁用ui框架的回调函数
    fn disable(&self) -> Result<i32, SystemError> {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    // 改变ui框架的帧缓冲区的回调函数
    fn change(&self, _buf: ScmBufferInfo) -> Result<i32, SystemError> {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    /// @brief 获取ScmUiFramework的元数据
    /// @return 成功：Ok(ScmUiFramework的元数据)
    ///         失败：Err(错误码)
    fn metadata(&self) -> Result<ScmUiFrameworkMetadata, SystemError> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
}
```
## 主要API
### scm_init() -初始化屏幕管理模块
#### 原型
```rust
pub extern "C" fn scm_init()
```
#### 说明
&emsp;&emsp;scm_init()主要是初始化一些scm中使用的全局变量，例如是否使用双缓冲区标志位，textui未初始化时使用的一些全局变量

### scm_reinit() -当内存管理单元被初始化之后，重新初始化屏幕管理模块
#### 原型
```rust
pub extern "C" fn scm_reinit() -> i32
```
#### 说明
&emsp;&emsp;scm_reinit()用于当内存管理单元被初始化之后，重新处理帧缓冲区问题

### scm_enable_double_buffer() -允许双缓冲区
#### 原型
```rust
pub extern "C" fn scm_enable_double_buffer() -> i32
```
#### 说明
&emsp;&emsp;scm_enable_double_buffer()用于启动双缓冲来往窗口输出打印信息。启用后，往窗口输出的信息会暂时放在一个缓冲区中，然后每次按一定时间将该缓冲区的信息输出到窗口帧缓冲区中，渲染显示到窗口上。

### scm_framework_enable（） -启用某个ui框架，将它的帧缓冲区渲染到屏幕上
#### 原型
```rust
pub fn scm_framework_enable(framework: Arc<dyn ScmUiFramework>) -> Result<i32, SystemError>
```
#### 说明
&emsp;&emsp;scm_framework_enable用于启用某个ui框架，将它的帧缓冲区渲染到屏幕上


### scm_register（） -向屏幕管理器注册UI框架
#### 原型
```rust
pub fn scm_register(framework: Arc<dyn ScmUiFramework>) -> Result<i32, SystemError> 
```
#### 说明
&emsp;&emsp;scm_register用于将ui框架注册到scm中，主要是调用ui框架的回调函数以安装ui框架，并将其激活
