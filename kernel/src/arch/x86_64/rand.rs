use core::arch::x86_64::_rdtsc;

pub fn rand() -> usize {
    return unsafe { (_rdtsc() * _rdtsc() + 998244353_u64 * _rdtsc()) as usize };
}
