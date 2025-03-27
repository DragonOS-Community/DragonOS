use core::ffi::CStr;

#[linkage = "weak"]
#[no_mangle]
fn kallsyms_address() {}
#[linkage = "weak"]
#[no_mangle]
fn kallsyms_num() {}
#[linkage = "weak"]
#[no_mangle]
fn kallsyms_names_index() {}
#[linkage = "weak"]
#[no_mangle]
fn kallsyms_names() {}

pub unsafe fn lookup_kallsyms(addr: u64, level: i32) -> Option<()> {
    let sym_names = kallsyms_names as *const u8;
    // 由于符号表使用nm -n生成，因此是按照地址升序排列的，因此可以二分
    let sym_num = kallsyms_num as usize;
    let kallsyms_address_list =
        core::slice::from_raw_parts(kallsyms_address as *const u64, sym_num);
    let sym_names_index = kallsyms_names_index as *const u64;
    let sym_names_index = core::slice::from_raw_parts(sym_names_index, sym_num);
    let mut index = usize::MAX;
    for i in 0..sym_num - 1 {
        if addr > kallsyms_address_list[i] && addr <= kallsyms_address_list[i + 1] {
            index = i;
            break;
        }
    }
    return if index < sym_num {
        let sym_name = CStr::from_ptr(sym_names.add(sym_names_index[index] as usize) as _)
            .to_str()
            .unwrap();
        println!(
            "[{}] function:{}() \t(+) {:04} address:{:#018x}",
            level,
            sym_name,
            addr - kallsyms_address_list[index],
            addr
        );
        Some(())
    } else {
        None
    };
}
pub unsafe fn addr_from_symbol(symbol: &str) -> Option<u64> {
    let sym_num = kallsyms_num as usize;
    let sym_names = kallsyms_names as *const u8;
    let kallsyms_address_list =
        core::slice::from_raw_parts(kallsyms_address as *const u64, sym_num);
    let sym_names_index = kallsyms_names_index as *const u64;
    let sym_names_index_list = core::slice::from_raw_parts(sym_names_index, sym_num);
    for i in 0..sym_num {
        let sym_name_cstr = CStr::from_ptr(sym_names.add(sym_names_index_list[i] as usize) as _);
        let sym_name = sym_name_cstr.to_str().unwrap();
        if sym_name == symbol {
            return Some(kallsyms_address_list[i]);
        }
    }
    None
}
