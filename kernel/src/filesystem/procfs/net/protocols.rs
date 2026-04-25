//! /proc/net/protocols - socket protocol 列表
//!
//! Linux 6.6: net/core/sock.c
//! 该文件列出内核注册的 `struct proto` 信息。
//! DragonOS 目前没有 1:1 的 proto_list，采用“静态能力表 + 可扩展注册点”的方式提供
//! 兼容的输出格式。

use crate::filesystem::{
    procfs::{
        template::{Builder, FileOps, ProcFileBuilder},
        utils::proc_read,
    },
    vfs::{FilePrivateData, IndexNode, InodeMode},
};
use crate::libs::mutex::MutexGuard;
use alloc::{format, sync::Arc, sync::Weak, vec::Vec};
use system_error::SystemError;

#[derive(Debug, Clone, Copy)]
struct ProtoRow {
    name: &'static str,
    obj_size: u32,
    sockets: i32,
    memory: i64,
    press: &'static str,
    maxhdr: u32,
    slab: &'static str,
    module: &'static str,
    methods: [char; 18],
}

impl ProtoRow {
    const fn new(name: &'static str, obj_size: u32) -> Self {
        Self {
            name,
            obj_size,
            sockets: 0,
            memory: -1,
            press: "NI",
            maxhdr: 0,
            slab: "no",
            module: "kernel",
            // cl co di ac io in de sh ss gs se re bi br ha uh gp em
            // DragonOS 当前仅保证对外接口可用；这里保守标注少数常用能力。
            methods: [
                'y', 'y', 'n', 'n', 'n', 'n', 'n', 'y', 'y', 'y', 'y', 'y', 'y', 'n', 'n', 'n',
                'n', 'n',
            ],
        }
    }

    fn push_to(&self, out: &mut Vec<u8>) {
        let line = format!(
            "{:<9} {:>4} {:>6}  {:>6}   {:<3} {:>6}   {:<3}  {:<10} {:>2} {:>2} {:>2} {:>2} {:>2} {:>2} {:>2} {:>2} {:>2} {:>2} {:>2} {:>2} {:>2} {:>2} {:>2} {:>2} {:>2} {:>2}\n",
            self.name,
            self.obj_size,
            self.sockets,
            self.memory,
            self.press,
            self.maxhdr,
            self.slab,
            self.module,
            self.methods[0],
            self.methods[1],
            self.methods[2],
            self.methods[3],
            self.methods[4],
            self.methods[5],
            self.methods[6],
            self.methods[7],
            self.methods[8],
            self.methods[9],
            self.methods[10],
            self.methods[11],
            self.methods[12],
            self.methods[13],
            self.methods[14],
            self.methods[15],
            self.methods[16],
            self.methods[17]
        );
        out.extend_from_slice(line.as_bytes());
    }
}

/// /proc/net/protocols 文件的 FileOps 实现
#[derive(Debug)]
pub struct ProtocolsFileOps;

impl ProtocolsFileOps {
    pub fn new_inode(parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(Self, InodeMode::S_IRUGO)
            .parent(parent)
            .build()
            .unwrap()
    }

    fn generate_protocols_content() -> Vec<u8> {
        let mut out: Vec<u8> = Vec::new();

        // Linux header:
        // "protocol  size sockets  memory press maxhdr  slab module cl co di ac io in de sh ss gs se re bi br ha uh gp em\n"
        out.extend_from_slice(
            format!(
                "{:<9} {:<4} {:<8} {:<6} {:<5} {:<7} {:<4} {:<10} {}",
                "protocol",
                "size",
                "sockets",
                "memory",
                "press",
                "maxhdr",
                "slab",
                "module",
                "cl co di ac io in de sh ss gs se re bi br ha uh gp em\n"
            )
            .as_bytes(),
        );

        // 目前以“内核支持的 socket 类型”构造静态列表。
        // 后续可以在 socket 子系统引入真正的 proto 注册表并在此处枚举。
        let rows: [ProtoRow; 5] = [
            ProtoRow::new(
                "TCP",
                core::mem::size_of::<crate::net::socket::inet::TcpSocket>() as u32,
            ),
            ProtoRow::new(
                "UDP",
                core::mem::size_of::<crate::net::socket::inet::UdpSocket>() as u32,
            ),
            ProtoRow::new(
                "RAW",
                core::mem::size_of::<crate::net::socket::inet::RawSocket>() as u32,
            ),
            ProtoRow::new(
                "UNIX",
                core::mem::size_of::<crate::net::socket::unix::stream::UnixStreamSocket>() as u32,
            ),
            ProtoRow::new(
                "PACKET",
                core::mem::size_of::<crate::net::socket::packet::PacketSocket>() as u32,
            ),
        ];

        for row in rows.iter() {
            row.push_to(&mut out);
        }

        out
    }
}

impl FileOps for ProtocolsFileOps {
    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let content = Self::generate_protocols_content();
        proc_read(offset, len, buf, &content)
    }
}
