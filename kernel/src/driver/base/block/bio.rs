use alloc::{boxed::Box, sync::Arc, vec::Vec};
use system_error::SystemError;

use crate::{libs::spinlock::SpinLock, mm::dma::DmaBuffer, sched::completion::Completion};

use super::block_device::{BlockId, LBA_SIZE};

/// BIO操作类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BioType {
    Read,
    Write,
    Flush,
}

/// BIO请求状态
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BioState {
    Init,
    Submitted,
    Completed,
    Failed,
}

/// 单个BIO请求
pub struct BioRequest {
    inner: SpinLock<InnerBioRequest>,
}

struct InnerBioRequest {
    bio_type: BioType,
    lba_start: BlockId,
    count: usize,
    buffer: DmaBuffer,
    state: BioState,
    completion: Arc<Completion>,
    result: Option<Result<usize, SystemError>>,
    complete_callbacks: Vec<BioCompleteCallback>,
    /// virtio-drivers返回的token，用于中断时匹配
    token: Option<u16>,
    stats_submit_cycle: usize,
}

type BioCompleteCallback = Box<dyn Fn(Result<usize, SystemError>) + Send + Sync>;

impl BioRequest {
    /// 创建一个读请求
    pub fn new_read(lba_start: BlockId, count: usize) -> Arc<Self> {
        Self::try_new_read(lba_start, count).expect("bio read allocation failed")
    }

    /// Create a read request without panicking when the DMA buffer cannot be
    /// allocated or the request size overflows.
    pub fn try_new_read(lba_start: BlockId, count: usize) -> Result<Arc<Self>, SystemError> {
        let len = Self::validate_request(lba_start, count)?;
        let buffer = DmaBuffer::try_alloc_bytes(len, Default::default())?;
        Ok(Arc::new(Self {
            inner: SpinLock::new(InnerBioRequest {
                bio_type: BioType::Read,
                lba_start,
                count,
                buffer,
                state: BioState::Init,
                completion: Arc::new(Completion::new()),
                result: None,
                complete_callbacks: Vec::new(),
                token: None,
                stats_submit_cycle: 0,
            }),
        }))
    }

    /// 创建一个写请求
    pub fn new_write(lba_start: BlockId, count: usize, data: &[u8]) -> Arc<Self> {
        Self::try_new_write(lba_start, count, data).expect("bio write allocation failed")
    }

    /// Create a write request with exact-length validation and fallible DMA
    /// allocation. A mismatched payload is never silently truncated or padded.
    pub fn try_new_write(
        lba_start: BlockId,
        count: usize,
        data: &[u8],
    ) -> Result<Arc<Self>, SystemError> {
        let len = Self::validate_request(lba_start, count)?;
        if data.len() != len {
            return Err(SystemError::EINVAL);
        }
        let mut buffer = DmaBuffer::try_alloc_bytes(len, Default::default())?;
        buffer.as_mut_slice().copy_from_slice(data);

        Ok(Arc::new(Self {
            inner: SpinLock::new(InnerBioRequest {
                bio_type: BioType::Write,
                lba_start,
                count,
                buffer,
                state: BioState::Init,
                completion: Arc::new(Completion::new()),
                result: None,
                complete_callbacks: Vec::new(),
                token: None,
                stats_submit_cycle: 0,
            }),
        }))
    }

    /// Create a new flush request.
    pub fn new_flush() -> Arc<Self> {
        Arc::new(Self {
            inner: SpinLock::new(InnerBioRequest {
                bio_type: BioType::Flush,
                lba_start: 0,
                count: 0,
                buffer: DmaBuffer::alloc_bytes(1, Default::default()),
                state: BioState::Init,
                completion: Arc::new(Completion::new()),
                result: None,
                complete_callbacks: Vec::new(),
                token: None,
                stats_submit_cycle: 0,
            }),
        })
    }

    /// 标记为已提交，设置token
    pub fn mark_submitted(&self, token: u16) -> Result<(), SystemError> {
        let mut inner = self.inner.lock_irqsave();
        if inner.state != BioState::Init {
            return Err(SystemError::EINVAL);
        }
        inner.state = BioState::Submitted;
        inner.token = Some(token);
        Ok(())
    }

    /// 获取缓冲区的可变引用（仅用于提交时）
    pub fn buffer_mut(&self) -> *mut [u8] {
        let mut inner = self.inner.lock_irqsave();
        inner.buffer.as_mut_slice() as *mut [u8]
    }

    /// 获取缓冲区的不可变引用
    pub fn buffer(&self) -> *const [u8] {
        let inner = self.inner.lock_irqsave();
        inner.buffer.as_slice() as *const [u8]
    }

    /// 将数据写入BIO缓冲区（用于同步回退路径）
    pub fn write_buffer(&self, data: &[u8]) {
        let mut inner = self.inner.lock_irqsave();
        let copy_len = data.len().min(inner.buffer.len());
        inner.buffer.as_mut_slice()[..copy_len].copy_from_slice(&data[..copy_len]);
    }

    /// 获取BIO类型
    pub fn bio_type(&self) -> BioType {
        self.inner.lock_irqsave().bio_type
    }

    /// 获取起始LBA
    pub fn lba_start(&self) -> BlockId {
        self.inner.lock_irqsave().lba_start
    }

    /// 获取扇区数
    pub fn count(&self) -> usize {
        self.inner.lock_irqsave().count
    }

    pub fn set_stats_submit_cycle(&self, cycle: usize) {
        self.inner.lock_irqsave().stats_submit_cycle = cycle;
    }

    pub fn stats_submit_cycle(&self) -> usize {
        self.inner.lock_irqsave().stats_submit_cycle
    }

    /// 获取token
    #[allow(dead_code)]
    pub fn token(&self) -> Option<u16> {
        self.inner.lock_irqsave().token
    }

    /// 完成BIO请求
    pub fn complete(&self, result: Result<usize, SystemError>) {
        let (completion, callbacks) = {
            let mut inner = self.inner.lock_irqsave();
            if matches!(inner.state, BioState::Completed | BioState::Failed) {
                return;
            }
            inner.state = if result.is_ok() {
                BioState::Completed
            } else {
                BioState::Failed
            };
            inner.result = Some(result.clone());
            let callbacks = core::mem::take(&mut inner.complete_callbacks);
            (inner.completion.clone(), callbacks)
        };
        for cb in callbacks {
            cb(result.clone());
        }
        completion.complete();
    }

    pub fn on_complete<F>(&self, callback: F)
    where
        F: Fn(Result<usize, SystemError>) + Send + Sync + 'static,
    {
        let mut inner = self.inner.lock_irqsave();
        if let Some(result) = inner.result.clone() {
            callback(result);
            return;
        }
        inner.complete_callbacks.push(Box::new(callback));
    }

    /// 等待BIO完成并返回结果
    pub fn wait(&self) -> Result<Vec<u8>, SystemError> {
        let completion = self.inner.lock_irqsave().completion.clone();

        // 等待完成
        completion.wait_for_completion()?;

        // 获取结果
        let inner = self.inner.lock_irqsave();
        match inner.result.as_ref() {
            Some(Ok(completed)) if *completed == Self::expected_len(&inner) => {
                Ok(inner.buffer.to_vec())
            }
            Some(Ok(_)) => Err(SystemError::EIO),
            Some(Err(e)) => Err(e.clone()),
            None => Err(SystemError::EIO),
        }
    }

    /// Wait for completion without copying the DMA payload. This is the
    /// completion path for writes and flushes.
    pub fn wait_status(&self) -> Result<usize, SystemError> {
        let completion = self.inner.lock_irqsave().completion.clone();
        completion.wait_for_completion()?;

        let inner = self.inner.lock_irqsave();
        match inner.result.as_ref() {
            Some(Ok(completed)) if *completed == Self::expected_len(&inner) => Ok(*completed),
            Some(Ok(_)) => Err(SystemError::EIO),
            Some(Err(e)) => Err(e.clone()),
            None => Err(SystemError::EIO),
        }
    }

    fn validate_request(lba_start: BlockId, count: usize) -> Result<usize, SystemError> {
        if count == 0 {
            return Err(SystemError::EINVAL);
        }
        lba_start.checked_add(count).ok_or(SystemError::EOVERFLOW)?;
        count.checked_mul(LBA_SIZE).ok_or(SystemError::EOVERFLOW)
    }

    fn expected_len(inner: &InnerBioRequest) -> usize {
        match inner.bio_type {
            BioType::Read | BioType::Write => inner.count * LBA_SIZE,
            BioType::Flush => 0,
        }
    }
}
