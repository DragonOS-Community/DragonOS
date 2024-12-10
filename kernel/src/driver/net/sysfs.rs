use crate::{
    driver::base::{
        class::Class,
        device::{device_manager, Device},
        kobject::KObject,
    },
    filesystem::{
        sysfs::{
            file::sysfs_emit_str, Attribute, AttributeGroup, SysFSOpsSupport, SYSFS_ATTR_MODE_RO,
            SYSFS_ATTR_MODE_RW,
        },
        vfs::syscall::ModeType,
    },
};
use alloc::sync::Arc;
use intertrait::cast::CastArc;
use log::error;
use system_error::SystemError;

use super::{class::sys_class_net_instance, NetDeivceState, NetDevice, Operstate};

/// 将设备注册到`/sys/class/net`目录下
/// 参考：https://code.dragonos.org.cn/xref/linux-2.6.39/net/core/net-sysfs.c?fi=netdev_register_kobject#1311
pub fn netdev_register_kobject(dev: Arc<dyn NetDevice>) -> Result<(), SystemError> {
    // 初始化设备
    device_manager().device_default_initialize(&(dev.clone() as Arc<dyn Device>));

    // 设置dev的class为net
    dev.set_class(Some(Arc::downgrade(
        &(sys_class_net_instance().cloned().unwrap() as Arc<dyn Class>),
    )));

    // 设置设备的kobject名
    dev.set_name(dev.iface_name().clone());

    device_manager().add_device(dev.clone() as Arc<dyn Device>)?;

    return Ok(());
}

// 参考：https://code.dragonos.org.cn/xref/linux-6.6.21/net/core/net-sysfs.c
#[derive(Debug)]
pub struct NetAttrGroup;

impl AttributeGroup for NetAttrGroup {
    fn name(&self) -> Option<&str> {
        None
    }

    fn attrs(&self) -> &[&'static dyn Attribute] {
        &[
            &AttrAddrAssignType,
            &AttrAddrLen,
            &AttrDevId,
            &AttrIfalias,
            &AttrIflink,
            &AttrIfindex,
            &AttrFeatrues,
            &AttrType,
            &AttrLinkMode,
            &AttrAddress,
            &AttrBroadcast,
            &AttrCarrier,
            &AttrSpeed,
            &AttrDuplex,
            &AttrDormant,
            &AttrOperstate,
            &AttrMtu,
            &AttrFlags,
            &AttrTxQueueLen,
            &AttrNetdevGroup,
        ]
    }

    fn is_visible(
        &self,
        _kobj: Arc<dyn KObject>,
        attr: &'static dyn Attribute,
    ) -> Option<ModeType> {
        return Some(attr.mode());
    }
}

/// # 表示网络接口的MAC地址是如何分配的
/// - 0(NET_ADDR_PERM): 永久的MAC地址（默认值）
/// - 1(NET_ADDR_RANDOM): 随机生成的MAC地址
/// - 2(NET_ADDR_STOLEN): 从其他设备中获取的MAC地址
/// - 3(NET_ADDR_SET): 由用户设置的MAC地址
#[derive(Debug)]
struct AttrAddrAssignType;

impl Attribute for AttrAddrAssignType {
    fn name(&self) -> &str {
        "addr_assign_type"
    }

    fn mode(&self) -> ModeType {
        SYSFS_ATTR_MODE_RO
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW
    }

    fn show(&self, kobj: Arc<dyn KObject>, buf: &mut [u8]) -> Result<usize, SystemError> {
        let net_device = kobj.cast::<dyn NetDevice>().map_err(|_| {
            error!("AttrAddrAssignType::show() failed: kobj is not a NetDevice");
            SystemError::EINVAL
        })?;
        let addr_assign_type = net_device.addr_assign_type();
        sysfs_emit_str(buf, &format!("{}\n", addr_assign_type))
    }
}

/// # 表示网络接口的MAC地址的长度，以字节为单位
#[derive(Debug)]
struct AttrAddrLen;

impl Attribute for AttrAddrLen {
    fn name(&self) -> &str {
        "addr_len"
    }

    fn mode(&self) -> ModeType {
        SYSFS_ATTR_MODE_RO
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW
    }

    fn show(&self, _kobj: Arc<dyn KObject>, _buf: &mut [u8]) -> Result<usize, SystemError> {
        todo!("AttrAddrLen::show")
    }
}

/// # 表示网络接口的设备ID，是一个十六进制数
#[derive(Debug)]
struct AttrDevId;

impl Attribute for AttrDevId {
    fn name(&self) -> &str {
        "dev_id"
    }

    fn mode(&self) -> ModeType {
        SYSFS_ATTR_MODE_RO
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW
    }

    fn show(&self, _kobj: Arc<dyn KObject>, _buf: &mut [u8]) -> Result<usize, SystemError> {
        todo!("AttrDevId::show")
    }
}

/// # 表示网络接口的别名，可以设置
#[derive(Debug)]
struct AttrIfalias;

impl Attribute for AttrIfalias {
    fn name(&self) -> &str {
        "ifalias"
    }

    fn mode(&self) -> ModeType {
        SYSFS_ATTR_MODE_RW
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW | SysFSOpsSupport::ATTR_STORE
    }

    fn show(&self, _kobj: Arc<dyn KObject>, _buf: &mut [u8]) -> Result<usize, SystemError> {
        todo!("AttrIfalias::show")
    }

    fn store(&self, _kobj: Arc<dyn KObject>, _buf: &[u8]) -> Result<usize, SystemError> {
        todo!("AttrIfalias::store")
    }
}

/// # 表示网络接口的链路索引，用于表示网络接口在系统中的位置
#[derive(Debug)]
struct AttrIflink;

impl Attribute for AttrIflink {
    fn name(&self) -> &str {
        "iflink"
    }

    fn mode(&self) -> ModeType {
        SYSFS_ATTR_MODE_RO
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW
    }

    fn show(&self, _kobj: Arc<dyn KObject>, _buf: &mut [u8]) -> Result<usize, SystemError> {
        todo!("AttrIflink::show")
    }
}

/// # 标识网络接口的索引
#[derive(Debug)]
struct AttrIfindex;

impl Attribute for AttrIfindex {
    fn name(&self) -> &str {
        "ifindex"
    }

    fn mode(&self) -> ModeType {
        SYSFS_ATTR_MODE_RO
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW
    }

    fn show(&self, _kobj: Arc<dyn KObject>, _buf: &mut [u8]) -> Result<usize, SystemError> {
        todo!("AttrIfindex::show")
    }
}

/// # 用于显示网络接口支持的特性，这些特性通常由网络驱动程序和硬件能力决定
#[derive(Debug)]
struct AttrFeatrues;

impl Attribute for AttrFeatrues {
    fn name(&self) -> &str {
        "features"
    }

    fn mode(&self) -> ModeType {
        SYSFS_ATTR_MODE_RO
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW
    }

    fn show(&self, _kobj: Arc<dyn KObject>, _buf: &mut [u8]) -> Result<usize, SystemError> {
        todo!("AttrFeatrues::show")
    }
}

/// # 用于表示网络接口的类型
/// - 1：ARPHRD_ETHER 以太网接口
/// - 24：ARPHRD_LOOPBACK 回环接口
/// - 512：ARPHRD_IEEE80211_RADIOTAP IEEE 802.11 无线接口
/// - 768：ARPHRD_IEEE802154 IEEE 802.15.4 无线接口
/// - 769：ARPHRD_6LOWPAN 6LoWPAN接口
#[derive(Debug)]
struct AttrType;

impl Attribute for AttrType {
    fn name(&self) -> &str {
        "type"
    }

    fn mode(&self) -> ModeType {
        SYSFS_ATTR_MODE_RO
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW
    }

    fn show(&self, kobj: Arc<dyn KObject>, buf: &mut [u8]) -> Result<usize, SystemError> {
        let net_deive = kobj.cast::<dyn NetDevice>().map_err(|_| {
            error!("AttrType::show() failed: kobj is not a NetDevice");
            SystemError::EINVAL
        })?;
        let net_type = net_deive.net_device_type();
        sysfs_emit_str(buf, &format!("{}\n", net_type))
    }
}

/// # 表示网络接口的链路模式，用于指示网络接口是否处于自动协商模式
/// - 0：表示网络接口处于自动协商模式
/// - 1：表示网络接口处于强制模式，即链路参数（如速度和双工模式）是手动配置的，而不是通过自动协商确定的
#[derive(Debug)]
struct AttrLinkMode;

impl Attribute for AttrLinkMode {
    fn name(&self) -> &str {
        "link_mode"
    }

    fn mode(&self) -> ModeType {
        SYSFS_ATTR_MODE_RO
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW
    }

    fn show(&self, _kobj: Arc<dyn KObject>, _buf: &mut [u8]) -> Result<usize, SystemError> {
        todo!("AttrLinkMode::show")
    }
}

/// # 表示网络接口的MAC地址
#[derive(Debug)]
struct AttrAddress;

impl Attribute for AttrAddress {
    fn name(&self) -> &str {
        "address"
    }

    fn mode(&self) -> ModeType {
        SYSFS_ATTR_MODE_RO
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW
    }

    fn show(&self, kobj: Arc<dyn KObject>, buf: &mut [u8]) -> Result<usize, SystemError> {
        let net_device = kobj.cast::<dyn NetDevice>().map_err(|_| {
            error!("AttrAddress::show() failed: kobj is not a NetDevice");
            SystemError::EINVAL
        })?;
        let mac_addr = net_device.mac();
        sysfs_emit_str(buf, &format!("{}\n", mac_addr))
    }
}

/// # 表示网络接口的广播地址
#[derive(Debug)]
struct AttrBroadcast;

impl Attribute for AttrBroadcast {
    fn name(&self) -> &str {
        "broadcast"
    }

    fn mode(&self) -> ModeType {
        SYSFS_ATTR_MODE_RO
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW
    }

    fn show(&self, _kobj: Arc<dyn KObject>, _buf: &mut [u8]) -> Result<usize, SystemError> {
        todo!("AttrBroadcast::show")
    }
}

/// # 表示网络接口的物理链路状态
/// - 0：表示网络接口处于关闭状态
/// - 1：表示网络接口处于打开状态
#[derive(Debug)]
struct AttrCarrier;

impl Attribute for AttrCarrier {
    fn name(&self) -> &str {
        "carrier"
    }

    fn mode(&self) -> ModeType {
        SYSFS_ATTR_MODE_RO
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW
    }

    fn show(&self, kobj: Arc<dyn KObject>, buf: &mut [u8]) -> Result<usize, SystemError> {
        let net_device = kobj.cast::<dyn NetDevice>().map_err(|_| {
            error!("AttrCarrier::show() failed: kobj is not a NetDevice");
            SystemError::EINVAL
        })?;
        if net_device
            .net_state()
            .contains(NetDeivceState::__LINK_STATE_START)
            && !net_device
                .net_state()
                .contains(NetDeivceState::__LINK_STATE_NOCARRIER)
        {
            sysfs_emit_str(buf, "1\n")
        } else {
            sysfs_emit_str(buf, "0\n")
        }
    }
}

/// # 表示网络接口的当前连接速度，单位为Mbps
/// - 特殊值：-1，表示无法确定，通常是因为接口不支持查询速度或接口未连接
#[derive(Debug)]
struct AttrSpeed;

impl Attribute for AttrSpeed {
    fn name(&self) -> &str {
        "speed"
    }

    fn mode(&self) -> ModeType {
        SYSFS_ATTR_MODE_RO
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW
    }

    fn show(&self, _kobj: Arc<dyn KObject>, _buf: &mut [u8]) -> Result<usize, SystemError> {
        todo!("AttrSpeed::show")
    }
}

/// # 表示网络接口的双工模式
/// - half：半双工，网络接口不能同时发送和接收数据
/// - full：全双工，网络接口可以同时发送和接收数据
/// - unknown：未知，通常表示接口未连接或无法确定双工模式
#[derive(Debug)]
struct AttrDuplex;

impl Attribute for AttrDuplex {
    fn name(&self) -> &str {
        "duplex"
    }

    fn mode(&self) -> ModeType {
        SYSFS_ATTR_MODE_RO
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW
    }

    fn show(&self, _kobj: Arc<dyn KObject>, _buf: &mut [u8]) -> Result<usize, SystemError> {
        todo!("AttrDuplex::show")
    }
}

/// 表示网络接口是否处于休眠状态
/// - 0：表示网络接口未处于休眠状态
/// - 1：表示网络接口处于休眠状态
#[derive(Debug)]
struct AttrDormant;

impl Attribute for AttrDormant {
    fn name(&self) -> &str {
        "dormant"
    }

    fn mode(&self) -> ModeType {
        SYSFS_ATTR_MODE_RO
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW
    }

    fn show(&self, _kobj: Arc<dyn KObject>, _buf: &mut [u8]) -> Result<usize, SystemError> {
        todo!("AttrDormant::show")
    }
}

/// # 表示网络接口的操作状态
/// - up：网络接口已启用并且正在运行
/// - down：网络接口已禁用或未连接
/// - dormant：网络接口处于休眠状态，等待某些条件满足后激活
/// - testing：网络接口正在测试中
/// - unknown：网络接口的状态未知
/// - notpresent：网络接口硬件不存在
/// - lowerlayerdown：网络接口的底层设备未启用
/// - inactive：网络接口未激活
#[derive(Debug)]
struct AttrOperstate;

impl Attribute for AttrOperstate {
    fn name(&self) -> &str {
        "operstate"
    }

    fn mode(&self) -> ModeType {
        SYSFS_ATTR_MODE_RO
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW
    }

    fn show(&self, _kobj: Arc<dyn KObject>, _buf: &mut [u8]) -> Result<usize, SystemError> {
        let net_device = _kobj.cast::<dyn NetDevice>().map_err(|_| {
            error!("AttrOperstate::show() failed: kobj is not a NetDevice");
            SystemError::EINVAL
        })?;
        if !net_device
            .net_state()
            .contains(NetDeivceState::__LINK_STATE_START)
        {
            net_device.set_operstate(Operstate::IF_OPER_DOWN);
        }

        let operstate_str = match net_device.operstate() {
            Operstate::IF_OPER_UP => "up",
            Operstate::IF_OPER_DOWN => "down",
            Operstate::IF_OPER_DORMANT => "dormant",
            Operstate::IF_OPER_TESTING => "testing",
            Operstate::IF_OPER_UNKNOWN => "unknown",
            Operstate::IF_OPER_NOTPRESENT => "notpresent",
            Operstate::IF_OPER_LOWERLAYERDOWN => "lowerlayerdown",
        };

        sysfs_emit_str(_buf, &format!("{}\n", operstate_str))
    }
}

/// # 表示网络接口的最大传输单元
#[derive(Debug)]
struct AttrMtu;

impl Attribute for AttrMtu {
    fn name(&self) -> &str {
        "mtu"
    }

    fn mode(&self) -> ModeType {
        SYSFS_ATTR_MODE_RO
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW
    }

    fn show(&self, _kobj: Arc<dyn KObject>, _buf: &mut [u8]) -> Result<usize, SystemError> {
        todo!("AttrMtu::show")
    }

    fn store(&self, _kobj: Arc<dyn KObject>, _buf: &[u8]) -> Result<usize, SystemError> {
        todo!("AttrMtu::store")
    }
}

/// # 表示网络接口的标志，这些标志提供了关于网络接口状态和配置的详细信息
/// - IFF_UP(0x1)：接口已启用
/// - IFF_BROADCAST(0x2)：支持广播
/// - IFF_DEBUG(0x4)：调试模式
/// - IFF_LOOPBACK(0x8)：环回接口
/// - IFF_POINTOPOINT(0x10)：点对点链路
/// - IFF_NOTRAILERS(0x20)：禁用拖尾
/// - IFF_RUNNING(0x40)：资源已分配
/// - IFF_NOARP(0x80)：无ARP协议
/// - IFF_PROMISC(0x100)：混杂模式
/// - IFF_ALLMULTI(0x200)：接收所有多播放数据包
/// - IFF_ MASTER(0x400)：主设备
/// - IFF_SLAVE(0x800)：从设备
/// - IFF_MULTICAST(0x1000)：支持多播
/// - IFF_PORTSEL(0x2000)：可以选择媒体类型
/// - IFF_AUTOMEDIA(0x4000)：自动选择媒体类型
/// - IFF_DYNAMIC(0x8000)：动态接口
#[derive(Debug)]
struct AttrFlags;

impl Attribute for AttrFlags {
    fn name(&self) -> &str {
        "flags"
    }

    fn mode(&self) -> ModeType {
        SYSFS_ATTR_MODE_RO
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW
    }

    fn show(&self, _kobj: Arc<dyn KObject>, _buf: &mut [u8]) -> Result<usize, SystemError> {
        todo!("AttrFlags::show")
    }

    fn store(&self, _kobj: Arc<dyn KObject>, _buf: &[u8]) -> Result<usize, SystemError> {
        todo!("AttrFlags::store")
    }
}

/// # 表示网络接口的传输队列长度
#[derive(Debug)]
struct AttrTxQueueLen;

impl Attribute for AttrTxQueueLen {
    fn name(&self) -> &str {
        "tx_queue_len"
    }

    fn mode(&self) -> ModeType {
        SYSFS_ATTR_MODE_RO
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW
    }

    fn show(&self, _kobj: Arc<dyn KObject>, _buf: &mut [u8]) -> Result<usize, SystemError> {
        todo!("AttrTxQueueLen::show")
    }

    fn store(&self, _kobj: Arc<dyn KObject>, _buf: &[u8]) -> Result<usize, SystemError> {
        todo!("AttrTxQueueLen::store")
    }
}

/// # 表示网络设备所属的设备组
#[derive(Debug)]
struct AttrNetdevGroup;

impl Attribute for AttrNetdevGroup {
    fn name(&self) -> &str {
        "netdev_group"
    }

    fn mode(&self) -> ModeType {
        SYSFS_ATTR_MODE_RO
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW
    }

    fn show(&self, _kobj: Arc<dyn KObject>, _buf: &mut [u8]) -> Result<usize, SystemError> {
        todo!("AttrNetdevGroup::show")
    }

    fn store(&self, _kobj: Arc<dyn KObject>, _buf: &[u8]) -> Result<usize, SystemError> {
        todo!("AttrNetdevGroup::store")
    }
}
