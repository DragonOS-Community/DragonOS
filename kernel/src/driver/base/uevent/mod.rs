use crate::driver::base::uevent::kobject_uevent::kobject_uevent_env;
use core::fmt::Write;
/*
Struct
    kset_uevent_ops

Function

    get_ktype
    kobject_name
    kset_get
    kset_put
    to_kset
*/
use crate::driver::base::kobject::KObject;
use crate::driver::net::Iface;
use crate::filesystem::sysfs::{Attribute, SysFSOpsSupport, SYSFS_ATTR_MODE_RW};
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use intertrait::cast::CastArc;
use system_error::SystemError;

use super::block::block_device::{BlockDevice, BlockDeviceOps};
use super::char::CharDevice;
use super::device::{Device, DeviceType};

pub mod kobject_uevent;

// https://code.dragonos.org.cn/xref/linux-6.1.9/lib/kobject_uevent.c?fi=kobject_uevent#457
#[derive(Debug)]
pub enum KobjectAction {
    KOBJADD,
    KOBJREMOVE, // Kobject（或上层数据结构）的添加/移除事件
    KOBJCHANGE, // Kobject（或上层数据结构）的状态或者内容发生改变; 如果设备驱动需要上报的事件不再上面事件的范围内，或者是自定义的事件，可以使用该event，并携带相应的参数。
    KOBJMOVE,   // Kobject（或上层数据结构）更改名称或者更改Parent（意味着在sysfs中更改了目录结构）
    KOBJONLINE,
    KOBJOFFLINE, // Kobject（或上层数据结构）的上线/下线事件，其实是是否使能
    KOBJBIND,
    KOBJUNBIND,
}

/// 解析一个字符串，以确定它代表的是哪个 kobject_action，并提取出随后的参数（如果有的话）
fn kobject_action_type(buf: &[u8]) -> Result<(KobjectAction, Vec<String>), SystemError> {
    let mut action = KobjectAction::KOBJCHANGE;
    let mut action_args: Vec<String> = Vec::new();
    let mut count = buf.len();
    if count != 0 && (buf[count - 1] == b'\n' || buf[count - 1] == b'\0') {
        count -= 1;
    }
    if count == 0 {
        return Err(SystemError::EINVAL);
    }

    let arg_start = buf.iter().position(|&c| c == b' ').unwrap_or(count);
    let count_first = arg_start;
    let args_start = arg_start + 1;

    // 匹配KobjectAction
    match &buf[..count_first] {
        b"add" => action = KobjectAction::KOBJADD,
        b"remove" => action = KobjectAction::KOBJREMOVE,
        b"change" => action = KobjectAction::KOBJCHANGE,
        b"move" => action = KobjectAction::KOBJMOVE,
        b"online" => action = KobjectAction::KOBJONLINE,
        b"offline" => action = KobjectAction::KOBJOFFLINE,
        b"bind" => action = KobjectAction::KOBJBIND,
        b"unbind" => action = KobjectAction::KOBJUNBIND,
        _ => return Err(SystemError::EINVAL),
    }

    // 如果有参数，提取参数
    if count - args_start > 0 {
        action_args = buf[args_start..]
            .split(|&c| c == b' ')
            .map(|s| String::from_utf8_lossy(s).to_string())
            .collect::<Vec<_>>();
    }

    Ok((action, action_args))
}

pub const UEVENT_NUM_ENVP: usize = 64;
pub const UEVENT_BUFFER_SIZE: usize = 2048;
pub const UEVENT_HELPER_PATH_LEN: usize = 256;

/// 表示处理内核对象 uevents 的环境
/// - envp，指针数组，用于保存每个环境变量的地址，最多可支持的环境变量数量为UEVENT_NUM_ENVP。
/// - envp_idx，用于访问环境变量指针数组的index。
/// - buf，保存环境变量的buffer，最大为UEVENT_BUFFER_SIZE。
/// - buflen，访问buf的变量。
// https://code.dragonos.org.cn/xref/linux-6.1.9/include/linux/kobject.h#31
#[derive(Debug)]
pub struct KobjUeventEnv {
    argv: Vec<String>,
    envp: Vec<String>,
    envp_idx: usize,
    buf: Vec<u8>,
    buflen: usize,
}

// kset_uevent_ops是为kset量身订做的一个数据结构，里面包含filter和uevent两个回调函数，用处如下：
/*
    filter，当任何Kobject需要上报uevent时，它所属的kset可以通过该接口过滤，阻止不希望上报的event，从而达到从整体上管理的目的。

    name，该接口可以返回kset的名称。如果一个kset没有合法的名称，则其下的所有Kobject将不允许上报uvent

    uevent，当任何Kobject需要上报uevent时，它所属的kset可以通过该接口统一为这些event添加环境变量。因为很多时候上报uevent时的环境变量都是相同的，因此可以由kset统一处理，就不需要让每个Kobject独自添加了。

*/

/// 设备文件夹下的`uevent`文件的属性
#[derive(Debug, Clone, Copy)]
pub struct UeventAttr;

impl Attribute for UeventAttr {
    fn name(&self) -> &str {
        "uevent"
    }

    fn mode(&self) -> crate::filesystem::vfs::syscall::ModeType {
        SYSFS_ATTR_MODE_RW
    }

    fn support(&self) -> crate::filesystem::sysfs::SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW | SysFSOpsSupport::ATTR_STORE
    }

    /// 用户空间读取 uevent 文件，返回 uevent 信息
    fn show(&self, _kobj: Arc<dyn KObject>, _buf: &mut [u8]) -> Result<usize, SystemError> {
        let device: Arc<dyn KObject> = _kobj
            .parent()
            .and_then(|x| x.upgrade())
            .ok_or(SystemError::ENODEV)?;
        let device = kobj2device(device).ok_or(SystemError::EINVAL)?;
        let device_type = device.dev_type();
        let mut uevent_content = String::new();
        match device_type {
            DeviceType::Block => {
                let block_device = device
                    .cast::<dyn BlockDevice>()
                    .ok()
                    .ok_or(SystemError::EINVAL)?;
                let major = block_device.id_table().device_number().major().data();
                let minor = block_device.id_table().device_number().minor();
                let device_name = block_device.id_table().name();
                writeln!(&mut uevent_content, "MAJOR={:?}", major).unwrap();
                writeln!(&mut uevent_content, "MINOR={:?}", minor).unwrap();
                writeln!(&mut uevent_content, "DEVNAME={}", device_name).unwrap();
                writeln!(&mut uevent_content, "DEVTYPE=disk").unwrap();
            }
            DeviceType::Char => {
                let char_device = device
                    .cast::<dyn CharDevice>()
                    .ok()
                    .ok_or(SystemError::EINVAL)?;
                let major = char_device.id_table().device_number().major().data();
                let minor = char_device.id_table().device_number().minor();
                let device_name = char_device.id_table().name();
                writeln!(&mut uevent_content, "MAJOR={}", major).unwrap();
                writeln!(&mut uevent_content, "MINOR={}", minor).unwrap();
                writeln!(&mut uevent_content, "DEVNAME={}", device_name).unwrap();
                writeln!(&mut uevent_content, "DEVTYPE=char").unwrap();
            }
            DeviceType::Net => {
                let net_device = device.cast::<dyn Iface>().ok().ok_or(SystemError::EINVAL)?;
                // let ifindex = net_device.ifindex().expect("Find ifindex error.\n");
                let device_name = net_device.iface_name();
                writeln!(&mut uevent_content, "INTERFACE={}", device_name).unwrap();
                // writeln!(&mut uevent_content, "IFINDEX={}", ifindex).unwrap();
            }
            _ => {
                // 处理其他设备类型
                let device_name = device.name();
                writeln!(&mut uevent_content, "DEVNAME={}", device_name).unwrap();
                writeln!(&mut uevent_content, "DEVTYPE={:?}", device_type).unwrap();
            }
        }
        sysfs_emit_str(_buf, &uevent_content)
    }
    /// 捕获来自用户空间对 uevent 文件的写操作，触发uevent事件
    fn store(&self, _kobj: Arc<dyn KObject>, _buf: &[u8]) -> Result<usize, SystemError> {
        return kobject_synth_uevent(_buf, _kobj);
    }
}

/// 将 kobject 转换为 device
fn kobj2device(kobj: Arc<dyn KObject>) -> Option<Arc<dyn Device>> {
    kobj.cast::<dyn Device>().ok()
}

/// 将设备的基本信息写入 uevent 文件
fn sysfs_emit_str(buf: &mut [u8], content: &str) -> Result<usize, SystemError> {
    let bytes = content.as_bytes();
    if buf.len() < bytes.len() {
        return Err(SystemError::ENOMEM);
    }
    buf[..bytes.len()].copy_from_slice(bytes);
    Ok(bytes.len())
}

/// 解析用户空间写入的 uevent 信息，触发 uevent 事件
fn kobject_synth_uevent(buf: &[u8], kobj: Arc<dyn KObject>) -> Result<usize, SystemError> {
    let no_uuid_envp = vec!["SYNTH_UUID=0".to_string()];
    let (action, action_args) = kobject_action_type(buf)?;

    let result = if action_args.is_empty() {
        kobject_uevent_env(kobj.clone(), action, no_uuid_envp)
    } else {
        kobject_uevent_env(kobj.clone(), action, action_args)
    };

    if let Err(e) = result {
        let device = kobj2device(kobj).ok_or(SystemError::EINVAL)?;
        let devname = device.name();
        log::error!("synth uevent: {}: {:?}", devname, e);
        return Err(SystemError::EINVAL);
    }
    Ok(buf.len())
}
