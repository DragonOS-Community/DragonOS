use super::Controller;

#[derive(Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct NoControl;

impl Controller for NoControl {
    fn window(&self) -> usize {
        usize::MAX
    }

    fn cwnd(&self) -> usize {
        0
    }

    fn ssthresh(&self) -> usize {
        usize::MAX
    }
}
