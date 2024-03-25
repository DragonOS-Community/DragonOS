mod test_unix_stream;
mod test_unix_stream_pair;

use test_unix_stream::test_unix_stream;
use test_unix_stream_pair::test_unix_stream_pair;

fn main() -> std::io::Result<()> {
    if let Err(e) = test_unix_stream() {
        println!("[ fault ] test_unix_stream, err: {}", e);
    } else {
        println!("[success] test_unix_stream");
    }

    if let Err(e) = test_unix_stream_pair() {
        println!("[ fault ] test_unix_stream_pair, err: {}", e);
    } else {
        println!("[success] test_unix_stream_pair");
    }

    Ok(())
}
