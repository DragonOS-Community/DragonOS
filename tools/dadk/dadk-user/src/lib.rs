//! # DADK - DragonOS Application Development Kit
//! # DragonOS 应用开发工具
//!
//! ## 简介
//!
//! DADK是一个用于开发DragonOS应用的工具包，设计目的是为了让开发者能够更加方便的开发DragonOS应用。
//!
//! ### DADK做什么？
//!
//! - 自动配置libc等编译用户程序所需的环境
//! - 自动处理软件库的依赖关系
//! - 自动处理软件库的编译
//! - 一键将软件库安装到DragonOS系统中
//!
//! ### DADK不做什么？
//!
//! - DADK不会帮助开发者编写代码
//! - DADK不提供任何开发DragonOS应用所需的API。这部分工作由libc等库来完成
//!
//! ## License
//!
//! DADK is licensed under the [GPLv2 License](https://www.gnu.org/licenses/old-licenses/gpl-2.0.html).
//!
//! ## 快速开始
//!
//! ### 安装DADK
//!
//! DADK是一个Rust程序，您可以通过Cargo来安装DADK。
//!
//! ```shell
//! # 从GitHub安装最新版
//! cargo install --git https://github.com/DragonOS-Community/DADK.git
//!
//! # 从crates.io下载
//! cargo install dadk
//!
//! ```
//!
//! ## DADK的工作原理
//!
//! DADK使用(任务名，任务版本）来标识每个构建目标。当使用DADK构建DragonOS应用时，DADK会根据用户的配置文件，自动完成以下工作：
//!
//! - 解析配置文件，生成DADK任务列表
//! - 根据DADK任务列表，进行拓扑排序。这一步会自动处理软件库的依赖关系。
//! - 收集环境变量信息，并根据DADK任务列表，设置全局环境变量、任务环境变量。
//! - 根据拓扑排序后的DADK任务列表，自动执行任务。
//!
//! ### DADK与环境变量
//!
//! 环境变量的设置是DADK能正常工作的关键因素之一，您可以在您的编译脚本中，通过引用环境变量，来获得其他软件库的编译信息。
//! 这是使得您的应用能够自动依赖其他软件库的关键一步。
//!
//! 只要您的编译脚本能够正确地引用环境变量，DADK就能够自动处理软件库的依赖关系。
//!
//! DADK会设置以下全局环境变量：
//!
//! - `DADK_CACHE_ROOT`：DADK的缓存根目录。您可以在编译脚本中，通过引用该环境变量，来获得DADK的缓存根目录。
//! - `DADK_BUILD_CACHE_DIR_任务名_任务版本`：DADK的任务构建结果缓存目录。当您要引用其他软件库的构建结果时，可以通过该环境变量来获得。
//! 同时，您也要在构建您的app时，把构建结果放到您的软件库的构建结果缓存目录（通过对应的环境变量获得）中。
//! - `DADK_SOURCE_CACHE_DIR_任务名_任务版本`：DADK的某个任务的源码目录。当您要引用其他软件库的源码目录时，可以通过该环境变量来获得。
//!
//! 同时，DADK会为每个任务设置其自身在配置文件中指定的环境变量。
//!
//! #### 全局环境变量命名格式
//!
//! 全局环境变量中的任务名和任务版本，都会被转换为大写字母，并对特殊字符进行替换。替换表如下：
//!
//! | 原字符 | 替换字符 |
//! | ------ | -------- |
//! | `.`    | `_`      |
//! | `-`    | `_`      |
//! | `\t`   | `_`      |
//! | 空格   | `_`      |
//! | `+`    | `_`      |
//! | `*`    | `_`      |
//!
//! **举例**：对于任务`libc-0.1.0`，其构建结果的全局环境变量名为`DADK_BUILD_CACHE_DIR_LIBC_0_1_0`。
//!
//!
//! ## TODO
//!
//! - 支持从在线归档文件下载源码、构建好的软件库
//! - 支持自动更新
//! - 完善clean命令的逻辑

#![feature(io_error_more)]

#[macro_use]
extern crate lazy_static;
pub extern crate clap;
extern crate log;
extern crate serde;
extern crate serde_json;

#[cfg(test)]
extern crate test_base;

use std::{path::PathBuf, process::exit, sync::Arc};

use context::DadkUserExecuteContext;
use log::info;
use parser::task::DADKTask;

use crate::scheduler::Scheduler;

pub mod context;
pub mod executor;
pub mod parser;
mod scheduler;
mod utils;

pub fn dadk_user_main(context: DadkUserExecuteContext) {
    let context = Arc::new(context);
    context.init(context.clone());
    // DragonOS sysroot在主机上的路径

    info!(
        "DragonOS sysroot dir: {}",
        context
            .sysroot_dir()
            .map_or_else(|| "None".to_string(), |d| d.display().to_string())
    );
    info!(
        "Config dir: {}",
        context
            .config_dir()
            .map_or_else(|| "None".to_string(), |d| d.display().to_string())
    );
    info!("Action: {:?}", context.action());
    info!(
        "Thread num: {}",
        context.thread_num().map_or_else(|| 0, |t| t)
    );

    let mut parser = parser::Parser::new(
        context.config_dir().unwrap().clone(),
        context.app_blocklist().clone(),
    );
    let r = parser.parse();
    if r.is_err() {
        exit(1);
    }
    let tasks: Vec<(PathBuf, DADKTask)> = r.unwrap();
    // info!("Parsed tasks: {:?}", tasks);

    let scheduler = Scheduler::new(
        context.clone(),
        context.sysroot_dir().cloned().unwrap(),
        *context.action(),
        tasks,
    );
    if scheduler.is_err() {
        exit(1);
    }

    let r = scheduler.unwrap().run();
    if r.is_err() {
        exit(1);
    }
}
