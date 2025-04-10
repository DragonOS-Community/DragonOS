use std::str;

#[derive(Debug, Clone)]
struct KernelSymbolEntry {
    vaddr: u64,
    #[allow(dead_code)]
    symbol_type: char,
    symbol: String,
    symbol_length: usize,
}

fn symbol_to_write(vaddr: u64, text_vaddr: u64, etext_vaddr: u64) -> bool {
    vaddr >= text_vaddr && vaddr <= etext_vaddr
}
fn read_symbol(line: &str) -> Option<KernelSymbolEntry> {
    if line.len() > 512 {
        return None;
    } // skip line with length >= 512
    let mut parts = line.split_whitespace();
    let vaddr = u64::from_str_radix(parts.next()?, 16).ok()?;
    let symbol_type = parts.next()?.chars().next()?;
    let symbol = parts.collect::<Vec<_>>().join(" ");
    if symbol_type != 'T' && symbol_type != 't' {
        return None;
    } // local symbol or global symbol in text section
    if symbol == "$x" {
        return None;
    } // skip $x symbol
    let symbol_length = symbol.len() + 1; // +1 for null terminator
    Some(KernelSymbolEntry {
        vaddr,
        symbol_type,
        symbol,
        symbol_length,
    })
}

fn read_map() -> (Vec<KernelSymbolEntry>, u64, u64) {
    let mut symbol_table = Vec::new();
    let mut text_vaddr = 0;
    let mut etext_vaddr = 0;
    let mut line = String::new();
    loop {
        let size = std::io::stdin().read_line(&mut line).unwrap();
        if size == 0 {
            break;
        }
        line = line.trim().to_string();
        if let Some(entry) = read_symbol(&line) {
            if entry.symbol.starts_with("_text") {
                text_vaddr = entry.vaddr;
            } else if entry.symbol.starts_with("_etext") {
                etext_vaddr = entry.vaddr;
            }
            symbol_table.push(entry);
        }
        line.clear();
    }
    (symbol_table, text_vaddr, etext_vaddr)
}

fn generate_result(symbol_table: &[KernelSymbolEntry], text_vaddr: u64, etext_vaddr: u64) {
    println!(".section .rodata\n");
    println!(".global kallsyms_address");
    println!(".align 8\n");
    println!("kallsyms_address:");

    let mut last_vaddr = 0;
    let mut total_syms_to_write = 0;

    for entry in symbol_table {
        if !symbol_to_write(entry.vaddr, text_vaddr, etext_vaddr) || entry.vaddr == last_vaddr {
            continue;
        }

        println!("\t.quad\t{:#x}", entry.vaddr);
        total_syms_to_write += 1;
        last_vaddr = entry.vaddr;
    }

    println!("\n.global kallsyms_num");
    println!(".align 8");
    println!("kallsyms_num:");
    println!("\t.quad\t{}", total_syms_to_write);

    println!("\n.global kallsyms_names_index");
    println!(".align 8");
    println!("kallsyms_names_index:");

    let mut position = 0;
    last_vaddr = 0;

    for entry in symbol_table {
        if !symbol_to_write(entry.vaddr, text_vaddr, etext_vaddr) || entry.vaddr == last_vaddr {
            continue;
        }

        println!("\t.quad\t{}", position);
        position += entry.symbol_length;
        last_vaddr = entry.vaddr;
    }

    println!("\n.global kallsyms_names");
    println!(".align 8");
    println!("kallsyms_names:");

    last_vaddr = 0;

    for entry in symbol_table {
        if !symbol_to_write(entry.vaddr, text_vaddr, etext_vaddr) || entry.vaddr == last_vaddr {
            continue;
        }

        println!("\t.asciz\t\"{}\"", entry.symbol);
        last_vaddr = entry.vaddr;
    }
}

fn main() {
    let (symbol_table, text_vaddr, etext_vaddr) = read_map();
    generate_result(&symbol_table, text_vaddr, etext_vaddr);
}
