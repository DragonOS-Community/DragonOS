/// Define a tracepoint with the given parameters.
///
/// This macro generates a tracepoint with the specified name, arguments, entry structure, assignment logic, identifier, and print format.
/// # Parameters
/// - `name`: The name of the tracepoint.
/// - `TP_system`: The subsystem or system to which the tracepoint belongs.
/// - `TP_PROTO`: The prototype of the tracepoint function.
/// - `TP_STRUCT__entry`: The structure of the tracepoint entry.
/// - `TP_fast_assign`: The assignment logic for the tracepoint entry.
/// - `TP_ident`: The identifier for the tracepoint entry.
/// - `TP_printk`: The print format for the tracepoint.
///
/// # Example
/// ```rust
/// define_event_trace!(
///     TEST2,
///     TP_PROTO(a: u32, b: u32),
///     TP_STRUCT__entry{
///           a: u32,
///           b: u32,
///     },
///     TP_fast_assign{
///           a:a,
///           b:{
///             // do something with b
///             b
///           }
///     },
///     TP_ident(__entry),
///     TP_printk({
///           // do something with __entry
///           format!("Hello from tracepoint! a={}, b={}", __entry.a, __entry.b)
///     })
/// );
/// ```
#[macro_export]
macro_rules! define_event_trace{
    (
        $name:ident,
        TP_system($system:ident),
        TP_PROTO($($arg:ident:$arg_type:ty),*),
        TP_STRUCT__entry{$($entry:ident:$entry_type:ty,)*},
        TP_fast_assign{$($assign:ident:$value:expr,)*},
        TP_ident($tp_ident:ident),
        TP_printk($fmt_expr: expr)
    ) => {
        paste::paste!{
            static_keys::define_static_key_false!([<__ $name _KEY>]);
            #[allow(non_upper_case_globals)]
            #[used]
            static [<__ $name>]: $crate::tracepoint::TracePoint = $crate::tracepoint::TracePoint::new(&[<__ $name _KEY>],stringify!($name), stringify!($system),[<trace_fmt_ $name>], [<trace_fmt_show $name>]);

            #[inline(always)]
            #[allow(non_snake_case)]
            pub fn [<trace_ $name>]( $($arg:$arg_type),* ){
                if static_keys::static_branch_unlikely!([<__ $name _KEY>]){
                    let mut f = |trace_func: &$crate::tracepoint::TracePointFunc |{
                        let func = trace_func.func;
                        let data = trace_func.data.as_ref();
                        let func = unsafe{core::mem::transmute::<fn(),fn(& (dyn core::any::Any+Send+Sync), $($arg_type),*)>(func)};
                        func(data $(,$arg)*);
                    };
                    let trace_point = &[<__ $name>];
                    trace_point.callback_list(&mut f);
                }
            }
            #[allow(unused,non_snake_case)]
            pub fn [<register_trace_ $name>](func: fn(& (dyn core::any::Any+Send+Sync), $($arg_type),*), data: alloc::boxed::Box<dyn core::any::Any+Send+Sync>){
                let func = unsafe{core::mem::transmute::<fn(& (dyn core::any::Any+Send+Sync), $($arg_type),*), fn()>(func)};
                [<__ $name>].register(func,data);
            }
            #[allow(unused,non_snake_case)]
            pub fn [<unregister_trace_ $name>](func: fn(& (dyn core::any::Any+Send+Sync), $($arg_type),*)){
                let func = unsafe{core::mem::transmute::<fn(& (dyn core::any::Any+Send+Sync), $($arg_type),*), fn()>(func)};
                [<__ $name>].unregister(func);
            }


            #[derive(Debug)]
            #[repr(C)]
            #[allow(non_snake_case,non_camel_case_types)]
            struct [<__ $name _TracePointMeta>]{
                trace_point: &'static $crate::tracepoint::TracePoint,
                print_func: fn(&mut (dyn core::any::Any+Send+Sync), $($arg_type),*),
            }

            #[allow(non_upper_case_globals)]
            #[link_section = ".tracepoint"]
            #[used]
            static [<__ $name _meta>]: [<__ $name _TracePointMeta>] = [<__ $name _TracePointMeta>]{
                trace_point:& [<__ $name>],
                print_func:[<trace_default_ $name>],
            };

            #[allow(unused,non_snake_case)]
            #[allow(clippy::redundant_field_names)]
            pub fn [<trace_default_ $name>](_data:&mut (dyn core::any::Any+Send+Sync), $($arg:$arg_type),* ){
                #[repr(C, packed)]
                struct Entry {
                    $($entry: $entry_type,)*
                }
                #[repr(C, packed)]
                struct FullEntry {
                    common: $crate::tracepoint::TraceEntry,
                    entry: Entry,
                }

                let entry = Entry {
                    $($assign: $value,)*
                };

                let process = $crate::process::ProcessManager::current_pcb();
                let pid = process.pid().data() as _;

                let common = $crate::tracepoint::TraceEntry {
                    type_: [<__ $name>].id() as _,
                    flags: [<__ $name>].flags(),
                    preempt_count: 0,
                    pid,
                };

                let full_entry = FullEntry {
                    common,
                    entry,
                };

                let event_buf = unsafe {
                    core::slice::from_raw_parts(
                        &full_entry as *const FullEntry as *const u8,
                        core::mem::size_of::<FullEntry>(),
                    )
                };

                let func = |f:&alloc::boxed::Box<dyn $crate::tracepoint::TracePointCallBackFunc>|{
                    f.call(event_buf);
                };

                [<__ $name>].raw_callback_list(&func);

                $crate::debug::tracing::trace_cmdline_push(pid as u32);
                $crate::debug::tracing::trace_pipe_push_raw_record(event_buf);
            }

            #[allow(unused,non_snake_case)]
            pub fn [<trace_fmt_ $name>](buf: &[u8]) -> alloc::string::String {
                #[repr(C)]
                struct Entry {
                    $($entry: $entry_type,)*
                }
                let $tp_ident = unsafe {
                    &*(buf.as_ptr() as *const Entry)
                };
                let fmt = format!("{}", $fmt_expr);
                fmt
            }

            #[allow(unused,non_snake_case)]
            pub fn [<trace_fmt_show $name>]()-> alloc::string::String {
                let mut fmt = format!("format:
\tfield: u16 common_type; offset: 0; size: 2; signed: 0;
\tfield: u8 common_flags; offset: 2; size: 1; signed: 0;
\tfield: u8 common_preempt_count; offset: 3; size: 1; signed: 0;
\tfield: i32 common_pid; offset: 4; size: 4; signed: 1;

");
                fn is_signed<T>() -> bool {
                    match core::any::type_name::<T>() {
                        "i8" | "i16" | "i32" | "i64" | "i128" | "isize" => true,
                        _ => false,
                    }
                }
                let mut offset = 8;
                $(
                    fmt.push_str(&format!("\tfield: {} {} offset: {}; size: {}; signed: {};\n",
                        stringify!($entry_type), stringify!($entry), offset, core::mem::size_of::<$entry_type>(), if is_signed::<$entry_type>() { 1 } else { 0 }));
                    offset += core::mem::size_of::<$entry_type>();
                )*
                fmt.push_str(&format!("\nprint fmt: \"{}\"", stringify!($fmt_expr)));
                fmt
            }
        }
    };
}
