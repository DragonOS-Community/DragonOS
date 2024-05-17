use self::filewriter::FileWriter;
use crate::check::GroupInfo;

mod filewriter;

pub struct Executor;

impl Executor {
    pub fn execute(group_info: GroupInfo) {
        // 写入文件
        let writer = FileWriter::new(group_info);
        writer.write();
    }
}
