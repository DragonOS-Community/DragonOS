use alloc::vec::Vec;
use system_error::SystemError;
use xz4rust::{XzDecoder, XzNextBlockResult};

#[allow(dead_code)]
pub fn xz_decompress(compressed_data: &[u8]) -> Result<Vec<u8>, SystemError> {
    let mut decompressed_data = Vec::new();

    let initial_alloc_size = xz4rust::DICT_SIZE_MIN;
    let max_alloc_size = xz4rust::DICT_SIZE_MAX;
    let mut decoder = XzDecoder::in_heap_with_alloc_dict_size(initial_alloc_size, max_alloc_size);

    let mut input_position = 0usize;
    loop {
        let mut temp_buffer = [0u8; 4096];
        match decoder.decode(&compressed_data[input_position..], &mut temp_buffer) {
            Ok(XzNextBlockResult::NeedMoreData(input_consumed, output_produced)) => {
                input_position += input_consumed;
                decompressed_data.extend_from_slice(&temp_buffer[..output_produced]);
            }
            Ok(XzNextBlockResult::EndOfStream(_, output_produced)) => {
                decompressed_data.extend_from_slice(&temp_buffer[..output_produced]);
                break;
            }
            Err(err) => {
                log::error!("Decompression failed {}", err);
                return Err(SystemError::E2BIG);
            }
        };
    }

    log::info!("XZ Decompress success!");

    Ok(decompressed_data)
}
