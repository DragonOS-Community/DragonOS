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
/// unsafe_define_event_trace!(
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
/// Define a trace event backed by the maskable-only text-patch backend.
///
/// # Safety
///
/// This is an unsafe declaration despite `macro_rules!` having no unsafe-call
/// syntax. Every invocation must document that the emitted branch site is not
/// reachable from NMI/MCE context.
#[macro_export]
macro_rules! unsafe_define_event_trace{
    (
        $name:ident,
        TP_system($system:ident),
        TP_PROTO($($arg:ident:$arg_type:ty),*),
        TP_STRUCT__entry{
            __string($dynamic:ident, $dynamic_value:expr),
            $($entry:ident:$entry_type:ty,)*
        },
        TP_fast_assign{$($assign:ident:$value:expr,)*},
        TP_ident($tp_ident:ident),
        TP_printk($fmt_expr: expr),
        TP_print_fmt($print_fmt: expr)
    ) => {
        paste::paste!{
            $crate::unsafe_define_maskable_static_key_false!([<__ $name _KEY>]);
            #[allow(non_upper_case_globals)]
            #[used]
            static [<__ $name>]: $crate::tracepoint::TracePoint = $crate::tracepoint::TracePoint::new(
                &[<__ $name _KEY>],
                stringify!($name),
                stringify!($system),
                [<trace_fmt_ $name>],
                [<trace_fmt_show $name>],
            );

            #[inline(always)]
            #[allow(non_snake_case)]
            pub fn [<trace_ $name>]($($arg:$arg_type),*) {
                if $crate::maskable_static_branch_unlikely!([<__ $name _KEY>]) {
                    let mut f = |trace_func: &$crate::tracepoint::TracePointFunc| {
                        let func = trace_func.func;
                        let data = trace_func.data.as_ref();
                        let func = unsafe {
                            core::mem::transmute::<
                                fn(),
                                fn(&(dyn core::any::Any + Send + Sync), $($arg_type),*)
                            >(func)
                        };
                        func(data $(,$arg)*);
                    };
                    [<__ $name>].callback_list(&mut f);
                }
            }

            #[allow(unused, non_snake_case)]
            pub fn [<register_trace_ $name>](
                func: fn(&(dyn core::any::Any + Send + Sync), $($arg_type),*),
                data: alloc::boxed::Box<dyn core::any::Any + Send + Sync>,
            ) {
                let func = unsafe {
                    core::mem::transmute::<
                        fn(&(dyn core::any::Any + Send + Sync), $($arg_type),*),
                        fn()
                    >(func)
                };
                [<__ $name>].register(func, data);
            }

            #[allow(unused, non_snake_case)]
            pub fn [<unregister_trace_ $name>](
                func: fn(&(dyn core::any::Any + Send + Sync), $($arg_type),*)
            ) {
                let func = unsafe {
                    core::mem::transmute::<
                        fn(&(dyn core::any::Any + Send + Sync), $($arg_type),*),
                        fn()
                    >(func)
                };
                [<__ $name>].unregister(func);
            }

            #[derive(Debug)]
            #[repr(C)]
            #[allow(non_snake_case, non_camel_case_types)]
            struct [<__ $name _TracePointMeta>] {
                trace_point: &'static $crate::tracepoint::TracePoint,
                print_func: fn(&(dyn core::any::Any + Send + Sync), $($arg_type),*),
            }

            #[allow(non_upper_case_globals)]
            #[link_section = ".tracepoint"]
            #[used]
            static [<__ $name _meta>]: [<__ $name _TracePointMeta>] = [<__ $name _TracePointMeta>] {
                trace_point: &[<__ $name>],
                print_func: [<trace_default_ $name>],
            };

            #[allow(unused, non_snake_case, clippy::redundant_field_names)]
            pub fn [<trace_default_ $name>](
                _data: &(dyn core::any::Any + Send + Sync),
                $($arg:$arg_type),*
            ) {
                fn align_up(value: usize, align: usize) -> Option<usize> {
                    value.checked_add(align.checked_sub(1)?)?.checked_div(align)?.checked_mul(align)
                }

                $(let $assign: $entry_type = $value;)*
                let dynamic_value: &[u8] = $dynamic_value;
                let Some(dynamic_len) = dynamic_value.len().checked_add(1) else {
                    return;
                };

                let mut fixed_len = core::mem::size_of::<$crate::tracepoint::TraceEntry>()
                    + core::mem::size_of::<u32>();
                let mut max_align = core::mem::align_of::<u32>();
                $(
                    let field_align = <$entry_type as $crate::tracepoint::TraceEventField>::ALIGN;
                    let Some(aligned) = align_up(fixed_len, field_align) else { return; };
                    let Some(next) = aligned.checked_add(
                        <$entry_type as $crate::tracepoint::TraceEventField>::SIZE
                    ) else { return; };
                    fixed_len = next;
                    max_align = max_align.max(field_align);
                )*
                let Some(fixed_len) = align_up(fixed_len, max_align) else { return; };
                let Ok(dynamic_offset) = u16::try_from(fixed_len) else { return; };
                let Ok(dynamic_len_u16) = u16::try_from(dynamic_len) else { return; };
                let Some(record_len) = fixed_len.checked_add(dynamic_len) else { return; };
                let data_loc = ((dynamic_len_u16 as u32) << 16) | dynamic_offset as u32;

                let process = $crate::process::ProcessManager::current_pcb();
                let common_pid = process.raw_pid().data() as i32;
                let mut event_buf = alloc::vec![0u8; fixed_len];
                event_buf[0..2].copy_from_slice(&([<__ $name>].id() as u16).to_ne_bytes());
                event_buf[2] = [<__ $name>].flags();
                event_buf[3] = 0;
                event_buf[4..8].copy_from_slice(&common_pid.to_ne_bytes());
                event_buf[8..12].copy_from_slice(&data_loc.to_ne_bytes());

                let mut field_offset = 12usize;
                $(
                    field_offset = align_up(
                        field_offset,
                        <$entry_type as $crate::tracepoint::TraceEventField>::ALIGN,
                    ).expect("validated trace field layout");
                    let field_end = field_offset
                        + <$entry_type as $crate::tracepoint::TraceEventField>::SIZE;
                    assert!(<$entry_type as $crate::tracepoint::TraceEventField>::write_ne(
                        $assign,
                        &mut event_buf[field_offset..field_end],
                    ));
                    field_offset = field_end;
                )*
                debug_assert_eq!(event_buf.len(), fixed_len);
                event_buf.reserve(record_len - fixed_len);
                event_buf.extend_from_slice(dynamic_value);
                event_buf.push(0);

                for callback in [<__ $name>].raw_callbacks_snapshot() {
                    callback.call(&event_buf);
                }

                if [<__ $name>].is_trace_pipe_enabled() {
                    $crate::debug::tracing::trace_cmdline_push(common_pid as u32);
                    $crate::debug::tracing::trace_pipe_push_raw_record(&event_buf);
                }
            }

            #[allow(unused, non_snake_case)]
            pub fn [<trace_fmt_ $name>](buf: &[u8]) -> alloc::string::String {
                fn align_up(value: usize, align: usize) -> Option<usize> {
                    value.checked_add(align.checked_sub(1)?)?.checked_div(align)?.checked_mul(align)
                }
                fn invalid() -> alloc::string::String {
                    alloc::string::String::from("<invalid trace record>")
                }

                if buf.len() < core::mem::size_of::<u32>() {
                    return invalid();
                }
                let data_loc = u32::from_ne_bytes(buf[0..4].try_into().unwrap());
                let full_offset = (data_loc & 0xffff) as usize;
                let dynamic_len = (data_loc >> 16) as usize;
                let Some(dynamic_offset) = full_offset
                    .checked_sub(core::mem::size_of::<$crate::tracepoint::TraceEntry>())
                else {
                    return invalid();
                };
                let Some(dynamic_end) = dynamic_offset.checked_add(dynamic_len) else {
                    return invalid();
                };
                let Some(dynamic_with_nul) = buf.get(dynamic_offset..dynamic_end) else {
                    return invalid();
                };
                if dynamic_with_nul.last() != Some(&0) {
                    return invalid();
                }
                let $dynamic = &dynamic_with_nul[..dynamic_with_nul.len() - 1];

                let mut field_offset = 4usize;
                $(
                    let Some(aligned) = align_up(
                        field_offset,
                        <$entry_type as $crate::tracepoint::TraceEventField>::ALIGN,
                    ) else { return invalid(); };
                    let Some(field_end) = aligned.checked_add(
                        <$entry_type as $crate::tracepoint::TraceEventField>::SIZE,
                    ) else { return invalid(); };
                    let Some(field_bytes) = buf.get(aligned..field_end) else {
                        return invalid();
                    };
                    let Some($entry) = <$entry_type as $crate::tracepoint::TraceEventField>::read_ne(field_bytes) else {
                        return invalid();
                    };
                    field_offset = field_end;
                )*

                struct Entry<'a> {
                    $dynamic: &'a [u8],
                    $($entry: $entry_type,)*
                }
                let $tp_ident = Entry {
                    $dynamic: $dynamic,
                    $($entry: $entry,)*
                };
                format!("{}", $fmt_expr)
            }

            #[allow(unused, non_snake_case)]
            pub fn [<trace_fmt_show $name>]() -> alloc::string::String {
                fn align_up(value: usize, align: usize) -> usize {
                    value.div_ceil(align) * align
                }

                let mut fmt = format!("format:
\tfield: u16 common_type; offset: 0; size: 2; signed: 0;
\tfield: u8 common_flags; offset: 2; size: 1; signed: 0;
\tfield: u8 common_preempt_count; offset: 3; size: 1; signed: 0;
\tfield: i32 common_pid; offset: 4; size: 4; signed: 1;

\tfield:__data_loc char[] {};\toffset:8;\tsize:4;\tsigned:1;\n",
                    stringify!($dynamic),
                );
                let mut offset = 12usize;
                $(
                    offset = align_up(
                        offset,
                        <$entry_type as $crate::tracepoint::TraceEventField>::ALIGN,
                    );
                    fmt.push_str(&format!(
                        "\tfield:{} {};\toffset:{};\tsize:{};\tsigned:{};\n",
                        <$entry_type as $crate::tracepoint::TraceEventField>::TYPE_NAME,
                        stringify!($entry),
                        offset,
                        <$entry_type as $crate::tracepoint::TraceEventField>::SIZE,
                        if <$entry_type as $crate::tracepoint::TraceEventField>::SIGNED { 1 } else { 0 },
                    ));
                    offset += <$entry_type as $crate::tracepoint::TraceEventField>::SIZE;
                )*
                fmt.push_str(&format!("\nprint fmt: {}", $print_fmt));
                fmt
            }
        }
    };
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
            $crate::unsafe_define_maskable_static_key_false!([<__ $name _KEY>]);
            #[allow(non_upper_case_globals)]
            #[used]
            static [<__ $name>]: $crate::tracepoint::TracePoint = $crate::tracepoint::TracePoint::new(&[<__ $name _KEY>],stringify!($name), stringify!($system),[<trace_fmt_ $name>], [<trace_fmt_show $name>]);

            #[inline(always)]
            #[allow(non_snake_case)]
            pub fn [<trace_ $name>]( $($arg:$arg_type),* ){
                if $crate::maskable_static_branch_unlikely!([<__ $name _KEY>]) {
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
                print_func: fn(&(dyn core::any::Any+Send+Sync), $($arg_type),*),
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
            pub fn [<trace_default_ $name>](_data:&(dyn core::any::Any+Send+Sync), $($arg:$arg_type),* ){
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
                let pid = process.raw_pid().data() as _;

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

                for callback in [<__ $name>].raw_callbacks_snapshot() {
                    callback.call(event_buf);
                }

                if [<__ $name>].is_trace_pipe_enabled() {
                    $crate::debug::tracing::trace_cmdline_push(pid as u32);
                    $crate::debug::tracing::trace_pipe_push_raw_record(event_buf);
                }
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
