use std::{ops::Deref, path::PathBuf};

use goblin::elf::Sym;
use log::info;

use crate::app::AppResult;

use super::error::{BackendError, BackendErrorKind};

#[derive(Debug)]
pub struct KernelLoader;

impl KernelLoader {
    pub fn load(kernel: &PathBuf) -> AppResult<KernelMetadata> {
        info!("Loading kernel: {:?}", kernel);
        let kernel_bytes = std::fs::read(kernel)?;
        let elf = goblin::elf::Elf::parse(&kernel_bytes).map_err(|e| {
            BackendError::new(
                BackendErrorKind::KernelLoadError,
                Some(format!("Failed to load kernel: {:?}", e)),
            )
        })?;
        let mut result = KernelMetadata::new(kernel.clone());

        info!("Parsing symbols...");
        for sym in elf.syms.iter() {
            let name = elf.strtab.get_at(sym.st_name).unwrap_or("");
            result.add_symbol(sym.clone(), name.to_string());
        }
        info!("Parsed {} symbols", result.symbols().len());
        info!("Loaded kernel: {:?}", kernel);
        return Ok(result);
    }
}

#[derive(Debug)]
pub struct KernelMetadata {
    pub kernel: PathBuf,
    sym_collection: SymbolCollection,
}

impl KernelMetadata {
    pub fn new(kernel: PathBuf) -> Self {
        Self {
            kernel,
            sym_collection: SymbolCollection::new(),
        }
    }

    pub fn symbols(&self) -> &[Symbol] {
        &self.sym_collection.symbols
    }

    pub fn sym_collection(&self) -> &SymbolCollection {
        &self.sym_collection
    }

    pub fn add_symbol(&mut self, sym: Sym, name: String) {
        self.sym_collection.add_symbol(sym, name);
    }
}

#[derive(Debug)]
pub struct SymbolCollection {
    symbols: Vec<Symbol>,
}

impl SymbolCollection {
    pub fn new() -> Self {
        Self {
            symbols: Vec::new(),
        }
    }

    pub fn add_symbol(&mut self, sym: Sym, name: String) {
        self.symbols.push(Symbol::new(sym, name));
    }

    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.symbols.len()
    }

    pub fn find_by_name(&self, name: &str) -> Option<&Symbol> {
        self.symbols.iter().find(|sym| sym.name() == name)
    }
}

#[derive(Debug, Clone)]
pub struct Symbol {
    sym: Sym,
    name: String,
}

impl Symbol {
    pub fn new(sym: Sym, name: String) -> Self {
        Self { sym, name }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the virtual address of the symbol.
    #[allow(dead_code)]
    pub fn vaddr(&self) -> usize {
        self.sym.st_value as usize
    }

    /// Returns the offset of the symbol in the kernel memory.
    pub fn memory_offset(&self) -> u64 {
        self.sym.st_value & (!0xffff_8000_0000_0000)
    }
}

impl Deref for Symbol {
    type Target = Sym;

    fn deref(&self) -> &Self::Target {
        &self.sym
    }
}
