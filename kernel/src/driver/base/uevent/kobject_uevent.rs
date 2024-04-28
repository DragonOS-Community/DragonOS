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
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use num::Zero;
use crate::driver::base::kobject::{KObjectManager, KObjectState, UEVENT_SUPPRESS};
use crate::net::socket::Socket;
use super::KobjectAction;
use super::KObject;
use super::KobjUeventEnv;
use crate::driver::base::kset::{KSet,KSetUeventOps};
use super::{UEVENT_NUM_ENVP,UEVENT_BUFFER_SIZE};
use crate::libs::mutex::Mutex;
use alloc::sync::Arc;
use alloc::sync::Weak;
use system_error::SystemError;
use crate::mm::c_adapter::{kfree, kzalloc};
use alloc::boxed::Box;

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
pub fn kobject_uevent(kobj: &dyn KObject, action: KobjectAction) -> Result<(), SystemError>  {
    // kobject_uevent和kobject_uevent_env功能一样，只是没有指定任何的环境变量
    match kobject_uevent_env(kobj, action, None) {
        Ok(_) => Ok(()), // return Ok(()) on success
        Err(e) => Err(e), // return the error on failure
    }
}
pub fn kobject_uevent_env(kobj: &dyn KObject, action: KobjectAction, envp_ext: Option<Vec<String>>) -> Result<i32, SystemError>  {
    
    // todo: 定义一些常量和变量
    // init uevent env
    // let env = KobjUeventEnv {
    //     argv: Vec::with_capacity(UEVENT_NUM_ENVP),
    //     envp: Vec::with_capacity(UEVENT_NUM_ENVP),
    //     envp_idx: 0,
    //     buf: Vec::with_capacity(UEVENT_BUFFER_SIZE),
    //     buflen: 0,
    // };
    
    //let mut kset = kobj.kset();
    let subsystem: String;
    let mut state = KObjectState::empty();
    let devpath: String;
    let mut top_kobj = kobj;
    let kset = kobj.kset();
    let mut retval: i32 = 0;
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
    match action {
        KobjectAction::KOBJREMOVE => {
            state.insert(KObjectState::REMOVE_UEVENT_SENT);
        },
        _ => {}
    }

    /* search the kset we belong to */
    
    // while let Some(weak_parent) = top_kobj.parent() {
    //     if let Some(strong_parent) = weak_parent.upgrade() {
    //         top_kobj = strong_parent.as_ref();
    //     }
    // }

    if top_kobj.kset().is_none() {
        kdebug!("attempted to send uevent without kset!\n");
        return Err(SystemError::EINVAL);
    } 

    

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
        return Ok(0);
    }
    /*
    struct kset_kset {
        int (* const filter)(struct kobject *kobj);
        const char *(* const name)(struct kobject *kobj);
        int (* const uevent)(struct kobject *kobj, struct kobj_uevent_env *env);
    };
    */


    /* skip the event, if the filter returns zero. */
    if kset.as_ref().unwrap().uevent_ops.is_some() && kset.as_ref().unwrap().uevent_ops.as_ref().unwrap().filter() == None {
        kdebug!("filter caused the event to drop!");
        return Ok(0);
    }

    /* originating subsystem */
    if  kset.as_ref().unwrap().uevent_ops.is_some() && kset.as_ref().unwrap().uevent_ops.as_ref().unwrap().uevent_name() != "" {
        subsystem = kset.as_ref().unwrap().uevent_ops.as_ref().unwrap().uevent_name();
    } else {
        subsystem = kobj.name();
    }
    if subsystem.is_empty() {
        kdebug!("unset sussystem caused the event to drop!");
    }

    /* environment buffer */
    // 创建一个用于环境变量的缓冲区

    // HELP_NEEDED: linux使用的是kzalloc，这里使用Box::new ？
    let env = Box::new(KobjUeventEnv {
            argv: Vec::with_capacity(UEVENT_NUM_ENVP),
            envp: Vec::with_capacity(UEVENT_NUM_ENVP),
            envp_idx: 0,
            buf: Vec::with_capacity(UEVENT_BUFFER_SIZE),
            buflen: 0,
        }) ;
    if env.buf.is_empty(){
        return Err(SystemError::ENOMEM);
    }
       

    //获取设备的完整对象路径
	/* complete object path */
	// devpath = kobject_get_path(kobj, GFP_KERNEL);
	// if (!devpath) {
	// 	retval = -ENOENT;
	// 	{};
	// }
    devpath = KObjectManager::kobject_get_path(kobj);
    if devpath.is_empty() {
        retval = SystemError::ENOENT.to_posix_errno();
        // goto exit
        drop(devpath);
        drop(env);
        return Ok(retval);
    }
    /*
    /* default keys */
	retval = add_uevent_var(env, "ACTION=%s", action_string);
	if retval
		{};
	retval = add_uevent_var(env, "DEVPATH=%s", devpath);
	if retval
		{};
	retval = add_uevent_var(env, "SUBSYSTEM=%s", subsystem);
	if retval
		{};
    */
    retval = add_uevent_var(&env, "ACTION=%s", &action_string).unwrap();
	if retval.is_zero(){
        // goto exit 
        // 这里的goto目标代码较少，暂时直接复制使用，不仿写goto逻辑
        // drop替代了kfree
        drop(devpath);
        drop(env);
        return Ok(retval);
        };
	retval = add_uevent_var(&env, "DEVPATH=%s", &devpath).unwrap();
	if retval.is_zero(){
        drop(devpath);
        drop(env);
        return Ok(retval);
    };
	retval = add_uevent_var(&env, "SUBSYSTEM=%s", &subsystem).unwrap();
	if retval.is_zero(){
        drop(devpath);
        drop(env);
        return Ok(retval);
    };
       
    /*
	/* keys passed in from the caller */
	if (envp_ext) {
		for (i = 0; envp_ext[i]; i++) {
			retval = add_uevent_var(env, "%s", envp_ext[i]);
			if retval
				{};
		}
	}
     */

    /* keys passed in from the caller */
    if let Some(env_ext) = envp_ext {
        for var in env_ext {
            // todo
            let retval = add_uevent_var(&env, "%s", &var).unwrap();
            if retval.is_zero(){
                // goto exit
                drop(devpath);
                drop(env);
                return Ok(retval);
            }
        }
    }
    if kset.as_ref().unwrap().uevent_ops.is_some() && kset.as_ref().unwrap().uevent_ops.as_ref().unwrap().uevent(&env) != 0 {
        retval = kset.as_ref().unwrap().uevent_ops.as_ref().unwrap().uevent(&env);
        if retval.is_zero(){
            kdebug!("kset uevent caused the event to drop!");
            // goto exit
            drop(devpath);
            drop(env);
            return Ok(retval);
        }
    }
    match action {
        KobjectAction::KOBJADD => {
            state.insert(KObjectState::ADD_UEVENT_SENT);
        },
        KobjectAction::KOBJUNBIND => {
            zap_modalias_env(&env);
        },
        _ => {}
    }

    /*
    mutex_lock(&uevent_sock_mutex);
	/* we will send an event, so request a new sequence number */
	retval = add_uevent_var(env, "SEQNUM=%llu", ++uevent_seqnum);
	if (retval) {
		mutex_unlock(&uevent_sock_mutex);
		goto exit;
	}
	retval = kobject_uevent_net_broadcast(kobj, env, action_string,
					      devpath);
	mutex_unlock(&uevent_sock_mutex);

#ifdef CONFIG_UEVENT_HELPER
	/* call uevent_helper, usually only enabled during early boot */
	if (uevent_helper[0] && !kobj_usermode_filter(kobj)) {
		struct subprocess_info *info;

		retval = add_uevent_var(env, "HOME=/");
		if (retval)
			goto exit;
		retval = add_uevent_var(env,
					"PATH=/sbin:/bin:/usr/sbin:/usr/bin");
		if (retval)
			goto exit;
		retval = init_uevent_argv(env, subsystem);
		if (retval)
			goto exit;

		retval = -ENOMEM;
		info = call_usermodehelper_setup(env->argv[0], env->argv,
						 env->envp, GFP_KERNEL,
						 NULL, cleanup_uevent_env, env);
		if (info) {
			retval = call_usermodehelper_exec(info, UMH_NO_WAIT);
			env = NULL;	/* freed by cleanup_uevent_env */
		}
	}
#endif

     */

    Ok(0)
}

pub fn add_uevent_var(env: &Box<KobjUeventEnv>, format: &str, args: &String) -> Result<i32, SystemError>{
    //todo
    // let len: usize;

    // if env.envp_idx >= env.envp.len() {
    //     println!("add_uevent_var: too many keys");
    //     return Err(SystemError::ENOMEM);
    // }

    // len = env.buf[env.buflen..].write_fmt(format, args).unwrap();

    // if len >= env.buf.len() - env.buflen {
    //     println!("add_uevent_var: buffer size too small");
    //     return Err(SystemError::ENOMEM);
    // }

    // env.envp[env.envp_idx] = &env.buf[env.buflen];
    // env.envp_idx += 1;
    // env.buflen += len + 1;

    Ok(0)
}

fn zap_modalias_env(env: &Box<KobjUeventEnv>)
{
    // todo
	// static const char modalias_prefix[] = "MODALIAS=";
	// size_t len;
	// int i, j;

	// for (i = 0; i < env->envp_idx;) {
	// 	if (strncmp(env->envp[i], modalias_prefix,
	// 		    sizeof(modalias_prefix) - 1)) {
	// 		i++;
	// 		continue;
	// 	}

	// 	len = strlen(env->envp[i]) + 1;

	// 	if (i != env->envp_idx - 1) {
	// 		memmove(env->envp[i], env->envp[i + 1],
	// 			env->buflen - len);

	// 		for (j = i; j < env->envp_idx - 1; j++)
	// 			env->envp[j] = env->envp[j + 1] - len;
	// 	}

	// 	env->envp_idx--;
	// 	env->buflen -= len;
	// }
}
