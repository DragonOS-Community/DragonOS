use crate::libs::spinlock::{SpinLock, SpinLockGuard};
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use core::any::Any;
use core::fmt::Debug;
use static_keys::StaticFalseKey;

pub struct TracePoint {
    name: &'static str,
    module_path: &'static str,
    key: &'static StaticFalseKey,
    register: Option<fn()>,
    unregister: Option<fn()>,
    callback: SpinLock<BTreeMap<usize, TracePointFunc>>,
}

impl Debug for TracePoint {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TracePoint")
            .field("name", &self.name)
            .finish()
    }
}

#[derive(Debug)]
#[repr(C)]
pub struct CommonTracePointMeta {
    pub trace_point: &'static TracePoint,
    pub print_func: fn(),
}

#[derive(Debug)]
pub struct TracePointFunc {
    pub func: fn(),
    pub data: Box<dyn Any + Send + Sync>,
}

impl TracePoint {
    pub const fn new(
        key: &'static StaticFalseKey,
        name: &'static str,
        module_path: &'static str,
        register: Option<fn()>,
        unregister: Option<fn()>,
    ) -> Self {
        Self {
            name,
            module_path,
            key,
            register,
            unregister,
            callback: SpinLock::new(BTreeMap::new()),
        }
    }

    pub fn name(&self) -> &'static str {
        self.name
    }

    pub fn module_path(&self) -> &'static str {
        self.module_path
    }

    /// Register a callback function to the tracepoint
    pub fn register(&self, func: fn(), data: Box<dyn Any + Sync + Send>) {
        let trace_point_func = TracePointFunc { func, data };
        let mut funcs = self.callback.lock();
        if let Some(register) = self.register {
            register();
        }
        let ptr = func as usize;
        funcs.entry(ptr).or_insert(trace_point_func);
    }

    /// Unregister a callback function from the tracepoint
    pub fn unregister(&self, func: fn()) {
        let mut funcs = self.callback.lock();
        if let Some(unregister) = self.unregister {
            unregister();
        }
        let func_ptr = func as usize;
        funcs.remove(&func_ptr);
    }

    /// Get the callback list
    pub fn callback_list(&self) -> SpinLockGuard<BTreeMap<usize, TracePointFunc>> {
        self.callback.lock()
    }

    /// Enable the tracepoint
    pub fn enable(&self) {
        unsafe {
            self.key.enable();
        }
    }

    /// Disable the tracepoint
    pub fn disable(&self) {
        unsafe {
            self.key.disable();
        }
    }

    /// Check if the tracepoint is enabled
    pub fn is_enabled(&self) -> bool {
        self.key.is_enabled()
    }
}

/// Define a tracepoint
///
/// User should call register_trace_\$name to register a callback function to the tracepoint and
/// call trace_\$name to trigger the callback function
#[macro_export]
macro_rules! define_trace_point {
    ($name:ident $(,$arg:ident:$arg_type:ty),*) => {
        paste::paste!{
            static_keys::define_static_key_false!([<__ $name _KEY>]);
            #[allow(non_upper_case_globals)]
            #[used]
            static [<__ $name>]: $crate::debug::tracing::tracepoint::TracePoint = $crate::debug::tracing::tracepoint::TracePoint::new(&[<__ $name _KEY>],stringify!($name), module_path!(),None,None);

            #[inline(always)]
            #[allow(non_snake_case)]
            pub fn [<TRACE_ $name>]( $($arg:$arg_type),* ){

                if static_keys::static_branch_unlikely!([<__ $name _KEY>]){
                    let mut funcs = [<__ $name>].callback_list();
                    for trace_func in funcs.values_mut(){
                        let func = trace_func.func;
                        let data = trace_func.data.as_mut();
                        let func = unsafe{core::mem::transmute::<fn(),fn(&mut (dyn core::any::Any+Send+Sync),$($arg_type),*)>(func)};
                        func(data $(,$arg)*);
                    }
                }

            }

            #[allow(unused,non_snake_case)]
            pub fn [<register_trace_ $name>](func:fn(&mut (dyn core::any::Any+Send+Sync),$($arg_type),*),data:alloc::boxed::Box<dyn core::any::Any+Send+Sync>){
                let func = unsafe{core::mem::transmute::<fn(&mut (dyn core::any::Any+Send+Sync),$($arg_type),*),fn()>(func)};
                [<__ $name>].register(func,data);
            }

            #[allow(unused,non_snake_case)]
            pub fn [<unregister_trace_ $name>](func:fn(&mut (dyn core::any::Any+Send+Sync),$($arg_type),*)){
                let func = unsafe{core::mem::transmute::<fn(&mut (dyn core::any::Any+Send+Sync),$($arg_type),*),fn()>(func)};
                [<__ $name>].unregister(func);
            }

        }
    };
}

#[macro_export]
macro_rules! define_event_trace{
    ($name:ident,
        ($($arg:ident:$arg_type:ty),*),
        $fmt:expr) =>{
        define_trace_point!($name $(,$arg:$arg_type),*);
        paste::paste!{
            #[derive(Debug)]
            #[repr(C)]
            #[allow(non_snake_case)]

            struct [<__ $name _TracePointMeta>]{
                trace_point: &'static $crate::debug::tracing::tracepoint::TracePoint,
                print_func: fn(&mut (dyn core::any::Any+Send+Sync),$($arg_type),*),
            }
             #[allow(non_upper_case_globals)]
             #[link_section = ".tracepoint"]
             #[used]
            static [<__ $name _meta>]: [<__ $name _TracePointMeta>] = [<__ $name _TracePointMeta>]{
                trace_point:&[<__ $name>],
                print_func:[<TRACE_PRINT_ $name>],
            };
            #[allow(non_snake_case)]
            pub fn [<TRACE_PRINT_ $name>](_data:&mut (dyn core::any::Any+Send+Sync),$($arg:$arg_type),* ){
                 let time = $crate::time::Instant::now();
                 let cpu_id = $crate::arch::cpu::current_cpu_id().data();
                 let current_pid = $crate::process::ProcessManager::current_pcb().pid().data();
                 let format = format!("[{}][{}][{}] {}\n",time,cpu_id,current_pid,$fmt);
                 $crate::debug::tracing::trace_pipe::trace_pipe_push_record(format);
            }
        }
    };
}
