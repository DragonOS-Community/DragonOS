#!/usr/bin/env python3
"""Run the real TCP port-table unit tests on the host.

Only kernel infrastructure is adapted: Mutex uses std, errno is an enum,
and unused process/random helpers have deterministic stand-ins. smoltcp and
hashbrown are the real dependencies. This checks admission/state transitions,
not kernel scheduling, IRQ safety, or syscall integration; use guest tests for
those. The generated Cargo project stays in a temporary directory.
"""

import json
from pathlib import Path
import subprocess
import sys
import tempfile


def main():
    root = Path(__file__).resolve().parents[1]
    port = root / "kernel/src/net/socket/inet/common/port.rs"
    smoltcp = root / "kernel/submodules/smoltcp"
    with tempfile.TemporaryDirectory(prefix="dragonos-tcp-port-") as temp:
        project = Path(temp)
        (project / "src").mkdir()
        (project / "Cargo.toml").write_text(
            '[package]\nname = "tcp-port-host-tests"\nversion = "0.1.0"\n'
            'edition = "2021"\n[dependencies]\nhashbrown = "=0.13.2"\n'
            f'smoltcp = {{ path = {json.dumps(str(smoltcp))}, '
            'default-features = false, features = ["std", "medium-ip", '
            '"proto-ipv4", "proto-ipv6", "socket-tcp"] }\n'
        )
        (project / "src/lib.rs").write_text(
            '''extern crate alloc;
extern crate self as system_error;
#[allow(non_camel_case_types)]
#[derive(Debug, PartialEq, Eq)]
pub enum SystemError { EADDRINUSE, EINVAL }
pub mod arch { pub mod rand { pub fn rand() -> usize { 19 } } }
pub mod libs { pub mod mutex {
    #[derive(Debug)]
    pub struct Mutex<T>(std::sync::Mutex<T>);
    impl<T> Mutex<T> {
        pub fn new(value: T) -> Self { Self(std::sync::Mutex::new(value)) }
        pub fn lock(&self) -> std::sync::MutexGuard<'_, T> { self.0.lock().unwrap() }
    }
} }
pub mod process {
    pub struct ProcessManager;
    impl ProcessManager {
        pub fn current_netns() -> Self { Self }
        pub fn local_port_range(&self) -> (u16, u16) { (32768, 60999) }
        pub fn set_local_port_range(&self, _: u16, _: u16) -> Result<(), crate::SystemError> { Ok(()) }
    }
}
#[derive(Debug)]
pub enum Types { Tcp, Udp, Other }
'''
            + f'#[path = {json.dumps(str(port))}]\npub mod port;\n'
        )
        return subprocess.run(
            ["cargo", "test", "--manifest-path", str(project / "Cargo.toml"), *sys.argv[1:]],
            cwd=root,
        ).returncode


if __name__ == "__main__":
    raise SystemExit(main())
