use super::CFilesArch;
use cc::Build;
use std::{collections::HashSet, path::PathBuf};

pub(super) struct X86_64CFilesArch;

impl CFilesArch for X86_64CFilesArch {
    fn setup_defines(&self, c: &mut cc::Build) {
        c.define("__x86_64__", None);
    }

    fn setup_files(&self, _c: &mut Build, files: &mut HashSet<PathBuf>) {
        // setup asm files
        files.insert(PathBuf::from("src/arch/x86_64/asm/head.S"));
        files.insert(PathBuf::from("src/arch/x86_64/asm/entry.S"));
        files.insert(PathBuf::from("src/arch/x86_64/asm/apu_boot.S"));
        files.insert(PathBuf::from("src/arch/x86_64/vm/vmx/vmenter.S"));
    }

    fn setup_global_flags(&self, c: &mut Build) {
        c.asm_flag("-m64");
        c.flag("-mcmodel=large").flag("-m64");
    }
}
