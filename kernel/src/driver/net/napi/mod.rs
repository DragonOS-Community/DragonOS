pub mod napi;

// 实现napi机制的驱动需要实现的特征，napi调度器会使用该特征
pub trait NapiDevice{
    fn intr_set(&mut self, state: bool);

    fn pkt_recv(&mut self) -> Option<NapiBuffer>;

}

pub struct NapiBuffer{
    
}