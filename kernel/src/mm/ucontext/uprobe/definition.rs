use super::*;

/// Linux `struct uprobe` 的定义域：文件对象与文件偏移唯一确定探针，指令只分析一次。
pub struct UprobeDefinition {
    inode: Arc<dyn IndexNode>,
    page_cache: Arc<PageCache>,
    inode_id: usize,
    pub(super) inode_key: usize,
    offset: usize,
    old_instruction: [u8; UPROBE_INSN_COPY_SIZE],
    pub(super) analysis: InsnAnalysis,
}

impl UprobeDefinition {
    pub fn new(inode: Arc<dyn IndexNode>, offset: usize) -> Result<Arc<Self>, SystemError> {
        let metadata = inode.metadata()?;
        let file_size = usize::try_from(metadata.size).map_err(|_| SystemError::EINVAL)?;
        if offset >= file_size {
            return Err(SystemError::EINVAL);
        }
        let inode_id = metadata.inode_id.data();
        let page_cache = inode.page_cache().ok_or(SystemError::EINVAL)?;
        // Mount wrappers and hardlink dentries may expose different IndexNode
        // Arcs for the same underlying inode. The shared page cache is the
        // canonical file-mapping identity used by every mmap/rmap path.
        let inode_key = Arc::as_ptr(&page_cache) as usize;
        {
            let definitions = UPROBE_DEFINITIONS.lock_irqsave();
            if let Some(existing) = definitions
                .get(&(inode_key, offset))
                .and_then(Weak::upgrade)
            {
                return Ok(existing);
            }
        }

        // Linux copies the definition instruction from the file mapping, not
        // from a particular process's possibly private/COW mapping. This also
        // allows a valid instruction to straddle a page or adjacent VMAs.
        let available = (file_size - offset).min(UPROBE_INSN_COPY_SIZE);
        let mut bytes = [0u8; UPROBE_INSN_COPY_SIZE];
        let read = page_cache.read(offset, &mut bytes[..available])?;
        if read == 0 {
            return Err(SystemError::EIO);
        }
        let analysis = analyze_insn(&bytes).map_err(|_| SystemError::EINVAL)?;
        if analysis.insn_len > read {
            return Err(SystemError::EINVAL);
        }
        let mut old_instruction = [0; UPROBE_INSN_COPY_SIZE];
        old_instruction[..analysis.insn_len].copy_from_slice(&bytes[..analysis.insn_len]);

        let definition = Arc::new(Self {
            inode,
            page_cache,
            inode_id,
            inode_key,
            offset,
            old_instruction,
            analysis,
        });
        let mut definitions = UPROBE_DEFINITIONS.lock_irqsave();
        if let Some(existing) = definitions
            .get(&(inode_key, offset))
            .and_then(Weak::upgrade)
        {
            return Ok(existing);
        }
        definitions.insert((inode_key, offset), Arc::downgrade(&definition));
        Ok(definition)
    }

    pub fn inode(&self) -> &Arc<dyn IndexNode> {
        &self.inode
    }

    pub(super) fn matches_inode(&self, inode: &Arc<dyn IndexNode>) -> bool {
        inode
            .page_cache()
            .is_some_and(|page_cache| Arc::ptr_eq(&page_cache, &self.page_cache))
    }

    pub fn inode_id(&self) -> usize {
        self.inode_id
    }

    pub fn offset(&self) -> usize {
        self.offset
    }

    pub(super) fn instruction(&self) -> ([u8; UPROBE_INSN_COPY_SIZE], InsnAnalysis) {
        (self.old_instruction, self.analysis)
    }
}

impl Drop for UprobeDefinition {
    fn drop(&mut self) {
        let key = (self.inode_key, self.offset);
        let self_ptr = core::ptr::from_ref(self);
        let mut definitions = UPROBE_DEFINITIONS.lock_irqsave();
        if definitions
            .get(&key)
            .is_some_and(|weak| core::ptr::eq(weak.as_ptr(), self_ptr))
        {
            definitions.remove(&key);
        }
    }
}

static UPROBE_DEFINITIONS: SpinLock<BTreeMap<(usize, usize), Weak<UprobeDefinition>>> =
    SpinLock::new(BTreeMap::new());
