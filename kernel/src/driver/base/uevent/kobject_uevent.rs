// https://code.dragonos.org.cn/xref/linux-6.1.9/lib/kobject_uevent.c
use super::KObject;
use super::KobjUeventEnv;
use super::KobjectAction;
use super::{UEVENT_BUFFER_SIZE, UEVENT_NUM_ENVP};
use crate::driver::base::kobject::{KObjectManager, KObjectState};
use crate::init::initcall::INITCALL_POSTCORE;
use crate::libs::mutex::Mutex;
use crate::libs::rwlock::RwLock;
use crate::net::socket::netlink::af_netlink::netlink_has_listeners;
use crate::net::socket::netlink::af_netlink::NetlinkSocket;
use crate::net::socket::netlink::af_netlink::{netlink_broadcast, NetlinkSock};
use crate::net::socket::netlink::skbuff::SkBuff;
use crate::net::socket::netlink::{
    netlink_kernel_create, NetlinkKernelCfg, NETLINK_KOBJECT_UEVENT, NL_CFG_F_NONROOT_RECV,
};
use alloc::boxed::Box;
use alloc::collections::LinkedList;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write;
use num::Zero;
use system_error::SystemError;
use unified_init::macros::unified_init;
// 全局变量
pub static UEVENT_SEQNUM: u64 = 0;
// #ifdef CONFIG_UEVENT_HELPER
// char uevent_helper[UEVENT_HELPER_PATH_LEN] = CONFIG_UEVENT_HELPER_PATH;
// #endif

struct UeventSock {
    inner: NetlinkSock,
}
impl UeventSock {
    pub fn new(inner: NetlinkSock) -> Self {
        UeventSock { inner }
    }
}

// 用于存储所有用于发送 uevent 消息的 netlink sockets。这些 sockets 用于在内核和用户空间之间传递设备事件通知。
// 每当需要发送 uevent 消息时，内核会遍历这个链表，并通过其中的每一个 socket 发送消息。
// 使用 Mutex 保护全局链表
lazy_static::lazy_static! {
    static ref UEVENT_SOCK_LIST: Mutex<LinkedList<UeventSock>> = Mutex::new(LinkedList::new());
}
// 回调函数，当接收到 uevent 消息时调用
fn uevent_net_rcv() {
    // netlink_rcv_skb(skb, &uevent_net_rcv_skb);
}

/// 内核初始化的时候，在设备初始化之前执行
#[unified_init(INITCALL_POSTCORE)]
fn kobejct_uevent_init() -> Result<(), SystemError> {
    // todo: net namespace
    return uevent_net_init();
}
// TODO：等net namespace实现后添加 net 参数和相关操作
// 内核启动的时候，即使没有进行网络命名空间的隔离也需要调用这个函数
// 支持 net namespace 之后需要在每个 net namespace 初始化的时候调用这个函数
/// 为每一个 net namespace 初始化 uevent
fn uevent_net_init() -> Result<(), SystemError> {
    let cfg = NetlinkKernelCfg {
        groups: 1,
        flags: NL_CFG_F_NONROOT_RECV,
        ..Default::default()
    };
    // 创建一个内核 netlink socket
    let ue_sk = UeventSock::new(netlink_kernel_create(NETLINK_KOBJECT_UEVENT, Some(cfg)).unwrap());

    // todo: net namespace
    // net.uevent_sock = ue_sk;

    // 每个 net namespace 向链表中添加一个新的 uevent socket
    UEVENT_SOCK_LIST.lock().push_back(ue_sk);
    log::info!("uevent_net_init finish");
    return Ok(());
}

// 系统关闭时清理
fn uevent_net_exit() {
    // 清理链表
    UEVENT_SOCK_LIST.lock().clear();
}

// /* This lock protects uevent_seqnum and uevent_sock_list */
// static DEFINE_MUTEX(uevent_sock_mutex);

/*



*/

/// kobject_uevent，和kobject_uevent_env功能一样，只是没有指定任何的环境变量
pub fn kobject_uevent(kobj: Arc<dyn KObject>, action: KobjectAction) -> Result<(), SystemError> {
    // kobject_uevent和kobject_uevent_env功能一样，只是没有指定任何的环境变量
    match kobject_uevent_env(kobj, action, Vec::new()) {
        Ok(_) => Ok(()),
        Err(e) => Err(e),
    }
}

///  kobject_uevent_env，以envp为环境变量，上报一个指定action的uevent。环境变量的作用是为执行用户空间程序指定运行环境。
pub fn kobject_uevent_env(
    kobj: Arc<dyn KObject>,
    action: KobjectAction,
    envp_ext: Vec<String>,
) -> Result<i32, SystemError> {
    log::info!("kobject_uevent_env: kobj: {:?}, action: {:?}", kobj, action);
    let mut state = KObjectState::empty();
    let mut top_kobj = kobj.parent().unwrap().upgrade().unwrap();
    let mut retval: i32;
    let action_string = match action {
        KobjectAction::KOBJADD => "add".to_string(),
        KobjectAction::KOBJREMOVE => "remove".to_string(),
        KobjectAction::KOBJCHANGE => "change".to_string(),
        KobjectAction::KOBJMOVE => "move".to_string(),
        KobjectAction::KOBJONLINE => "online".to_string(),
        KobjectAction::KOBJOFFLINE => "offline".to_string(),
        KobjectAction::KOBJBIND => "bind".to_string(),
        KobjectAction::KOBJUNBIND => "unbind".to_string(),
    };
    /*
     * Mark "remove" event done regardless of result, for some subsystems
     * do not want to re-trigger "remove" event via automatic cleanup.
     */
    if let KobjectAction::KOBJREMOVE = action {
        log::info!("kobject_uevent_env: action: remove");
        state.insert(KObjectState::REMOVE_UEVENT_SENT);
    }

    // 不断向上查找，直到找到最顶层的kobject
    while let Some(weak_parent) = top_kobj.parent() {
        log::info!("kobject_uevent_env: top_kobj: {:?}", top_kobj);
        top_kobj = weak_parent.upgrade().unwrap();
    }
    /* 查找当前kobject或其parent是否从属于某个kset;如果都不从属于某个kset，则返回错误。(说明一个kobject若没有加入kset，是不会上报uevent的) */
    if kobj.kset().is_none() && top_kobj.kset().is_none() {
        log::info!("attempted to send uevent without kset!\n");
        return Err(SystemError::EINVAL);
    }

    let kset = top_kobj.kset();
    // 判断该 kobject 的状态是否设置了uevent_suppress，如果设置了，则忽略所有的uevent上报并返回
    if kobj.kobj_state().contains(KObjectState::UEVENT_SUPPRESS) {
        log::info!("uevent_suppress caused the event to drop!");
        return Ok(0);
    }

    // 如果所属的kset的kset->filter返回的是0，过滤此次上报
    if let Some(kset_ref) = kset.as_ref() {
        if let Some(uevent_ops) = &kset_ref.uevent_ops {
            if uevent_ops.filter() == Some(0) {
                log::info!("filter caused the event to drop!");
                return Ok(0);
            }
        }
    }

    // 判断所属的kset是否有合法的名称（称作subsystem，和前期的内核版本有区别），否则不允许上报uevent
    // originating subsystem
    let subsystem: String = if let Some(kset_ref) = kset.as_ref() {
        if let Some(uevent_ops) = &kset_ref.uevent_ops {
            let name = uevent_ops.uevent_name();
            if !name.is_empty() {
                name
            } else {
                kobj.name()
            }
        } else {
            kobj.name()
        }
    } else {
        kobj.name()
    };
    if subsystem.is_empty() {
        log::info!("unset subsystem caused the event to drop!");
    }
    log::info!("kobject_uevent_env: subsystem: {}", subsystem);

    // 创建一个用于环境变量的缓冲区
    let mut env = Box::new(KobjUeventEnv {
        argv: Vec::with_capacity(UEVENT_NUM_ENVP),
        envp: Vec::with_capacity(UEVENT_NUM_ENVP),
        envp_idx: 0,
        buf: vec![0; UEVENT_BUFFER_SIZE],
        buflen: 0,
    });
    if env.buf.is_empty() {
        log::error!("kobject_uevent_env: failed to allocate buffer");
        return Err(SystemError::ENOMEM);
    }

    // 获取设备的完整对象路径
    let devpath: String = KObjectManager::kobject_get_path(&kobj);
    log::info!("kobject_uevent_env: devpath: {}", devpath);
    if devpath.is_empty() {
        retval = SystemError::ENOENT.to_posix_errno();
        // goto exit
        drop(devpath);
        drop(env);
        log::warn!("kobject_uevent_env: devpath is empty");
        return Ok(retval);
    }
    retval = add_uevent_var(&mut env, "ACTION=%s", &action_string).unwrap();
    log::info!("kobject_uevent_env: retval: {}", retval);
    if !retval.is_zero() {
        drop(devpath);
        drop(env);
        log::info!("add_uevent_var failed ACTION");
        return Ok(retval);
    };
    retval = add_uevent_var(&mut env, "DEVPATH=%s", &devpath).unwrap();
    if !retval.is_zero() {
        drop(devpath);
        drop(env);
        log::info!("add_uevent_var failed DEVPATH");
        return Ok(retval);
    };
    retval = add_uevent_var(&mut env, "SUBSYSTEM=%s", &subsystem).unwrap();
    if !retval.is_zero() {
        drop(devpath);
        drop(env);
        log::info!("add_uevent_var failed SUBSYSTEM");
        return Ok(retval);
    };

    /* keys passed in from the caller */

    for var in envp_ext {
        let retval = add_uevent_var(&mut env, "%s", &var).unwrap();
        if !retval.is_zero() {
            drop(devpath);
            drop(env);
            log::info!("add_uevent_var failed");
            return Ok(retval);
        }
    }
    if let Some(kset_ref) = kset.as_ref() {
        if let Some(uevent_ops) = kset_ref.uevent_ops.as_ref() {
            if uevent_ops.uevent(&env) != 0 {
                retval = uevent_ops.uevent(&env);
                if retval.is_zero() {
                    log::info!("kset uevent caused the event to drop!");
                    // goto exit
                    drop(devpath);
                    drop(env);
                    return Ok(retval);
                }
            }
        }
    }
    match action {
        KobjectAction::KOBJADD => {
            state.insert(KObjectState::ADD_UEVENT_SENT);
        }
        KobjectAction::KOBJUNBIND => {
            zap_modalias_env(&mut env);
        }
        _ => {}
    }

    //mutex_lock(&uevent_sock_mutex);
    /* we will send an event, so request a new sequence number */
    retval = add_uevent_var(&mut env, "SEQNUM=%llu", &(UEVENT_SEQNUM + 1).to_string()).unwrap();
    if !retval.is_zero() {
        drop(devpath);
        drop(env);
        log::info!("add_uevent_var failed");
        return Ok(retval);
    }
    retval = kobject_uevent_net_broadcast(kobj, &env, &action_string, &devpath);
    //mutex_unlock(&uevent_sock_mutex);

    #[cfg(feature = "UEVENT_HELPER")]
    fn handle_uevent_helper() {
        // TODO
        // 在特性 `UEVENT_HELPER` 开启的情况下，这里的代码会执行
        // 指定处理uevent的用户空间程序，通常是热插拔程序mdev、udevd等
        // 	/* call uevent_helper, usually only enabled during early boot */
        // 	if (uevent_helper[0] && !kobj_usermode_filter(kobj)) {
        // 		struct subprocess_info *info;

        // 		retval = add_uevent_var(env, "HOME=/");
        // 		if (retval)
        // 			goto exit;
        // 		retval = add_uevent_var(env,
        // 					"PATH=/sbin:/bin:/usr/sbin:/usr/bin");
        // 		if (retval)
        // 			goto exit;
        // 		retval = init_uevent_argv(env, subsystem);
        // 		if (retval)
        // 			goto exit;

        // 		retval = -ENOMEM;
        // 		info = call_usermodehelper_setup(env->argv[0], env->argv,
        // 						 env->envp, GFP_KERNEL,
        // 						 NULL, cleanup_uevent_env, env);
        // 		if (info) {
        // 			retval = call_usermodehelper_exec(info, UMH_NO_WAIT);
        // 			env = NULL;	/* freed by cleanup_uevent_env */
        // 		}
        // 	}
    }
    #[cfg(not(feature = "UEVENT_HELPER"))]
    fn handle_uevent_helper() {
        // 在特性 `UEVENT_HELPER` 关闭的情况下，这里的代码会执行
    }
    handle_uevent_helper();
    drop(devpath);
    drop(env);
    log::info!("kobject_uevent_env: retval: {}", retval);
    return Ok(retval);
}

/// 以格式化字符的形式，将环境变量copy到env指针中。
pub fn add_uevent_var(
    env: &mut Box<KobjUeventEnv>,
    format: &str,
    args: &str,
) -> Result<i32, SystemError> {
    log::info!("add_uevent_var: format: {}, args: {}", format, args);
    if env.envp_idx >= env.envp.capacity() {
        log::info!("add_uevent_var: too many keys");
        return Err(SystemError::ENOMEM);
    }

    let mut buffer = String::new();
    write!(&mut buffer, "{} {}", format, args).map_err(|_| SystemError::ENOMEM)?;
    let len = buffer.len();

    if len >= env.buf.capacity() - env.buflen {
        log::info!("add_uevent_var: buffer size too small");
        return Err(SystemError::ENOMEM);
    }

    // Convert the buffer to bytes and add to env.buf
    env.buf.extend_from_slice(buffer.as_bytes());
    env.buf.push(0); // Null-terminate the string
    env.buflen += len + 1;

    // Add the string to envp
    env.envp.push(buffer);
    env.envp_idx += 1;

    Ok(0)
}

// 用于处理设备树中与模块相关的环境变量
fn zap_modalias_env(env: &mut Box<KobjUeventEnv>) {
    // 定义一个静态字符串
    const MODALIAS_PREFIX: &str = "MODALIAS=";
    let mut len: usize;

    let mut i = 0;
    while i < env.envp_idx {
        // 如果是以 MODALIAS= 开头的字符串
        if env.envp[i].starts_with(MODALIAS_PREFIX) {
            len = env.envp[i].len() + 1;
            // 如果不是最后一个元素
            if i != env.envp_idx - 1 {
                // 将后续的环境变量向前移动，以覆盖掉 "MODALIAS=" 前缀的环境变量
                for j in i..env.envp_idx - 1 {
                    env.envp[j] = env.envp[j + 1].clone();
                }
            }
            // 减少环境变量数组的索引，因为一个变量已经被移除
            env.envp_idx -= 1;
            // 减少环境变量的总长度
            env.buflen -= len;
        } else {
            i += 1;
        }
    }
}

// 用于处理网络相关的uevent（通用事件）广播
// https://code.dragonos.org.cn/xref/linux-6.1.9/lib/kobject_uevent.c#381
pub fn kobject_uevent_net_broadcast(
    kobj: Arc<dyn KObject>,
    env: &KobjUeventEnv,
    action_string: &str,
    devpath: &str,
) -> i32 {
    // let net:Net = None;
    // let mut ops = kobj_ns_ops(kobj);

    // if (!ops && kobj.kset().is_some()) {
    // 	let ksobj:KObject = &kobj.kset().kobj();

    // 	if (ksobj.parent() != NULL){
    //         ops = kobj_ns_ops(ksobj.parent());
    //     }

    // }
    // TODO: net结构体？
    // https://code.dragonos.org.cn/xref/linux-6.1.9/include/net/net_namespace.h#60
    /* kobjects currently only carry network namespace tags and they
     * are the only tag relevant here since we want to decide which
     * network namespaces to broadcast the uevent into.
     */
    // if (ops && ops.netlink_ns() && kobj.ktype().namespace())
    // 	if (ops.type() == KOBJ_NS_TYPE_NET)
    // 		net = kobj.ktype().namespace(kobj);
    // 如果有网络命名空间，则广播标记的uevent；如果没有，则广播未标记的uevent
    // if !net.is_none() {
    //     ret = uevent_net_broadcast_tagged(net.unwrap(), env, action_string, devpath);
    // } else {
    let ret = uevent_net_broadcast_untagged(env, action_string, devpath);
    // }
    log::info!("kobject_uevent_net_broadcast finish. ret: {}", ret);
    ret
}

pub fn uevent_net_broadcast_tagged(
    sk: &dyn NetlinkSocket,
    env: &KobjUeventEnv,
    action_string: &str,
    devpath: &str,
) -> i32 {
    let ret = 0;
    ret
}

/// 分配一个用于 uevent 消息的 skb（socket buffer）。
pub fn alloc_uevent_skb<'a>(
    env: &'a KobjUeventEnv,
    action_string: &'a str,
    devpath: &'a str,
) -> Arc<RwLock<SkBuff>> {
    let skb = Arc::new(RwLock::new(SkBuff::new()));
    skb
}
// https://code.dragonos.org.cn/xref/linux-6.1.9/lib/kobject_uevent.c#309
///  广播一个未标记的 uevent 消息
pub fn uevent_net_broadcast_untagged(
    env: &KobjUeventEnv,
    action_string: &str,
    devpath: &str,
) -> i32 {
    log::info!(
        "uevent_net_broadcast_untagged: action_string: {}, devpath: {}",
        action_string,
        devpath
    );
    let mut retval = 0;
    let mut skb = Arc::new(RwLock::new(SkBuff::new()));

    // 锁定 UEVENT_SOCK_LIST 并遍历
    let ue_sk_list = UEVENT_SOCK_LIST.lock();
    for ue_sk in ue_sk_list.iter() {
        // 如果没有监听者，则跳过
        if netlink_has_listeners(&ue_sk.inner, 1) == 0 {
            log::info!("uevent_net_broadcast_untagged: no listeners");
            continue;
        }
        // 如果 skb 为空，则分配一个新的 skb
        if skb.read().is_empty() {
            log::info!("uevent_net_broadcast_untagged: alloc_uevent_skb failed");
            retval = SystemError::ENOMEM.to_posix_errno();
            skb = alloc_uevent_skb(env, action_string, devpath);
            if skb.read().is_empty() {
                continue;
            }
        }
        log::info!("next is netlink_broadcast");
        let netlink_socket: Arc<dyn NetlinkSocket> = Arc::new(ue_sk.inner.clone());
        retval = match netlink_broadcast(&netlink_socket, Arc::clone(&skb), 0, 1, 1) {
            Ok(_) => 0,
            Err(err) => err.to_posix_errno(),
        };
        log::info!("finished netlink_broadcast");
        // ENOBUFS should be handled in userspace
        if retval == SystemError::ENOBUFS.to_posix_errno()
            || retval == SystemError::ESRCH.to_posix_errno()
        {
            retval = 0;
        }
    }
    // consume_skb(skb);
    retval
}
