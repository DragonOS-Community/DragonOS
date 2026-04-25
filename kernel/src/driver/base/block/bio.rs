use alloc::{boxed::Box, sync::Arc, vec::Vec};
use system_error::SystemError;

use crate::{libs::spinlock::SpinLock, mm::dma::DmaBuffer, sched::completion::Completion};

use super::block_device::{BlockId, LBA_SIZE};

/// BIO操作类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BioType {
    Read,
    Write,
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
}

type BioCompleteCallback = Box<dyn Fn(Result<usize, SystemError>) + Send + Sync>;

impl BioRequest {
    /// 创建一个读请求
    pub fn new_read(lba_start: BlockId, count: usize) -> Arc<Self> {
        let buffer = DmaBuffer::alloc_bytes(count * LBA_SIZE, Default::default());
        Arc::new(Self {
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
            }),
        })
    }

    /// 创建一个写请求
    pub fn new_write(lba_start: BlockId, count: usize, data: &[u8]) -> Arc<Self> {
        let mut buffer = DmaBuffer::alloc_bytes(count * LBA_SIZE, Default::default());
        let copy_len = data.len().min(buffer.len());
        buffer.as_mut_slice()[..copy_len].copy_from_slice(&data[..copy_len]);

        Arc::new(Self {
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
            Some(Ok(_)) => Ok(inner.buffer.to_vec()),
            Some(Err(e)) => Err(e.clone()),
            None => Err(SystemError::EIO),
        }
    }
}
