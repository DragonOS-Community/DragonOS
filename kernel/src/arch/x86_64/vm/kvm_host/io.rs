use system_error::SystemError;

use crate::{
    arch::{
        vm::{
            kvm_host::{vcpu::X86VcpuArch, KvmReg},
            uapi::{kvm_exit::KVM_EXIT_IO, KVM_PIO_PAGE_OFFSET},
        },
        MMArch,
    },
    kwarn,
    mm::MemoryManagementArch,
    virt::vm::user_api::{UapiKvmRun, KVM_EXIT_IO_IN, KVM_EXIT_IO_OUT},
};

#[derive(Debug)]
pub struct KvmPioRequest {
    pub linear_rip: usize,
    pub count: usize,
    pub is_in: bool,
    pub port: u16,
    pub size: u32,
}

impl X86VcpuArch {
    pub fn kvm_fast_pio(
        &mut self,
        run: &mut UapiKvmRun,
        size: u32,
        port: u16,
        is_in: bool,
    ) -> Result<bool, SystemError> {
        let ret = if is_in {
            self.kvm_fast_pio_in(size, port);
        } else {
            self.kvm_fast_pio_out(run, size, port);
        };

        todo!();
    }

    fn kvm_fast_pio_in(&self, size: u32, port: u16) {
        let val = if size < 4 {
            self.read_reg(KvmReg::VcpuRegsRax)
        } else {
            0
        };
        todo!()
    }

    fn kvm_fast_pio_out(&mut self, run: &mut UapiKvmRun, size: u32, port: u16) {
        let val = self.read_reg(KvmReg::VcpuRegsRax) as usize;

        let data = unsafe {
            core::slice::from_raw_parts_mut(
                &mut (val as u8) as *mut u8,
                core::mem::size_of_val(&val),
            )
        };
        if self.emulator_pio_in_out(run, size, port, data, 1, false) {
            return;
        }

        todo!()
    }

    // 返回值 -》 true： 用户态io， false： apic等io
    fn emulator_pio_in_out(
        &mut self,
        run: &mut UapiKvmRun,
        size: u32,
        port: u16,
        data: &mut [u8],
        count: u32,
        is_in: bool,
    ) -> bool {
        if self.pio.count != 0 {
            kwarn!("emulator_pio_in_out: self.pio.count != 0, check!");
        }

        for i in 0..count {
            let r: bool = if is_in {
                // 暂时
                false
            } else {
                // 暂时
                false
            };

            if !r {
                if i == 0 {
                    // 第一个就失败，说明不是内部端口，采用用户空间io处理
                    self.pio.port = port;
                    self.pio.is_in = is_in;
                    self.pio.count = count as usize;
                    self.pio.size = size;

                    if is_in {
                        self.pio_data[0..(size * count) as usize].fill(0);
                    } else {
                        self.pio_data[0..(size * count) as usize]
                            .copy_from_slice(&data[0..(size * count) as usize]);
                    }
                    run.exit_reason = KVM_EXIT_IO;
                    unsafe {
                        let io = &mut run.__bindgen_anon_1.io;
                        io.direction = if is_in {
                            KVM_EXIT_IO_IN
                        } else {
                            KVM_EXIT_IO_OUT
                        };
                        io.size = size as u8;
                        io.data_offset = KVM_PIO_PAGE_OFFSET * MMArch::PAGE_SIZE as u64;
                        io.count = count;
                        io.port = port;
                    }
                    return true;
                }

                if is_in {
                    self.pio_data[0..(size * (count - i)) as usize].fill(0);
                }
                break;
            }
        }

        return false;
    }
}
