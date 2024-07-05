use core::cell::RefCell;
use alloc::rc::Rc;
use core::fmt::Write;
// https://code.dragonos.org.cn/xref/linux-6.1.9/lib/kobject_uevent.c
/*

Variable

    kobject_actions √
    uevent_helper
    uevent_net_ops
    uevent_seqnum   √

Struct

    uevent_sock

Function

    action_arg_word_end
    add_uevent_var
    alloc_uevent_skb
    cleanup_uevent_env
    init_uevent_argv
    kobj_usermode_filter
    kobject_action_args
    kobject_action_type
    kobject_synth_uevent
    kobject_uevent  √
    kobject_uevent_env  √
    kobject_uevent_init
    kobject_uevent_net_broadcast    √
    uevent_net_broadcast
    uevent_net_broadcast_tagged
    uevent_net_broadcast_untagged √
    uevent_net_exit
    uevent_net_init
    uevent_net_rcv
    uevent_net_rcv_skb
    zap_modalias_env    √

*/
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use num::Zero;

use super::KObject;
use super::KobjUeventEnv;
use super::KobjectAction;
use super::{UEVENT_BUFFER_SIZE, UEVENT_NUM_ENVP};
use crate::driver::base::kobject::{KObjectManager, KObjectState};
use crate::net::net_core::consume_skb;
use crate::net::socket::netlink::af_netlink::{netlink_has_listeners, NetlinkFlags};
use crate::net::socket::netlink::af_netlink::NetlinkSocket;
use crate::net::socket::netlink::af_netlink::{netlink_broadcast, NetlinkSock};
use crate::net::socket::netlink::skbuff::SkBuff;
use crate::net::socket::Socket;
use alloc::boxed::Box;
use alloc::sync::Arc;
use system_error::SystemError;
// 存放需要用到的全局变量
pub static UEVENT_SEQNUM: u64 = 0;
pub static UEVENT_SUPPRESS: i32 = 1;
// #ifdef CONFIG_UEVENT_HELPER
// char uevent_helper[UEVENT_HELPER_PATH_LEN] = CONFIG_UEVENT_HELPER_PATH;
// #endif

// struct uevent_sock {
// 	struct list_head list;
// 	struct sock *sk;
// };

// #ifdef CONFIG_NET
// static LIST_HEAD(uevent_sock_list);
// #endif

// /* This lock protects uevent_seqnum and uevent_sock_list */
// static DEFINE_MUTEX(uevent_sock_mutex);

// to be adjust
pub const BUFFERSIZE: usize = 666;
#[derive(Debug)]
pub struct ListHead {
    next: Option<Box<ListHead>>,
    prev: Option<Box<ListHead>>,
}
// https://code.dragonos.org.cn/xref/linux-6.1.9/lib/kobject_uevent.c#38
#[derive(Debug)]
pub struct UeventSock {
    netlinksock: NetlinkSock,
    list: Vec<ListHead>,
}
impl UeventSock {}

impl NetlinkSocket for UeventSock {
    fn sk_family(&self) -> i32 {
        0
    }
    fn sk_state(&self) -> i32 {
        0
    }
    fn sk_protocol(&self) -> usize {
        0
    }
    fn is_kernel(&self) -> bool {
        true
    }
    fn equals(&self, other: &dyn NetlinkSocket) -> bool {
        false
    }
    fn portid(&self) -> u32 {
        0
    }
    fn ngroups(&self) -> u64 {
        0
    }
    fn groups(&self) -> Vec<u64> {
        Vec::new()
    }
    fn flags(&self) -> u32 {
        0
    }
}
// static const char *kobject_actions[] = {
// 	[KOBJ_ADD] =		"add",
// 	[KOBJ_REMOVE] =		"remove",
// 	[KOBJ_CHANGE] =		"change",
// 	[KOBJ_MOVE] =		"move",
// 	[KOBJ_ONLINE] =		"online",
// 	[KOBJ_OFFLINE] =	"offline",
// 	[KOBJ_BIND] =		"bind",
// 	[KOBJ_UNBIND] =		"unbind",
// };

/*
 kobject_uevent_env，以envp为环境变量，上报一个指定action的uevent。环境变量的作用是为执行用户空间程序指定运行环境。具体动作如下：

    查找kobj本身或者其parent是否从属于某个kset，如果不是，则报错返回（注2：由此可以说明，如果一个kobject没有加入kset，是不允许上报uevent的）
    查看kobj->uevent_suppress是否设置，如果设置，则忽略所有的uevent上报并返回（注3：由此可知，可以通过Kobject的uevent_suppress标志，管控Kobject的uevent的上报）
    如果所属的kset有kset->filter函数，则调用该函数，过滤此次上报（注4：这佐证了3.2小节有关filter接口的说明，kset可以通过filter接口过滤不希望上报的event，从而达到整体的管理效果）
    判断所属的kset是否有合法的名称（称作subsystem，和前期的内核版本有区别），否则不允许上报uevent
    分配一个用于此次上报的、存储环境变量的buffer（结果保存在env指针中），并获得该Kobject在sysfs中路径信息（用户空间软件需要依据该路径信息在sysfs中访问它）
    调用add_uevent_var接口（下面会介绍），将Action、路径信息、subsystem等信息，添加到env指针中
    如果传入的envp不空，则解析传入的环境变量中，同样调用add_uevent_var接口，添加到env指针中
    如果所属的kset存在kset->uevent接口，调用该接口，添加kset统一的环境变量到env指针
    根据ACTION的类型，设置kobj->state_add_uevent_sent和kobj->state_remove_uevent_sent变量，以记录正确的状态
    调用add_uevent_var接口，添加格式为"SEQNUM=%llu”的序列号
    如果定义了"CONFIG_NET”，则使用netlink发送该uevent
    以uevent_helper、subsystem以及添加了标准环境变量（HOME=/，PATH=/sbin:/bin:/usr/sbin:/usr/bin）的env指针为参数，调用kmod模块提供的call_usermodehelper函数，上报uevent。
    其中uevent_helper的内容是由内核配置项CONFIG_UEVENT_HELPER_PATH(位于./drivers/base/Kconfig)决定的(可参考lib/kobject_uevent.c, line 32)，该配置项指定了一个用户空间程序（或者脚本），用于解析上报的uevent，例如"/sbin/hotplug”。
    call_usermodehelper的作用，就是fork一个进程，以uevent为参数，执行uevent_helper。

kobject_uevent，和kobject_uevent_env功能一样，只是没有指定任何的环境变量。

add_uevent_var，以格式化字符的形式（类似printf、printk等），将环境变量copy到env指针中。

kobject_action_type，将enum kobject_action类型的Action，转换为字符串
*/

//kobject_uevent->kobject_uevent_env
pub fn kobject_uevent(kobj: Arc<dyn KObject>, action: KobjectAction) -> Result<(), SystemError> {
    // kobject_uevent和kobject_uevent_env功能一样，只是没有指定任何的环境变量
    match kobject_uevent_env(kobj, action, None) {
        Ok(_) => Ok(()),
        Err(e) => Err(e),
    }
}
pub fn kobject_uevent_env(
    kobj: Arc<dyn KObject>,
    action: KobjectAction,
    envp_ext: Option<Vec<String>>,
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
        state.insert(KObjectState::REMOVE_UEVENT_SENT);
    }

    /* search the kset we belong to */
    while let Some(weak_parent) = top_kobj.parent() {
        top_kobj = weak_parent.upgrade().unwrap();
    }

    if top_kobj.kset().is_none() {
        log::info!("attempted to send uevent without kset!\n");
        return Err(SystemError::EINVAL);
    }

    let kset = top_kobj.kset();
    /* skip the event, if uevent_suppress is set*/
    if UEVENT_SUPPRESS == 1 {
        log::info!("uevent_suppress caused the event to drop!");
        return Ok(0);
    }

    /* skip the event, if the filter returns zero. */
    if kset.as_ref().unwrap().uevent_ops.is_some()
        && kset.as_ref().unwrap().uevent_ops.is_none()
    {
        log::info!("filter caused the event to drop!");
        return Ok(0);
    }

    /* originating subsystem */
    let subsystem: String = if kset.as_ref().unwrap().uevent_ops.is_some()
        && kset
            .as_ref()
            .unwrap()
            .uevent_ops
            .as_ref()
            .unwrap()
            .uevent_name()
            != ""
    {
        kset.as_ref()
            .unwrap()
            .uevent_ops
            .as_ref()
            .unwrap()
            .uevent_name()
    } else {
        kobj.name()
    };
    if subsystem.is_empty() {
        log::info!("unset sussystem caused the event to drop!");
    }

    /* environment buffer */
    // 创建一个用于环境变量的缓冲区
    let mut env = Box::new(KobjUeventEnv {
        argv: Vec::with_capacity(UEVENT_NUM_ENVP),
        envp: Vec::with_capacity(UEVENT_NUM_ENVP),
        envp_idx: 0,
        buf: Vec::with_capacity(UEVENT_BUFFER_SIZE),
        buflen: 0,
    });
    if env.buf.is_empty() {
        return Err(SystemError::ENOMEM);
    }

    //获取设备的完整对象路径
    /* complete object path */
    let devpath: String = KObjectManager::kobject_get_path(&kobj);
    if devpath.is_empty() {
        retval = SystemError::ENOENT.to_posix_errno();
        // goto exit
        drop(devpath);
        drop(env);
        return Ok(retval);
    }
    retval = add_uevent_var(&mut env, "ACTION=%s", &action_string).unwrap();
    if retval.is_zero() {
        drop(devpath);
        drop(env);
        return Ok(retval);
    };
    retval = add_uevent_var(&mut env, "DEVPATH=%s", &devpath).unwrap();
    if retval.is_zero() {
        drop(devpath);
        drop(env);
        return Ok(retval);
    };
    retval = add_uevent_var(&mut env, "SUBSYSTEM=%s", &subsystem).unwrap();
    if retval.is_zero() {
        drop(devpath);
        drop(env);
        return Ok(retval);
    };

    /* keys passed in from the caller */
    if let Some(env_ext) = envp_ext {
        for var in env_ext {
            let retval = add_uevent_var(&mut env, "%s", &var).unwrap();
            if retval.is_zero() {
                drop(devpath);
                drop(env);
                return Ok(retval);
            }
        }
    }
    if kset.as_ref().unwrap().uevent_ops.is_some()
        && kset
            .as_ref()
            .unwrap()
            .uevent_ops
            .as_ref()
            .unwrap()
            .uevent(&env)
            != 0
    {
        retval = kset
            .as_ref()
            .unwrap()
            .uevent_ops
            .as_ref()
            .unwrap()
            .uevent(&env);
        if retval.is_zero() {
            log::info!("kset uevent caused the event to drop!");
            // goto exit
            drop(devpath);
            drop(env);
            return Ok(retval);
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
    if retval.is_zero() {
        drop(devpath);
        drop(env);
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
    return Ok(retval);
}

pub fn add_uevent_var(
    env: &mut Box<KobjUeventEnv>,
    format: &str,
    args: &String,
) -> Result<i32, SystemError> {
    if env.envp_idx >= env.envp.len() {
        log::info!("add_uevent_var: too many keys");
        return Err(SystemError::ENOMEM);
    }

    let mut buffer = String::with_capacity(env.buf.len() - env.buflen);
    write!(&mut buffer, "{} {}", format, args).map_err(|_| SystemError::ENOMEM)?;
    let len = buffer.len();

    if len >= env.buf.len() - env.buflen {
        log::info!("add_uevent_var: buffer size too small");
        return Err(SystemError::ENOMEM);
    }

    env.envp[env.envp_idx].replace(buffer);
    env.envp_idx += 1;
    env.buflen += len + 1;

    Ok(0)
}

// 用于处理设备树中与模块相关的环境变量
fn zap_modalias_env(env: &mut Box<KobjUeventEnv>) {
    // 定义一个静态字符串
    const MODALIAS_PREFIX: &str = "MODALIAS=";
    let mut len: usize;

    for i in 0..env.envp_idx {
        // 如果存在而且是以MODALIAS=开头的字符串
        if env.envp[i].is_some() && env.envp[i].as_ref().unwrap().starts_with("MODALIAS=") {
            len = env.envp[i].as_ref().unwrap().len() + 1;
            // 如果不是最后一个元素
            if i != env.envp_idx - 1 {
                // 将下一个环境变量移动到当前的位置，这样可以覆盖掉"MODALIAS="前缀的环境变量。
                let next_envp = env.envp[i + 1].as_ref().unwrap().clone();
                env.envp[i].replace(next_envp);
                // 更新数组中后续元素的位置，以反映它们被移动后的位置
                for j in i..env.envp_idx - 1 {
                    let next_envp = env.envp[j + 1].as_ref().unwrap().clone();
                    env.envp[j].replace(next_envp);
                }
            }
            // 减少环境变量数组的索引，因为一个变量已经被移除
            env.envp_idx -= 1;
            // 减少环境变量的总长度
            env.buflen -= len;
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
    let mut ret = 0;
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
    //     ret = uevent_net_broadcast_tagged(net.unwrap().sk, env, action_string, devpath);
    // } else {
    ret = uevent_net_broadcast_untagged(env, action_string, devpath);
    // }
    ret
}

pub fn uevent_net_broadcast_tagged(
    sk: &dyn Socket,
    env: &KobjUeventEnv,
    action_string: &str,
    devpath: &str,
) -> i32 {
    let ret = 0;
    ret
}
static UEVENT_SOCK_LIST: Vec<UeventSock> = Vec::new();
pub fn alloc_uevent_skb<'a>(
    env: &'a KobjUeventEnv,
    action_string: &'a str,
    devpath: &'a str,
) -> Rc<RefCell<SkBuff<'a>>> {
    let skb = Rc::new(RefCell::new(SkBuff::new()));
    skb
}
pub fn uevent_net_broadcast_untagged(
    env: &KobjUeventEnv,
    action_string: &str,
    devpath: &str,
) -> i32 {
    let mut retval = 0;
    let skb = Rc::new(RefCell::new(SkBuff::new()));

    // 模拟 skb_get 行为，增加引用并返回引用
    fn get_packet_buffer(shared_skb: Rc<RefCell<SkBuff>>) -> Rc<RefCell<SkBuff>> {
        // Rc::clone 会增加内部引用计数
        shared_skb.clone()
    }

    // 发送uevent message
    for ue_sk in &UEVENT_SOCK_LIST {
        let uevent_sock = &ue_sk.netlinksock;
        if netlink_has_listeners(uevent_sock, 1) == 0 {
            continue;
        }

        if skb.borrow().is_empty() {
            retval = SystemError::ENOMEM.to_posix_errno();
            let skb = alloc_uevent_skb(env, action_string, devpath);
            if skb.borrow().is_empty() {
                continue;
            }
        }

        retval =
            match netlink_broadcast(&ue_sk.netlinksock, get_packet_buffer(skb.clone()), 0, 1, 1) {
                Ok(_) => 0,
                Err(err) => err.to_posix_errno(),
            };

        // ENOBUFS should be handled in userspace
        if retval == SystemError::ENOBUFS.to_posix_errno()
            || retval == SystemError::ESRCH.to_posix_errno()
        {
            retval = 0;
        }
    }
    consume_skb(skb);
    retval
}
