//https://code.dragonos.org.cn/xref/linux-6.1.9/lib/kobject_uevent.c
/*

Variable

    kobject_actions
    uevent_helper
    uevent_net_ops
    uevent_seqnum

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
    kobject_uevent
    kobject_uevent_env
    kobject_uevent_init
    kobject_uevent_net_broadcast
    uevent_net_broadcast
    uevent_net_broadcast_tagged
    uevent_net_broadcast_untagged
    uevent_net_exit
    uevent_net_init
    uevent_net_rcv
    uevent_net_rcv_skb
    zap_modalias_env
    
*/
use alloc::string::String;
use alloc::vec::Vec;
use crate::driver::base::kobject::{KObjectState,UEVENT_SUPPRESS};
use crate::net::socket::Socket;
use super::KobjectAction;
use super::KObject;
use super::KobjUeventEnv;
use super::{UEVENT_NUM_ENVP,UEVENT_BUFFER_SIZE};
use crate::libs::mutex::Mutex;
use alloc::sync::Arc;
use alloc::sync::Weak;




// u64 uevent_seqnum;
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

pub struct UeventSock {
    //list: Vec<dyn list_head>,
    sk: dyn Socket,
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
    如果所属的kset有uevent_ops->filter函数，则调用该函数，过滤此次上报（注4：这佐证了3.2小节有关filter接口的说明，kset可以通过filter接口过滤不希望上报的event，从而达到整体的管理效果）
    判断所属的kset是否有合法的名称（称作subsystem，和前期的内核版本有区别），否则不允许上报uevent
    分配一个用于此次上报的、存储环境变量的buffer（结果保存在env指针中），并获得该Kobject在sysfs中路径信息（用户空间软件需要依据该路径信息在sysfs中访问它）
    调用add_uevent_var接口（下面会介绍），将Action、路径信息、subsystem等信息，添加到env指针中
    如果传入的envp不空，则解析传入的环境变量中，同样调用add_uevent_var接口，添加到env指针中
    如果所属的kset存在uevent_ops->uevent接口，调用该接口，添加kset统一的环境变量到env指针
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
    pub fn kobject_uevent(kobj: &dyn KObject, action: KobjectAction) -> Result<(), &'static str> {
        // kobject_uevent和kobject_uevent_env功能一样，只是没有指定任何的环境变量
        match kobject_uevent_env(kobj, action, None) {
            Ok(_) => Ok(()), // return Ok(()) on success
            Err(e) => Err(e), // return the error on failure
        }
    }
    pub fn kobject_uevent_env(kobj: &dyn KObject, action: KobjectAction, envp_ext: Option<Vec<String>>) -> Result<(), &'static str> {
        
        // todo: 定义一些常量和变量
        // init uevent env
        let env = KobjUeventEnv {
            argv: [None, None, None],
            envp: [None; UEVENT_NUM_ENVP],
            envp_idx: 0,
            buf: [0; UEVENT_BUFFER_SIZE],
            buflen: 0,
        };
        
        let kset = kobj.kset().unwrap();
        let subsystem: String;

        let action_string = match action {
            KobjectAction::KOBJADD => "add",
            KobjectAction::KOBJREMOVE => "remove",
            KobjectAction::KOBJCHANGE => "change",
            KobjectAction::KOBJMOVE => "move",
            KobjectAction::KOBJONLINE => "online",
            KobjectAction::KOBJOFFLINE => "offline",
            KobjectAction::KOBJBIND => "bind",
            KobjectAction::KOBJUNBIND => "unbind",
        };


        let mut state = KObjectState::empty();

        match action {
            KobjectAction::KOBJREMOVE => {
                state.insert(KObjectState::REMOVE_UEVENT_SENT);
            },
            _ => {}
        }

        /* search the kset we belong to */
        //let top_kobj = kobj;
        let top_kobj = Arc::new(kobj); // assuming kobj is of type dyn KObject

        let weak_parent = Arc::downgrade(&top_kobj);

        while let Some(parent_arc) = weak_parent.upgrade() {
            let kset = top_kobj.kset();
            if !kset.is_some() {
                break;
            }
            top_kobj = parent_arc;
        }
        /*
        struct kset_uevent_ops {
            int (* const filter)(struct kobject *kobj);
            const char *(* const name)(struct kobject *kobj);
            int (* const uevent)(struct kobject *kobj, struct kobj_uevent_env *env);
        };
         */
        if top_kobj.kset().is_none() {
            if kset.uevent_ops().is_none() {
                kdebug!("kset has no uevent_ops");
            }
            if kset.uevent_ops().unwrap().filter().is_none() {
                kdebug!("kset uevent_ops has no filter");
            }
            if kset.uevent_ops().unwrap().filter().unwrap()(kobj, action) {
                return Ok(());
            }
        }
        let kset = top_kobj.kset().unwrap();
        let uevent_ops = kset.uevent_ops().unwrap();

        /* skip the event, if uevent_suppress is set*/
        /* 
        if (kobj->uevent_suppress) {
            pr_debug("kobject: '%s' (%p): %s: uevent_suppress "
                    "caused the event to drop!\n",
                    kobject_name(kobj), kobj, __func__);
            return 0;
        }
         */
        if UEVENT_SUPPRESS == 1 {
            kdebug!("uevent_suppress caused the event to drop!");
            return Ok(());
        }

        /* skip the event, if the filter returns zero. */
        if uevent_ops.filter.is_some() && uevent_ops.filter.unwrap()(kobj) {
            kdebug!("filter caused the event to drop!");
            return Ok(());
        }

        /* originating subsystem */
        if uevent_ops && uevent_ops.name {
            let subsystem = uevent_ops.name(kobj);
        }
        else {
            let subsystem = kset.name();
        }
        if subsystem.is_empty() {
            kdebug!("unset sussystem caused the event to drop!");
        }

        /* environment buffer */
        // env = kzalloc(sizeof(struct kobj_uevent_env), GFP_KERNEL);
        // if (!env)
        // 	return -ENOMEM;



        if let Some(env_ext) = envp_ext {
            for var in env_ext {
                // todo
            }
        }
    
    
        match action {
            KobjectAction::KOBJADD => {
                state.insert(KObjectState::ADD_UEVENT_SENT);
            },
            KobjectAction::KOBJUNBIND => {
                //zap_modalias_env(env);
            },
            _ => {}
        }
    
        // ... more code omitted ...
    
        Ok(())
    }