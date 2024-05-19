#[derive(Debug)]
pub struct Process {
    pid: usize,
    name: String,
    state: String,
    ppid: String,
    cpu_id: String,
    priority: String,
    preempt: String,
    vrtime: String,
    vmpeak: String,
    vmdata: String,
    vmexe: String,
    flags: String,
}
impl Process {
    pub fn new_from_pid(pid:usize) -> Result<Process,String> {
	let file : String = std::fs::read_to_string(format!("/proc/{}/status",pid)).expect(&format!("unable to read from /proc/{}/status !!",pid));
	let mut string: std::str::Lines = file.lines();
	let name: String = string.nth(0).unwrap().split(':').nth(1).unwrap().trim().to_string();
	let state: String = string.nth(0).unwrap().split(':').nth(1).unwrap().trim().to_string();
	let ppid = string.nth(1).unwrap().split(':').nth(1).unwrap().trim().to_string();
	let cpu_id = string.nth(0).unwrap().split(':').nth(1).unwrap().trim().to_string();
	let priority = string.nth(0).unwrap().split(':').nth(1).unwrap().trim().to_string();
	let preempt = string.nth(0).unwrap().split(':').nth(1).unwrap().trim().to_string();
	let vrtime = string.nth(0).unwrap().split(':').nth(1).unwrap().trim().to_string();
	let vmpeak = string.nth(0).unwrap().split(':').nth(1).unwrap().trim().to_string();
	let vmdata = string.nth(0).unwrap().split(':').nth(1).unwrap().trim().to_string();
	let vmexe = string.nth(0).unwrap().split(':').nth(1).unwrap().trim().to_string();
	let flags = string.nth(0).unwrap().split(':').nth(1).unwrap().trim().to_string();
	Ok(Process{pid: pid,name,state,ppid,cpu_id,priority,preempt,vrtime,vmpeak,vmdata,vmexe,flags})
    }
}

use std::path::Path;
fn main() {
    let path: &Path = Path::new("/proc");
    for entry in path.read_dir().expect("Unable To Read /proc!") {
	if let Ok(entry) = entry {
	    if let Ok(file_name) = entry.file_name().into_string() {
		if let Ok(pid) = file_name.parse::<usize>() { //解析pid 并排除proc下其他文件
		    if let Ok(process) = Process::new_from_pid(pid) {
			println!("{:?}",process)
		    }
		}
	    }
	}
    }

    
}

