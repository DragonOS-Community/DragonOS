use libc::syscall;

/// SysInfo 结构体，对应 Linux 的 sysinfo 系统调用
/// 参考: include/uapi/linux/sysinfo.h
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct SysInfo {
    /// 自启动以来的秒数
    pub uptime: i64,
    /// 1, 5, 15 分钟负载平均值
    pub loads: [u64; 3],
    /// 总可用主内存大小（单位取决于 mem_unit）
    pub totalram: u64,
    /// 可用内存大小（单位取决于 mem_unit）
    pub freeram: u64,
    /// 共享内存量（单位取决于 mem_unit）
    pub sharedram: u64,
    /// 缓冲区使用的内存（单位取决于 mem_unit）
    pub bufferram: u64,
    /// 总交换空间大小（单位取决于 mem_unit）
    pub totalswap: u64,
    /// 仍可用的交换空间（单位取决于 mem_unit）
    pub freeswap: u64,
    /// 当前进程数量
    pub procs: u16,
    /// 填充字段
    pub pad: u16,
    /// 总高端内存大小（单位取决于 mem_unit）
    pub totalhigh: u64,
    /// 可用高端内存大小（单位取决于 mem_unit）
    pub freehigh: u64,
    /// 内存单元大小（字节）
    pub mem_unit: u32,
}

/// 测试 totalram 值是否合理
fn test_totalram_sane_value(info: &SysInfo) -> Result<(), String> {
    // totalram 应该大于 0
    if info.totalram == 0 {
        return Err("totalram == 0, should be > 0".to_string());
    }

    // 计算实际总内存（字节）
    let totalram_bytes = info.totalram * info.mem_unit as u64;

    // 总内存应该至少有 1MB（合理的最小值）
    let min_memory = 1024 * 1024; // 1MB
    if totalram_bytes < min_memory {
        return Err(format!(
            "totalram {} bytes < {} bytes (1MB), too small",
            totalram_bytes, min_memory
        ));
    }

    // 总内存不应该超过 1PB（合理的最大值，防止溢出）
    let max_memory = 1024u64 * 1024 * 1024 * 1024; // 1PB
    if totalram_bytes > max_memory {
        return Err(format!(
            "totalram {} bytes > {} bytes (1PB), too large",
            totalram_bytes, max_memory
        ));
    }

    Ok(())
}

/// 测试 uptime 值是否合理
fn test_uptime_sane_value(info: &SysInfo) -> Result<(), String> {
    // uptime 应该大于等于 0
    if info.uptime < 0 {
        return Err(format!("uptime {} < 0, should be >= 0", info.uptime));
    }

    // uptime 应该是合理的（不超过 100 年）
    let max_uptime_seconds = 100i64 * 365 * 24 * 3600;
    if info.uptime > max_uptime_seconds {
        return Err(format!(
            "uptime {} seconds > {} seconds (100 years), too large",
            info.uptime, max_uptime_seconds
        ));
    }

    Ok(())
}

/// 测试 freeram 值是否合理
fn test_freeram_sane_value(info: &SysInfo) -> Result<(), String> {
    // freeram 应该大于 0
    if info.freeram == 0 {
        return Err("freeram == 0, should be > 0".to_string());
    }

    // 计算实际空闲内存（字节）
    let freeram_bytes = info.freeram * info.mem_unit as u64;
    let totalram_bytes = info.totalram * info.mem_unit as u64;

    // 空闲内存不应该超过总内存
    if freeram_bytes > totalram_bytes {
        return Err(format!(
            "freeram {} bytes > totalram {} bytes, should be <= totalram",
            freeram_bytes, totalram_bytes
        ));
    }

    Ok(())
}

/// 测试 procs 值是否合理
fn test_procs_sane_value(info: &SysInfo) -> Result<(), String> {
    // procs 应该大于 0（至少有当前进程）
    if info.procs == 0 {
        return Err("procs == 0, should be > 0".to_string());
    }

    // 进程数应该在合理范围内（不超过 65535）
    // 这是 u16 的最大值，也是合理的上限
    if info.procs > 10000 {
        // 某些系统可能有大量进程，但 10000 是一个合理的上限
        // 如果超过这个值，发出警告但不一定失败
        eprintln!("[warning] procs {} is unusually high (> 10000)", info.procs);
    }

    Ok(())
}

fn main() {
    let mut info: SysInfo = unsafe { std::mem::zeroed() };

    // 调用 sysinfo 系统调用
    // SYS_sysinfo 系统调用号为 99
    let result = unsafe { syscall(99, &mut info as *mut SysInfo) };

    if result != -1 {
        println!("sysinfo succeeded:");
        println!("  uptime: {} seconds", info.uptime);
        println!(
            "  loads: [{}, {}, {}]",
            info.loads[0], info.loads[1], info.loads[2]
        );
        println!(
            "  totalram: {} (mem_unit: {})",
            info.totalram, info.mem_unit
        );
        println!("  freeram: {} (mem_unit: {})", info.freeram, info.mem_unit);
        println!("  procs: {}", info.procs);
        println!();

        // 运行各项测试
        let mut passed = 0;
        let mut failed = 0;

        // 测试 1: totalram 值是否合理
        if let Err(e) = test_totalram_sane_value(&info) {
            println!("[fault] TotalRamSaneValue: {}", e);
            failed += 1;
        } else {
            println!("[success] TotalRamSaneValue");
            passed += 1;
        }

        // 测试 2: uptime 值是否合理
        if let Err(e) = test_uptime_sane_value(&info) {
            println!("[fault] UptimeSaneValue: {}", e);
            failed += 1;
        } else {
            println!("[success] UptimeSaneValue");
            passed += 1;
        }

        // 测试 3: freeram 值是否合理
        if let Err(e) = test_freeram_sane_value(&info) {
            println!("[fault] FreeRamSaneValue: {}", e);
            failed += 1;
        } else {
            println!("[success] FreeRamSaneValue");
            passed += 1;
        }

        // 测试 4: procs 值是否合理
        if let Err(e) = test_procs_sane_value(&info) {
            println!("[fault] NumProcsSaneValue: {}", e);
            failed += 1;
        } else {
            println!("[success] NumProcsSaneValue");
            passed += 1;
        }

        // 输出总结
        println!();
        println!("=================================");
        println!(
            "Total: {} tests, {} passed, {} failed",
            passed + failed,
            passed,
            failed
        );
        println!("=================================");

        if failed > 0 {
            std::process::exit(1);
        }
    } else {
        eprintln!("sysinfo failed: syscall returned -1");
        std::process::exit(1);
    }
}
