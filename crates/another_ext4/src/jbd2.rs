use crate::prelude::*;

#[allow(unused)]
pub trait Jbd2: Send + Sync + Any + Debug {
    fn load_journal(&mut self);
    fn journal_start(&mut self);
    fn transaction_start(&mut self);
    fn write_transaction(&mut self, block_id: usize, block_data: Vec<u8>);
    fn transaction_stop(&mut self);
    fn journal_stop(&mut self);
    fn recover(&mut self);
}
