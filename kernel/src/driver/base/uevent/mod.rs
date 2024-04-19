// include/linux/kobject.h
// lib/kobject_uevent.c
use crate::driver::base::kobject::KObject;
use alloc::string::String;
use alloc::vec::Vec;
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

// https://code.dragonos.org.cn/xref/linux-6.1.9/lib/kobject_uevent.c?fi=kobject_uevent#457
// kobject_action
pub enum KobjectAction {
        KOBJADD, 
        KOBJREMOVE, //Kobject（或上层数据结构）的添加/移除事件
        KOBJCHANGE, //Kobject（或上层数据结构）的状态或者内容发生改变; 如果设备驱动需要上报的事件不再上面事件的范围内，或者是自定义的事件，可以使用该event，并携带相应的参数。
        KOBJMOVE, //Kobject（或上层数据结构）更改名称或者更改Parent（意味着在sysfs中更改了目录结构）
        KOBJONLINE,
        KOBJOFFLINE, //Kobject（或上层数据结构）的上线/下线事件，其实是是否使能
        KOBJBIND,
        KOBJUNBIND,
    }
const UEVENT_NUM_ENVP: usize = 64;
const UEVENT_BUFFER_SIZE: usize = 2048;
const UEVENT_HELPER_PATH_LEN: usize = 256;
struct KobjUeventEnv {
argv: [Option<String>; 3],
envp: [Option<String>; UEVENT_NUM_ENVP],
envp_idx: i32,
buf: [char; UEVENT_BUFFER_SIZE],
buflen: i32,
}
    //kobject_uevent->kobject_uevent_env
    pub fn kobject_uevent(kobj: &mut dyn KObject, action: KobjectAction) -> Result<(), &'static str> {
        match kobject_uevent_env(kobj, action, None) {
            Ok(_) => Ok(()), // return Ok(()) on success
            Err(e) => Err(e), // return the error on failure
        }
    }
    pub fn kobject_uevent_env(kobj: &mut dyn KObject, action: KobjectAction, envp_ext: Option<Vec<String>>) -> Result<(), &'static str> {
        // maybe we can have a better way to handle this
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
        // failed to init because of the envp: [None; UEVENT_NUM_ENVP],
        // let mut env = KobjUeventEnv {
        //         argv: [None, None, None],
        //         envp: [None; UEVENT_NUM_ENVP],
        //         envp_idx: 0,
        //         buf: ['\0'; UEVENT_BUFFER_SIZE],
        //         buflen: 0,
        // };

        match action {
                KobjectAction::KOBJREMOVE => {
                        //TODO: kobj.state_remove_uevent_sent = true;
                },
                _ => {}
        }
    
        // ... more code omitted ...
    
        if let Some(env_ext) = envp_ext {
            for var in env_ext {
                // ... use var ...
            }
        }
    
        // ... more code omitted ...
    
        match action {
            KobjectAction::KOBJADD => {
                //kobj.state_add_uevent_sent = true;
            },
            KobjectAction::KOBJUNBIND => {
                // ... code omitted ...
            },
            _ => {}
        }
    
        // ... more code omitted ...
    
        Ok(())
    }