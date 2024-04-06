use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom, Write};

fn main() -> std::io::Result<()> {
    let file_size_bytes: u64 = 512;
    let mut file = File::create("large_file")?;
    file.seek(std::io::SeekFrom::Start(file_size_bytes - 1))?;
    file.write_all(&[0])?;
    let mut file = File::open("large_file")?;
    // let mut reader = BufReader::new(file);
    let mut buffer = [0; 512];
    let mut count = 0;
    loop {
        count += 1;
        file.seek(SeekFrom::Start(0))?;
        let bytes_read = file.read_exact(&mut buffer)?;
        if count > 10000 {
            break;
        }
    }
    Ok(())
}
