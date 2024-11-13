pub mod ps2_dev;
#[cfg(all(target_arch = "x86_64", feature = "driver_ps2_mouse"))]
pub mod ps2_mouse;
pub mod serio;
