// SPDX-License-Identifier: (Apache-2.0 OR MIT)

use alloc::{collections::BTreeMap, format, vec, vec::Vec};
use core::{mem, mem::ManuallyDrop};
use std::io::ErrorKind;

use cranelift_codegen::{
    entity::EntityRef,
    ir::{
        condcodes::IntCC,
        types::{I16, I32, I64, I8},
        AbiParam, Block, Endianness, FuncRef, Function, InstBuilder, MemFlags, Signature,
        SourceLoc, StackSlotData, StackSlotKind, TrapCode, Type, UserFuncName, Value,
    },
    isa::OwnedTargetIsa,
    settings::{self, Configurable},
};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{FuncId, Linkage, Module};

use super::{Error, HashMap, HashSet};
use crate::ebpf::{
    self, Insn, BPF_ALU_OP_MASK, BPF_IND, BPF_JEQ, BPF_JGE, BPF_JGT, BPF_JLE, BPF_JLT, BPF_JMP32,
    BPF_JNE, BPF_JSET, BPF_JSGE, BPF_JSGT, BPF_JSLE, BPF_JSLT, BPF_X, STACK_SIZE,
};

pub type JittedFunction = extern "C" fn(
    *mut u8, // mem_ptr
    usize,   // mem_len
    *mut u8, // mbuff_ptr
    usize,   // mbuff_len
) -> u64;

pub(crate) struct CraneliftCompiler {
    isa: OwnedTargetIsa,
    module: JITModule,

    helpers: HashMap<u32, ebpf::Helper>,
    helper_func_refs: HashMap<u32, FuncRef>,

    /// List of blocks corresponding to each instruction.
    /// We only store the first instruction that observes a new block
    insn_blocks: BTreeMap<u32, Block>,
    /// Map of block targets for each jump/branching instruction.
    insn_targets: BTreeMap<u32, (Block, Block)>,
    filled_blocks: HashSet<Block>,

    /// Map of register numbers to Cranelift variables.
    registers: [Variable; 11],
    /// Other usefull variables used throughout the program.
    mem_start: Variable,
    mem_end: Variable,
    mbuf_start: Variable,
    mbuf_end: Variable,
    stack_start: Variable,
    stack_end: Variable,
}

impl CraneliftCompiler {
    pub(crate) fn new(helpers: HashMap<u32, ebpf::Helper>) -> Self {
        let mut flag_builder = settings::builder();

        flag_builder.set("opt_level", "speed").unwrap();

        // Enable stack probes
        flag_builder.enable("enable_probestack").unwrap();
        flag_builder.set("probestack_strategy", "inline").unwrap();

        let isa_builder = cranelift_native::builder().unwrap_or_else(|msg| {
            panic!("host machine is not supported: {}", msg);
        });
        let isa = isa_builder
            .finish(settings::Flags::new(flag_builder))
            .unwrap();

        let mut jit_builder =
            JITBuilder::with_isa(isa.clone(), cranelift_module::default_libcall_names());
        // Register all the helpers
        for (k, v) in helpers.iter() {
            let name = format!("helper_{}", k);
            jit_builder.symbol(name, (*v) as usize as *const u8);
        }

        let mut module = JITModule::new(jit_builder);

        let registers = (0..11)
            .map(|i| Variable::new(i))
            .collect::<Vec<_>>()
            .try_into()
            .unwrap();

        Self {
            isa,
            module,
            helpers,
            helper_func_refs: HashMap::new(),
            insn_blocks: BTreeMap::new(),
            insn_targets: BTreeMap::new(),
            filled_blocks: HashSet::new(),
            registers,
            mem_start: Variable::new(11),
            mem_end: Variable::new(12),
            mbuf_start: Variable::new(13),
            mbuf_end: Variable::new(14),
            stack_start: Variable::new(15),
            stack_end: Variable::new(16),
        }
    }

    pub(crate) fn compile_function(mut self, prog: &[u8]) -> Result<CraneliftProgram, Error> {
        let name = "main";
        // This is not a standard eBPF function! We use an informal ABI with just 4 parameters.
        // See [JittedFunction] which is the signature of this function.
        //
        // Since this function only serves as the entrypoint for the JITed program, it doesen't
        // really matter.
        let sig = Signature {
            params: vec![
                AbiParam::new(I64),
                AbiParam::new(I64),
                AbiParam::new(I64),
                AbiParam::new(I64),
            ],
            returns: vec![AbiParam::new(I64)],
            call_conv: self.isa.default_call_conv(),
        };

        let func_id = self
            .module
            .declare_function(name, Linkage::Local, &sig)
            .unwrap();

        let mut ctx = self.module.make_context();
        ctx.func = Function::with_name_signature(UserFuncName::testcase(name.as_bytes()), sig);
        let mut func_ctx = FunctionBuilderContext::new();

        {
            let mut builder: FunctionBuilder = FunctionBuilder::new(&mut ctx.func, &mut func_ctx);

            let entry = builder.create_block();
            builder.append_block_params_for_function_params(entry);
            builder.switch_to_block(entry);

            self.build_cfg(&mut builder, prog)?;
            self.build_function_prelude(&mut builder, entry)?;
            self.translate_program(&mut builder, prog)?;

            builder.seal_all_blocks();
            builder.finalize();
        }

        self.module.define_function(func_id, &mut ctx).unwrap();
        self.module.finalize_definitions().unwrap();
        self.module.clear_context(&mut ctx);

        Ok(CraneliftProgram::new(self.module, func_id))
    }

    fn build_function_prelude(
        &mut self,
        bcx: &mut FunctionBuilder,
        entry: Block,
    ) -> Result<(), Error> {
        // Register the VM registers as variables
        for var in self.registers.iter() {
            bcx.declare_var(*var, I64);
        }

        // Register the bounds check variables
        bcx.declare_var(self.mem_start, I64);
        bcx.declare_var(self.mem_end, I64);
        bcx.declare_var(self.mbuf_start, I64);
        bcx.declare_var(self.mbuf_end, I64);
        bcx.declare_var(self.stack_start, I64);
        bcx.declare_var(self.stack_end, I64);

        // Register the helpers
        for (k, _) in self.helpers.iter() {
            let name = format!("helper_{}", k);
            let sig = Signature {
                params: vec![
                    AbiParam::new(I64),
                    AbiParam::new(I64),
                    AbiParam::new(I64),
                    AbiParam::new(I64),
                    AbiParam::new(I64),
                ],
                returns: vec![AbiParam::new(I64)],
                call_conv: self.isa.default_call_conv(),
            };
            let func_id = self
                .module
                .declare_function(&name, Linkage::Import, &sig)
                .unwrap();

            let func_ref = self.module.declare_func_in_func(func_id, bcx.func);
            self.helper_func_refs.insert(*k, func_ref);
        }

        // Register the stack
        let ss = bcx.create_sized_stack_slot(StackSlotData {
            kind: StackSlotKind::ExplicitSlot,
            size: STACK_SIZE as u32,
        });
        let addr_ty = self.isa.pointer_type();
        let stack_addr = bcx.ins().stack_addr(addr_ty, ss, STACK_SIZE as i32);
        bcx.def_var(self.registers[10], stack_addr);

        // Initialize the bounds check variables
        let stack_start = bcx.ins().stack_addr(addr_ty, ss, 0);
        bcx.def_var(self.stack_start, stack_start);
        let stack_end = bcx.ins().stack_addr(addr_ty, ss, STACK_SIZE as i32);
        bcx.def_var(self.stack_end, stack_end);

        // This is our internal ABI where the first 2 params are the memory
        let mem_start = bcx.block_params(entry)[0];
        let mem_len = bcx.block_params(entry)[1];
        let mem_end = bcx.ins().iadd(mem_start, mem_len);
        bcx.def_var(self.mem_start, mem_start);
        bcx.def_var(self.mem_end, mem_end);

        // And the next 2 are the mbuf
        let mbuf_start = bcx.block_params(entry)[2];
        let mbuf_len = bcx.block_params(entry)[3];
        let mbuf_end = bcx.ins().iadd(mbuf_start, mbuf_len);
        bcx.def_var(self.mbuf_start, mbuf_start);
        bcx.def_var(self.mbuf_end, mbuf_end);

        // The ABI for eBPF specifies that R1 must contain either the memory, or mbuff pointer
        // If the mbuf length is non-zero, then we use that, otherwise we use the memory pointer
        let mbuf_exists = bcx.ins().icmp_imm(IntCC::NotEqual, mbuf_len, 0);
        let mem_or_mbuf = bcx.ins().select(mbuf_exists, mbuf_start, mem_start);
        bcx.def_var(self.registers[1], mem_or_mbuf);

        // R2 should contain the length of the memory or mbuf
        // At least ebpf-conformance tests expect this
        let mem_or_mbuf_len = bcx.ins().select(mbuf_exists, mbuf_len, mem_len);
        bcx.def_var(self.registers[2], mem_or_mbuf_len);

        // Insert the *actual* initial block
        let program_entry = bcx.create_block();
        bcx.ins().jump(program_entry, &[]);
        self.filled_blocks.insert(bcx.current_block().unwrap());
        self.insn_blocks.insert(0, program_entry);

        Ok(())
    }

    fn translate_program(&mut self, bcx: &mut FunctionBuilder, prog: &[u8]) -> Result<(), Error> {
        let mut insn_ptr: usize = 0;
        while insn_ptr * ebpf::INSN_SIZE < prog.len() {
            let insn = ebpf::get_insn(prog, insn_ptr);

            // If this instruction is on a new block switch to it.
            if let Some(block) = self.insn_blocks.get(&(insn_ptr as u32)) {
                // Blocks must have a terminator instruction at the end before we switch away from them
                let current_block = bcx.current_block().unwrap();
                if !self.filled_blocks.contains(&current_block) {
                    bcx.ins().jump(*block, &[]);
                }

                bcx.switch_to_block(*block);
            }

            // Set the source location for the instruction
            bcx.set_srcloc(SourceLoc::new(insn_ptr as u32));

            match insn.opc {
                // BPF_LD class
                // LD_ABS_* and LD_IND_* are supposed to load pointer to data from metadata buffer.
                // Since this pointer is constant, and since we already know it (mem), do not
                // bother re-fetching it, just use mem already.
                ebpf::LD_ABS_B
                | ebpf::LD_ABS_H
                | ebpf::LD_ABS_W
                | ebpf::LD_ABS_DW
                | ebpf::LD_IND_B
                | ebpf::LD_IND_H
                | ebpf::LD_IND_W
                | ebpf::LD_IND_DW => {
                    let ty = match insn.opc {
                        ebpf::LD_ABS_B | ebpf::LD_IND_B => I8,
                        ebpf::LD_ABS_H | ebpf::LD_IND_H => I16,
                        ebpf::LD_ABS_W | ebpf::LD_IND_W => I32,
                        ebpf::LD_ABS_DW | ebpf::LD_IND_DW => I64,
                        _ => unreachable!(),
                    };

                    // Both instructions add the imm part of the instruction to the pointer
                    let ptr = bcx.use_var(self.mem_start);
                    let offset = bcx
                        .ins()
                        .iconst(self.isa.pointer_type(), insn.imm as u32 as i64);
                    let addr = bcx.ins().iadd(ptr, offset);

                    // IND instructions additionally add the value of the source register
                    let is_ind = (insn.opc & BPF_IND) != 0;
                    let addr = if is_ind {
                        let src_reg = self.insn_src(bcx, &insn);
                        bcx.ins().iadd(addr, src_reg)
                    } else {
                        addr
                    };

                    // The offset here has already been added to the pointer, so we pass 0
                    let loaded = self.reg_load(bcx, ty, addr, 0);

                    let ext = if ty != I64 {
                        bcx.ins().uextend(I64, loaded)
                    } else {
                        loaded
                    };

                    self.set_dst(bcx, &insn, ext);
                }
                ebpf::LD_DW_IMM => {
                    insn_ptr += 1;
                    let next_insn = ebpf::get_insn(prog, insn_ptr);

                    let imm = (((insn.imm as u32) as u64) + ((next_insn.imm as u64) << 32)) as i64;
                    let iconst = bcx.ins().iconst(I64, imm);
                    self.set_dst(bcx, &insn, iconst);
                }

                // BPF_LDX class
                ebpf::LD_B_REG | ebpf::LD_H_REG | ebpf::LD_W_REG | ebpf::LD_DW_REG => {
                    let ty = match insn.opc {
                        ebpf::LD_B_REG => I8,
                        ebpf::LD_H_REG => I16,
                        ebpf::LD_W_REG => I32,
                        ebpf::LD_DW_REG => I64,
                        _ => unreachable!(),
                    };

                    let base = self.insn_src(bcx, &insn);
                    let loaded = self.reg_load(bcx, ty, base, insn.off);

                    let ext = if ty != I64 {
                        bcx.ins().uextend(I64, loaded)
                    } else {
                        loaded
                    };

                    self.set_dst(bcx, &insn, ext);
                }

                // BPF_ST and BPF_STX class
                ebpf::ST_B_IMM
                | ebpf::ST_H_IMM
                | ebpf::ST_W_IMM
                | ebpf::ST_DW_IMM
                | ebpf::ST_B_REG
                | ebpf::ST_H_REG
                | ebpf::ST_W_REG
                | ebpf::ST_DW_REG => {
                    let ty = match insn.opc {
                        ebpf::ST_B_IMM | ebpf::ST_B_REG => I8,
                        ebpf::ST_H_IMM | ebpf::ST_H_REG => I16,
                        ebpf::ST_W_IMM | ebpf::ST_W_REG => I32,
                        ebpf::ST_DW_IMM | ebpf::ST_DW_REG => I64,
                        _ => unreachable!(),
                    };
                    let is_imm = match insn.opc {
                        ebpf::ST_B_IMM | ebpf::ST_H_IMM | ebpf::ST_W_IMM | ebpf::ST_DW_IMM => true,
                        ebpf::ST_B_REG | ebpf::ST_H_REG | ebpf::ST_W_REG | ebpf::ST_DW_REG => false,
                        _ => unreachable!(),
                    };

                    let value = if is_imm {
                        self.insn_imm64(bcx, &insn)
                    } else {
                        self.insn_src(bcx, &insn)
                    };

                    let narrow = if ty != I64 {
                        bcx.ins().ireduce(ty, value)
                    } else {
                        value
                    };

                    let base = self.insn_dst(bcx, &insn);
                    self.reg_store(bcx, ty, base, insn.off, narrow);
                }

                ebpf::ST_W_XADD => unimplemented!(),
                ebpf::ST_DW_XADD => unimplemented!(),

                // BPF_ALU class
                // TODO Check how overflow works in kernel. Should we &= U32MAX all src register value
                // before we do the operation?
                // Cf ((0x11 << 32) - (0x1 << 32)) as u32 VS ((0x11 << 32) as u32 - (0x1 << 32) as u32
                ebpf::ADD32_IMM => {
                    let src = self.insn_dst32(bcx, &insn);
                    let imm = self.insn_imm32(bcx, &insn);
                    let res = bcx.ins().iadd(src, imm);
                    self.set_dst32(bcx, &insn, res);
                }
                ebpf::ADD32_REG => {
                    //((reg[_dst] & U32MAX) + (reg[_src] & U32MAX)) & U32MAX,
                    let lhs = self.insn_dst32(bcx, &insn);
                    let rhs = self.insn_src32(bcx, &insn);
                    let res = bcx.ins().iadd(lhs, rhs);
                    self.set_dst32(bcx, &insn, res);
                }
                ebpf::SUB32_IMM => {
                    // reg[_dst] = (reg[_dst] as i32).wrapping_sub(insn.imm)         as u64,
                    let src = self.insn_dst32(bcx, &insn);
                    let imm = self.insn_imm32(bcx, &insn);
                    let res = bcx.ins().isub(src, imm);
                    self.set_dst32(bcx, &insn, res);
                }
                ebpf::SUB32_REG => {
                    // reg[_dst] = (reg[_dst] as i32).wrapping_sub(reg[_src] as i32) as u64,
                    let lhs = self.insn_dst32(bcx, &insn);
                    let rhs = self.insn_src32(bcx, &insn);
                    let res = bcx.ins().isub(lhs, rhs);
                    self.set_dst32(bcx, &insn, res);
                }
                ebpf::MUL32_IMM => {
                    // reg[_dst] = (reg[_dst] as i32).wrapping_mul(insn.imm)         as u64,
                    let src = self.insn_dst32(bcx, &insn);
                    let imm = self.insn_imm32(bcx, &insn);
                    let res = bcx.ins().imul(src, imm);
                    self.set_dst32(bcx, &insn, res);
                }
                ebpf::MUL32_REG => {
                    // reg[_dst] = (reg[_dst] as i32).wrapping_mul(reg[_src] as i32) as u64,
                    let lhs = self.insn_dst32(bcx, &insn);
                    let rhs = self.insn_src32(bcx, &insn);
                    let res = bcx.ins().imul(lhs, rhs);
                    self.set_dst32(bcx, &insn, res);
                }
                ebpf::DIV32_IMM => {
                    // reg[_dst] = (reg[_dst] as u32 / insn.imm              as u32) as u64,
                    let res = if insn.imm == 0 {
                        bcx.ins().iconst(I32, 0)
                    } else {
                        let imm = self.insn_imm32(bcx, &insn);
                        let src = self.insn_dst32(bcx, &insn);
                        bcx.ins().udiv(src, imm)
                    };
                    self.set_dst32(bcx, &insn, res);
                }
                ebpf::DIV32_REG => {
                    // reg[_dst] = (reg[_dst] as u32 / reg[_src]             as u32) as u64,
                    let zero = bcx.ins().iconst(I32, 0);
                    let one = bcx.ins().iconst(I32, 1);

                    let lhs = self.insn_dst32(bcx, &insn);
                    let rhs = self.insn_src32(bcx, &insn);

                    let rhs_is_zero = bcx.ins().icmp(IntCC::Equal, rhs, zero);
                    let safe_rhs = bcx.ins().select(rhs_is_zero, one, rhs);
                    let div_res = bcx.ins().udiv(lhs, safe_rhs);

                    let res = bcx.ins().select(rhs_is_zero, zero, div_res);
                    self.set_dst32(bcx, &insn, res);
                }
                ebpf::OR32_IMM => {
                    // reg[_dst] = (reg[_dst] as u32             | insn.imm  as u32) as u64,
                    let src = self.insn_dst32(bcx, &insn);
                    let imm = self.insn_imm32(bcx, &insn);
                    let res = bcx.ins().bor(src, imm);
                    self.set_dst32(bcx, &insn, res);
                }
                ebpf::OR32_REG => {
                    // reg[_dst] = (reg[_dst] as u32             | reg[_src] as u32) as u64,
                    let lhs = self.insn_dst32(bcx, &insn);
                    let rhs = self.insn_src32(bcx, &insn);
                    let res = bcx.ins().bor(lhs, rhs);
                    self.set_dst32(bcx, &insn, res);
                }
                ebpf::AND32_IMM => {
                    // reg[_dst] = (reg[_dst] as u32             & insn.imm  as u32) as u64,
                    let src = self.insn_dst32(bcx, &insn);
                    let imm = self.insn_imm32(bcx, &insn);
                    let res = bcx.ins().band(src, imm);
                    self.set_dst32(bcx, &insn, res);
                }
                ebpf::AND32_REG => {
                    // reg[_dst] = (reg[_dst] as u32             & reg[_src] as u32) as u64,
                    let lhs = self.insn_dst32(bcx, &insn);
                    let rhs = self.insn_src32(bcx, &insn);
                    let res = bcx.ins().band(lhs, rhs);
                    self.set_dst32(bcx, &insn, res);
                }
                ebpf::LSH32_IMM => {
                    // reg[_dst] = (reg[_dst] as u32).wrapping_shl(insn.imm  as u32) as u64,
                    let src = self.insn_dst32(bcx, &insn);
                    let imm = self.insn_imm32(bcx, &insn);
                    let res = bcx.ins().ishl(src, imm);
                    self.set_dst32(bcx, &insn, res);
                }
                ebpf::LSH32_REG => {
                    // reg[_dst] = (reg[_dst] as u32).wrapping_shl(reg[_src] as u32) as u64,
                    let lhs = self.insn_dst32(bcx, &insn);
                    let rhs = self.insn_src32(bcx, &insn);
                    let res = bcx.ins().ishl(lhs, rhs);
                    self.set_dst32(bcx, &insn, res);
                }
                ebpf::RSH32_IMM => {
                    // reg[_dst] = (reg[_dst] as u32).wrapping_shr(insn.imm  as u32) as u64,
                    let src = self.insn_dst32(bcx, &insn);
                    let imm = self.insn_imm32(bcx, &insn);
                    let res = bcx.ins().ushr(src, imm);
                    self.set_dst32(bcx, &insn, res);
                }
                ebpf::RSH32_REG => {
                    // reg[_dst] = (reg[_dst] as u32).wrapping_shr(reg[_src] as u32) as u64,
                    let lhs = self.insn_dst32(bcx, &insn);
                    let rhs = self.insn_src32(bcx, &insn);
                    let res = bcx.ins().ushr(lhs, rhs);
                    self.set_dst32(bcx, &insn, res);
                }
                ebpf::NEG32 => {
                    // { reg[_dst] = (reg[_dst] as i32).wrapping_neg()                 as u64; reg[_dst] &= U32MAX; },
                    let src = self.insn_dst32(bcx, &insn);
                    let res = bcx.ins().ineg(src);
                    // TODO: Do we need to mask the result?
                    self.set_dst32(bcx, &insn, res);
                }
                ebpf::MOD32_IMM => {
                    // reg[_dst] = (reg[_dst] as u32             % insn.imm  as u32) as u64,

                    if insn.imm != 0 {
                        let imm = self.insn_imm32(bcx, &insn);
                        let src = self.insn_dst32(bcx, &insn);
                        let res = bcx.ins().urem(src, imm);
                        self.set_dst32(bcx, &insn, res);
                    }
                }
                ebpf::MOD32_REG => {
                    // reg[_dst] = (reg[_dst] as u32 % reg[_src]             as u32) as u64,
                    let zero = bcx.ins().iconst(I32, 0);
                    let one = bcx.ins().iconst(I32, 1);

                    let lhs = self.insn_dst32(bcx, &insn);
                    let rhs = self.insn_src32(bcx, &insn);

                    let rhs_is_zero = bcx.ins().icmp(IntCC::Equal, rhs, zero);
                    let safe_rhs = bcx.ins().select(rhs_is_zero, one, rhs);
                    let div_res = bcx.ins().urem(lhs, safe_rhs);

                    let res = bcx.ins().select(rhs_is_zero, lhs, div_res);
                    self.set_dst32(bcx, &insn, res);
                }
                ebpf::XOR32_IMM => {
                    // reg[_dst] = (reg[_dst] as u32             ^ insn.imm  as u32) as u64,
                    let src = self.insn_dst32(bcx, &insn);
                    let imm = self.insn_imm32(bcx, &insn);
                    let res = bcx.ins().bxor(src, imm);
                    self.set_dst32(bcx, &insn, res);
                }
                ebpf::XOR32_REG => {
                    // reg[_dst] = (reg[_dst] as u32             ^ reg[_src] as u32) as u64,
                    let lhs = self.insn_dst32(bcx, &insn);
                    let rhs = self.insn_src32(bcx, &insn);
                    let res = bcx.ins().bxor(lhs, rhs);
                    self.set_dst32(bcx, &insn, res);
                }
                ebpf::MOV32_IMM => {
                    let imm = self.insn_imm32(bcx, &insn);
                    self.set_dst32(bcx, &insn, imm);
                }
                ebpf::MOV32_REG => {
                    // reg[_dst] = (reg[_src] as u32)                                as u64,
                    let src = self.insn_src32(bcx, &insn);
                    self.set_dst32(bcx, &insn, src);
                }
                ebpf::ARSH32_IMM => {
                    // { reg[_dst] = (reg[_dst] as i32).wrapping_shr(insn.imm  as u32) as u64; reg[_dst] &= U32MAX; },
                    let src = self.insn_dst32(bcx, &insn);
                    let imm = self.insn_imm32(bcx, &insn);
                    let res = bcx.ins().sshr(src, imm);
                    self.set_dst32(bcx, &insn, res);
                }
                ebpf::ARSH32_REG => {
                    // { reg[_dst] = (reg[_dst] as i32).wrapping_shr(reg[_src] as u32) as u64; reg[_dst] &= U32MAX; },
                    let lhs = self.insn_dst32(bcx, &insn);
                    let rhs = self.insn_src32(bcx, &insn);
                    let res = bcx.ins().sshr(lhs, rhs);
                    self.set_dst32(bcx, &insn, res);
                }

                ebpf::BE | ebpf::LE => {
                    let should_swap = match insn.opc {
                        ebpf::BE => self.isa.endianness() == Endianness::Little,
                        ebpf::LE => self.isa.endianness() == Endianness::Big,
                        _ => unreachable!(),
                    };

                    let ty: Type = match insn.imm {
                        16 => I16,
                        32 => I32,
                        64 => I64,
                        _ => unreachable!(),
                    };

                    if should_swap {
                        let src = self.insn_dst(bcx, &insn);
                        let src_narrow = if ty != I64 {
                            bcx.ins().ireduce(ty, src)
                        } else {
                            src
                        };

                        let res = bcx.ins().bswap(src_narrow);
                        let res_wide = if ty != I64 {
                            bcx.ins().uextend(I64, res)
                        } else {
                            res
                        };

                        self.set_dst(bcx, &insn, res_wide);
                    }
                }

                // BPF_ALU64 class
                ebpf::ADD64_IMM => {
                    // reg[_dst] = reg[_dst].wrapping_add(insn.imm as u64),
                    let imm = self.insn_imm64(bcx, &insn);
                    let src = self.insn_dst(bcx, &insn);
                    let res = bcx.ins().iadd(src, imm);
                    self.set_dst(bcx, &insn, res);
                }
                ebpf::ADD64_REG => {
                    // reg[_dst] = reg[_dst].wrapping_add(reg[_src]),
                    let lhs = self.insn_dst(bcx, &insn);
                    let rhs = self.insn_src(bcx, &insn);
                    let res = bcx.ins().iadd(lhs, rhs);
                    self.set_dst(bcx, &insn, res);
                }
                ebpf::SUB64_IMM => {
                    // reg[_dst] = reg[_dst].wrapping_sub(insn.imm as u64),
                    let imm = self.insn_imm64(bcx, &insn);
                    let src = self.insn_dst(bcx, &insn);
                    let res = bcx.ins().isub(src, imm);
                    self.set_dst(bcx, &insn, res);
                }
                ebpf::SUB64_REG => {
                    // reg[_dst] = reg[_dst].wrapping_sub(reg[_src]),
                    let lhs = self.insn_dst(bcx, &insn);
                    let rhs = self.insn_src(bcx, &insn);
                    let res = bcx.ins().isub(lhs, rhs);
                    self.set_dst(bcx, &insn, res);
                }
                ebpf::MUL64_IMM => {
                    // reg[_dst] = reg[_dst].wrapping_mul(insn.imm as u64),
                    let imm = self.insn_imm64(bcx, &insn);
                    let src = self.insn_dst(bcx, &insn);
                    let res = bcx.ins().imul(src, imm);
                    self.set_dst(bcx, &insn, res);
                }
                ebpf::MUL64_REG => {
                    // reg[_dst] = reg[_dst].wrapping_mul(reg[_src]),
                    let lhs = self.insn_dst(bcx, &insn);
                    let rhs = self.insn_src(bcx, &insn);
                    let res = bcx.ins().imul(lhs, rhs);
                    self.set_dst(bcx, &insn, res);
                }
                ebpf::DIV64_IMM => {
                    // reg[_dst] /= insn.imm as u64,
                    let res = if insn.imm == 0 {
                        bcx.ins().iconst(I64, 0)
                    } else {
                        let imm = self.insn_imm64(bcx, &insn);
                        let src = self.insn_dst(bcx, &insn);
                        bcx.ins().udiv(src, imm)
                    };
                    self.set_dst(bcx, &insn, res);
                }
                ebpf::DIV64_REG => {
                    // reg[_dst] /= reg[_src], if reg[_src] != 0
                    // reg[_dst] = 0, if reg[_src] == 0
                    let zero = bcx.ins().iconst(I64, 0);
                    let one = bcx.ins().iconst(I64, 1);

                    let lhs = self.insn_dst(bcx, &insn);
                    let rhs = self.insn_src(bcx, &insn);

                    let rhs_is_zero = bcx.ins().icmp(IntCC::Equal, rhs, zero);
                    let safe_rhs = bcx.ins().select(rhs_is_zero, one, rhs);
                    let div_res = bcx.ins().udiv(lhs, safe_rhs);

                    let res = bcx.ins().select(rhs_is_zero, zero, div_res);
                    self.set_dst(bcx, &insn, res);
                }
                ebpf::MOD64_IMM => {
                    // reg[_dst] %= insn.imm as u64,

                    if insn.imm != 0 {
                        let imm = self.insn_imm64(bcx, &insn);
                        let src = self.insn_dst(bcx, &insn);
                        let res = bcx.ins().urem(src, imm);
                        self.set_dst(bcx, &insn, res);
                    };
                }
                ebpf::MOD64_REG => {
                    // reg[_dst] %= reg[_src], if reg[_src] != 0

                    let zero = bcx.ins().iconst(I64, 0);
                    let one = bcx.ins().iconst(I64, 1);

                    let lhs = self.insn_dst(bcx, &insn);
                    let rhs = self.insn_src(bcx, &insn);

                    let rhs_is_zero = bcx.ins().icmp(IntCC::Equal, rhs, zero);
                    let safe_rhs = bcx.ins().select(rhs_is_zero, one, rhs);
                    let div_res = bcx.ins().urem(lhs, safe_rhs);

                    let res = bcx.ins().select(rhs_is_zero, lhs, div_res);
                    self.set_dst(bcx, &insn, res);
                }
                ebpf::OR64_IMM => {
                    // reg[_dst] |= insn.imm as u64,
                    let imm = self.insn_imm64(bcx, &insn);
                    let src = self.insn_dst(bcx, &insn);
                    let res = bcx.ins().bor(src, imm);
                    self.set_dst(bcx, &insn, res);
                }
                ebpf::OR64_REG => {
                    // reg[_dst] |= reg[_src],
                    let lhs = self.insn_dst(bcx, &insn);
                    let rhs = self.insn_src(bcx, &insn);
                    let res = bcx.ins().bor(lhs, rhs);
                    self.set_dst(bcx, &insn, res);
                }
                ebpf::AND64_IMM => {
                    // reg[_dst] &= insn.imm as u64,
                    let imm = self.insn_imm64(bcx, &insn);
                    let src = self.insn_dst(bcx, &insn);
                    let res = bcx.ins().band(src, imm);
                    self.set_dst(bcx, &insn, res);
                }
                ebpf::AND64_REG => {
                    // reg[_dst] &= reg[_src],
                    let lhs = self.insn_dst(bcx, &insn);
                    let rhs = self.insn_src(bcx, &insn);
                    let res = bcx.ins().band(lhs, rhs);
                    self.set_dst(bcx, &insn, res);
                }
                ebpf::LSH64_IMM => {
                    // reg[_dst] <<= insn.imm as u64,
                    let imm = self.insn_imm64(bcx, &insn);
                    let src = self.insn_dst(bcx, &insn);
                    let res = bcx.ins().ishl(src, imm);
                    self.set_dst(bcx, &insn, res);
                }
                ebpf::LSH64_REG => {
                    // reg[_dst] <<= reg[_src],
                    let lhs = self.insn_dst(bcx, &insn);
                    let rhs = self.insn_src(bcx, &insn);
                    let res = bcx.ins().ishl(lhs, rhs);
                    self.set_dst(bcx, &insn, res);
                }
                ebpf::RSH64_IMM => {
                    // reg[_dst] >>= insn.imm as u64,
                    let imm = self.insn_imm64(bcx, &insn);
                    let src = self.insn_dst(bcx, &insn);
                    let res = bcx.ins().ushr(src, imm);
                    self.set_dst(bcx, &insn, res);
                }
                ebpf::RSH64_REG => {
                    // reg[_dst] >>= reg[_src],
                    let lhs = self.insn_dst(bcx, &insn);
                    let rhs = self.insn_src(bcx, &insn);
                    let res = bcx.ins().ushr(lhs, rhs);
                    self.set_dst(bcx, &insn, res);
                }
                ebpf::NEG64 => {
                    // reg[_dst] = -(reg[_dst] as i64) as u64,
                    let src = self.insn_dst(bcx, &insn);
                    let res = bcx.ins().ineg(src);
                    self.set_dst(bcx, &insn, res);
                }
                ebpf::XOR64_IMM => {
                    // reg[_dst] ^= insn.imm as u64,
                    let imm = self.insn_imm64(bcx, &insn);
                    let src = self.insn_dst(bcx, &insn);
                    let res = bcx.ins().bxor(src, imm);
                    self.set_dst(bcx, &insn, res);
                }
                ebpf::XOR64_REG => {
                    // reg[_dst] ^= reg[_src],
                    let lhs = self.insn_dst(bcx, &insn);
                    let rhs = self.insn_src(bcx, &insn);
                    let res = bcx.ins().bxor(lhs, rhs);
                    self.set_dst(bcx, &insn, res);
                }
                ebpf::MOV64_IMM => {
                    // reg[_dst] = insn.imm as u64,
                    let imm = self.insn_imm64(bcx, &insn);
                    bcx.def_var(self.registers[insn.dst as usize], imm);
                }
                ebpf::MOV64_REG => {
                    // reg[_dst] = reg[_src],
                    let src = self.insn_src(bcx, &insn);
                    bcx.def_var(self.registers[insn.dst as usize], src);
                }
                ebpf::ARSH64_IMM => {
                    // reg[_dst] = (reg[_dst] as i64 >> insn.imm) as u64,
                    let imm = self.insn_imm64(bcx, &insn);
                    let src = self.insn_dst(bcx, &insn);
                    let res = bcx.ins().sshr(src, imm);
                    self.set_dst(bcx, &insn, res);
                }
                ebpf::ARSH64_REG => {
                    // reg[_dst] = (reg[_dst] as i64 >> reg[_src]) as u64,
                    let lhs = self.insn_dst(bcx, &insn);
                    let rhs = self.insn_src(bcx, &insn);
                    let res = bcx.ins().sshr(lhs, rhs);
                    self.set_dst(bcx, &insn, res);
                }

                // BPF_JMP & BPF_JMP32 class
                ebpf::JA => {
                    let (_, target_block) = self.insn_targets[&(insn_ptr as u32)];

                    bcx.ins().jump(target_block, &[]);
                    self.filled_blocks.insert(bcx.current_block().unwrap());
                }
                ebpf::JEQ_IMM
                | ebpf::JEQ_REG
                | ebpf::JGT_IMM
                | ebpf::JGT_REG
                | ebpf::JGE_IMM
                | ebpf::JGE_REG
                | ebpf::JLT_IMM
                | ebpf::JLT_REG
                | ebpf::JLE_IMM
                | ebpf::JLE_REG
                | ebpf::JNE_IMM
                | ebpf::JNE_REG
                | ebpf::JSGT_IMM
                | ebpf::JSGT_REG
                | ebpf::JSGE_IMM
                | ebpf::JSGE_REG
                | ebpf::JSLT_IMM
                | ebpf::JSLT_REG
                | ebpf::JSLE_IMM
                | ebpf::JSLE_REG
                | ebpf::JSET_IMM
                | ebpf::JSET_REG
                | ebpf::JEQ_IMM32
                | ebpf::JEQ_REG32
                | ebpf::JGT_IMM32
                | ebpf::JGT_REG32
                | ebpf::JGE_IMM32
                | ebpf::JGE_REG32
                | ebpf::JLT_IMM32
                | ebpf::JLT_REG32
                | ebpf::JLE_IMM32
                | ebpf::JLE_REG32
                | ebpf::JNE_IMM32
                | ebpf::JNE_REG32
                | ebpf::JSGT_IMM32
                | ebpf::JSGT_REG32
                | ebpf::JSGE_IMM32
                | ebpf::JSGE_REG32
                | ebpf::JSLT_IMM32
                | ebpf::JSLT_REG32
                | ebpf::JSLE_IMM32
                | ebpf::JSLE_REG32
                | ebpf::JSET_IMM32
                | ebpf::JSET_REG32 => {
                    let (fallthrough, target) = self.insn_targets[&(insn_ptr as u32)];

                    let is_reg = (insn.opc & BPF_X) != 0;
                    let is_32 = (insn.opc & BPF_JMP32) != 0;
                    let intcc = match insn.opc {
                        c if (c & BPF_ALU_OP_MASK) == BPF_JEQ => IntCC::Equal,
                        c if (c & BPF_ALU_OP_MASK) == BPF_JNE => IntCC::NotEqual,
                        c if (c & BPF_ALU_OP_MASK) == BPF_JGT => IntCC::UnsignedGreaterThan,
                        c if (c & BPF_ALU_OP_MASK) == BPF_JGE => IntCC::UnsignedGreaterThanOrEqual,
                        c if (c & BPF_ALU_OP_MASK) == BPF_JLT => IntCC::UnsignedLessThan,
                        c if (c & BPF_ALU_OP_MASK) == BPF_JLE => IntCC::UnsignedLessThanOrEqual,
                        c if (c & BPF_ALU_OP_MASK) == BPF_JSGT => IntCC::SignedGreaterThan,
                        c if (c & BPF_ALU_OP_MASK) == BPF_JSGE => IntCC::SignedGreaterThanOrEqual,
                        c if (c & BPF_ALU_OP_MASK) == BPF_JSLT => IntCC::SignedLessThan,
                        c if (c & BPF_ALU_OP_MASK) == BPF_JSLE => IntCC::SignedLessThanOrEqual,
                        // JSET is handled specially below
                        c if (c & BPF_ALU_OP_MASK) == BPF_JSET => IntCC::NotEqual,
                        _ => unreachable!(),
                    };

                    let lhs = if is_32 {
                        self.insn_dst32(bcx, &insn)
                    } else {
                        self.insn_dst(bcx, &insn)
                    };
                    let rhs = match (is_reg, is_32) {
                        (true, false) => self.insn_src(bcx, &insn),
                        (true, true) => self.insn_src32(bcx, &insn),
                        (false, false) => self.insn_imm64(bcx, &insn),
                        (false, true) => self.insn_imm32(bcx, &insn),
                    };

                    let cmp_res = if (insn.opc & BPF_ALU_OP_MASK) == BPF_JSET {
                        bcx.ins().band(lhs, rhs)
                    } else {
                        bcx.ins().icmp(intcc, lhs, rhs)
                    };
                    bcx.ins().brif(cmp_res, target, &[], fallthrough, &[]);
                    self.filled_blocks.insert(bcx.current_block().unwrap());
                }

                // Do not delegate the check to the verifier, since registered functions can be
                // changed after the program has been verified.
                ebpf::CALL => {
                    let func_ref = self
                        .helper_func_refs
                        .get(&(insn.imm as u32))
                        .copied()
                        .ok_or_else(|| {
                            Error::new(
                                ErrorKind::Other,
                                format!(
                                    "[CRANELIFT] Error: unknown helper function (id: {:#x})",
                                    insn.imm as u32
                                ),
                            )
                        })?;

                    let arg0 = bcx.use_var(self.registers[1]);
                    let arg1 = bcx.use_var(self.registers[2]);
                    let arg2 = bcx.use_var(self.registers[3]);
                    let arg3 = bcx.use_var(self.registers[4]);
                    let arg4 = bcx.use_var(self.registers[5]);

                    let call = bcx.ins().call(func_ref, &[arg0, arg1, arg2, arg3, arg4]);
                    let ret = bcx.inst_results(call)[0];
                    self.set_dst(bcx, &insn, ret);
                }
                ebpf::TAIL_CALL => unimplemented!(),
                ebpf::EXIT => {
                    let ret = bcx.use_var(self.registers[0]);
                    bcx.ins().return_(&[ret]);
                    self.filled_blocks.insert(bcx.current_block().unwrap());
                }
                _ => unimplemented!("inst: {:?}", insn),
            }

            insn_ptr += 1;
        }

        Ok(())
    }

    fn insn_imm64(&mut self, bcx: &mut FunctionBuilder, insn: &Insn) -> Value {
        bcx.ins().iconst(I64, insn.imm as u64 as i64)
    }
    fn insn_imm32(&mut self, bcx: &mut FunctionBuilder, insn: &Insn) -> Value {
        bcx.ins().iconst(I32, insn.imm as u32 as u64 as i64)
    }

    fn insn_dst(&mut self, bcx: &mut FunctionBuilder, insn: &Insn) -> Value {
        bcx.use_var(self.registers[insn.dst as usize])
    }
    fn insn_dst32(&mut self, bcx: &mut FunctionBuilder, insn: &Insn) -> Value {
        let dst = self.insn_dst(bcx, insn);
        bcx.ins().ireduce(I32, dst)
    }

    fn insn_src(&mut self, bcx: &mut FunctionBuilder, insn: &Insn) -> Value {
        bcx.use_var(self.registers[insn.src as usize])
    }
    fn insn_src32(&mut self, bcx: &mut FunctionBuilder, insn: &Insn) -> Value {
        let src = self.insn_src(bcx, insn);
        bcx.ins().ireduce(I32, src)
    }

    fn set_dst(&mut self, bcx: &mut FunctionBuilder, insn: &Insn, val: Value) {
        bcx.def_var(self.registers[insn.dst as usize], val);
    }
    fn set_dst32(&mut self, bcx: &mut FunctionBuilder, insn: &Insn, val: Value) {
        let val32 = bcx.ins().uextend(I64, val);
        self.set_dst(bcx, insn, val32);
    }

    fn reg_load(&mut self, bcx: &mut FunctionBuilder, ty: Type, base: Value, offset: i16) -> Value {
        self.insert_bounds_check(bcx, ty, base, offset);

        let mut flags = MemFlags::new();
        flags.set_endianness(Endianness::Little);

        bcx.ins().load(ty, flags, base, offset as i32)
    }
    fn reg_store(
        &mut self,
        bcx: &mut FunctionBuilder,
        ty: Type,
        base: Value,
        offset: i16,
        val: Value,
    ) {
        self.insert_bounds_check(bcx, ty, base, offset);

        let mut flags = MemFlags::new();
        flags.set_endianness(Endianness::Little);

        bcx.ins().store(flags, val, base, offset as i32);
    }

    /// Inserts a bounds check for a memory access
    ///
    /// This emits a conditional trap if the access is out of bounds for any of the known
    /// valid memory regions. These are the stack, the memory, and the mbuf.
    fn insert_bounds_check(
        &mut self,
        bcx: &mut FunctionBuilder,
        ty: Type,
        base: Value,
        offset: i16,
    ) {
        let access_size = bcx.ins().iconst(I64, ty.bytes() as i64);

        let offset = bcx.ins().iconst(I64, offset as i64);
        let start_addr = bcx.ins().iadd(base, offset);
        let end_addr = bcx.ins().iadd(start_addr, access_size);

        let does_not_overflow =
            bcx.ins()
                .icmp(IntCC::UnsignedGreaterThanOrEqual, end_addr, start_addr);

        // Check if it's a valid stack access
        let stack_start = bcx.use_var(self.stack_start);
        let stack_end = bcx.use_var(self.stack_end);
        let stack_start_valid =
            bcx.ins()
                .icmp(IntCC::UnsignedGreaterThanOrEqual, start_addr, stack_start);
        let stack_end_valid = bcx
            .ins()
            .icmp(IntCC::UnsignedLessThanOrEqual, end_addr, stack_end);
        let stack_valid = bcx.ins().band(stack_start_valid, stack_end_valid);

        // Check if it's a valid memory access
        let mem_start = bcx.use_var(self.mem_start);
        let mem_end = bcx.use_var(self.mem_end);
        let has_mem = bcx.ins().icmp_imm(IntCC::NotEqual, mem_start, 0);
        let mem_start_valid =
            bcx.ins()
                .icmp(IntCC::UnsignedGreaterThanOrEqual, start_addr, mem_start);
        let mem_end_valid = bcx
            .ins()
            .icmp(IntCC::UnsignedLessThanOrEqual, end_addr, mem_end);

        let mem_valid = bcx.ins().band(mem_start_valid, mem_end_valid);
        let mem_valid = bcx.ins().band(mem_valid, has_mem);

        // Check if it's a valid mbuf access
        let mbuf_start = bcx.use_var(self.mbuf_start);
        let mbuf_end = bcx.use_var(self.mbuf_end);
        let has_mbuf = bcx.ins().icmp_imm(IntCC::NotEqual, mbuf_start, 0);
        let mbuf_start_valid =
            bcx.ins()
                .icmp(IntCC::UnsignedGreaterThanOrEqual, start_addr, mbuf_start);
        let mbuf_end_valid = bcx
            .ins()
            .icmp(IntCC::UnsignedLessThanOrEqual, end_addr, mbuf_end);
        let mbuf_valid = bcx.ins().band(mbuf_start_valid, mbuf_end_valid);
        let mbuf_valid = bcx.ins().band(mbuf_valid, has_mbuf);

        // Join all of these checks together and trap if any of them fails

        // We need it to be valid to at least one region of memory
        let valid_region = bcx.ins().bor(stack_valid, mem_valid);
        let valid_region = bcx.ins().bor(valid_region, mbuf_valid);

        // And that it does not overflow
        let valid = bcx.ins().band(does_not_overflow, valid_region);

        // TODO: We can potentially throw a custom trap code here to indicate
        // which check failed.
        bcx.ins().trapz(valid, TrapCode::HeapOutOfBounds);
    }

    /// Analyze the program and build the CFG
    ///
    /// We do this because cranelift does not allow us to switch back to a previously
    /// filled block and add instructions to it. So we can't split the program as we
    /// translate it.
    fn build_cfg(&mut self, bcx: &mut FunctionBuilder, prog: &[u8]) -> Result<(), Error> {
        let mut insn_ptr: usize = 0;
        while insn_ptr * ebpf::INSN_SIZE < prog.len() {
            let insn = ebpf::get_insn(prog, insn_ptr);

            match insn.opc {
                // This instruction consumes two opcodes
                ebpf::LD_DW_IMM => {
                    insn_ptr += 1;
                }

                ebpf::JA
                | ebpf::JEQ_IMM
                | ebpf::JEQ_REG
                | ebpf::JGT_IMM
                | ebpf::JGT_REG
                | ebpf::JGE_IMM
                | ebpf::JGE_REG
                | ebpf::JLT_IMM
                | ebpf::JLT_REG
                | ebpf::JLE_IMM
                | ebpf::JLE_REG
                | ebpf::JNE_IMM
                | ebpf::JNE_REG
                | ebpf::JSGT_IMM
                | ebpf::JSGT_REG
                | ebpf::JSGE_IMM
                | ebpf::JSGE_REG
                | ebpf::JSLT_IMM
                | ebpf::JSLT_REG
                | ebpf::JSLE_IMM
                | ebpf::JSLE_REG
                | ebpf::JSET_IMM
                | ebpf::JSET_REG
                | ebpf::JEQ_IMM32
                | ebpf::JEQ_REG32
                | ebpf::JGT_IMM32
                | ebpf::JGT_REG32
                | ebpf::JGE_IMM32
                | ebpf::JGE_REG32
                | ebpf::JLT_IMM32
                | ebpf::JLT_REG32
                | ebpf::JLE_IMM32
                | ebpf::JLE_REG32
                | ebpf::JNE_IMM32
                | ebpf::JNE_REG32
                | ebpf::JSGT_IMM32
                | ebpf::JSGT_REG32
                | ebpf::JSGE_IMM32
                | ebpf::JSGE_REG32
                | ebpf::JSLT_IMM32
                | ebpf::JSLT_REG32
                | ebpf::JSLE_IMM32
                | ebpf::JSLE_REG32
                | ebpf::JSET_IMM32
                | ebpf::JSET_REG32
                | ebpf::EXIT
                | ebpf::TAIL_CALL => {
                    self.prepare_jump_blocks(bcx, insn_ptr, &insn);
                }
                _ => {}
            }

            insn_ptr += 1;
        }

        Ok(())
    }

    fn prepare_jump_blocks(&mut self, bcx: &mut FunctionBuilder, insn_ptr: usize, insn: &Insn) {
        let insn_ptr = insn_ptr as u32;
        let next_pc: u32 = insn_ptr + 1;
        let target_pc: u32 = (insn_ptr as isize + insn.off as isize + 1)
            .try_into()
            .unwrap();

        // This is the fallthrough block
        let fallthrough_block = *self
            .insn_blocks
            .entry(next_pc)
            .or_insert_with(|| bcx.create_block());

        // Jump Target
        let target_block = *self
            .insn_blocks
            .entry(target_pc)
            .or_insert_with(|| bcx.create_block());

        // Mark the blocks for this instruction
        self.insn_targets
            .insert(insn_ptr, (fallthrough_block, target_block));
    }
}

/// Contains the backing memory for a previously compiled function.
///
/// Currently this will allways just contain code for a single function, but
/// in the future we might want to support multiple functions per module.
///
/// Ensures that the backing memory is freed when dropped.
pub struct CraneliftProgram {
    module: ManuallyDrop<JITModule>,

    main_id: FuncId,
}

impl CraneliftProgram {
    pub(crate) fn new(module: JITModule, main_id: FuncId) -> Self {
        Self {
            module: ManuallyDrop::new(module),
            main_id,
        }
    }

    /// We shouldn't allow this function pointer to be exposed outside of this
    /// module, since it's not guaranteed to be valid after the module is dropped.
    pub(crate) fn get_main_function(&self) -> JittedFunction {
        let function_ptr = self.module.get_finalized_function(self.main_id);
        unsafe { mem::transmute(function_ptr) }
    }

    /// Execute this module by calling the main function
    pub fn execute(
        &self,
        mem_ptr: *mut u8,
        mem_len: usize,
        mbuff_ptr: *mut u8,
        mbuff_len: usize,
    ) -> u64 {
        let main = self.get_main_function();

        main(mem_ptr, mem_len, mbuff_ptr, mbuff_len)
    }
}

impl Drop for CraneliftProgram {
    fn drop(&mut self) {
        // We need to have an owned version of `JITModule` to be able to free
        // it's memory. Use `ManuallyDrop` to get the owned `JITModule`.
        //
        // We can no longer use `module` after this, but since we are `Drop`
        // it should be safe.
        unsafe {
            let module = ManuallyDrop::take(&mut self.module);
            module.free_memory()
        };
    }
}
