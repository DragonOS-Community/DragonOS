pub mod ps2_dev;
#[cfg(all(
    target_arch = "x86_64",
    any(feature = "driver_ps2_mouse", feature = "mm_selftest")
))]
pub mod ps2_mouse;
pub mod serio;
