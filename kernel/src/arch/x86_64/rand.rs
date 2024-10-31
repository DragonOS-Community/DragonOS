use core::arch::x86_64::_rdtsc;

pub fn rand() -> usize {
    return unsafe { (_rdtsc() * _rdtsc() + 998244353_u64 * _rdtsc()) as usize };
}

//TODO move it out from arch module
pub fn rand_bytes<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    let mut remaining = N;
    let mut index = 0;

    while remaining > 0 {
        let random_num = rand();
        let random_bytes = random_num.to_le_bytes();

        let to_copy = core::cmp::min(remaining, size_of::<usize>());
        bytes[index..index + to_copy].copy_from_slice(&random_bytes[..to_copy]);

        index += to_copy;
        remaining -= to_copy;
    }

    bytes
}
