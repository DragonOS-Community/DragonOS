use log::{info, warn};

mod test_cpumask;
mod test_unix_stream;
mod test_unix_stream_pair;

pub struct TestUnit<'a> {
    pub func: fn() -> Result<(), std::io::Error>,
    pub name: &'a str,
}

pub struct Test;

impl Test {
    const TEST_UNITS: [TestUnit<'_>; 3] = [
        TestUnit {
            func: Self::test_unix_stream,
            name: "test_unix_stream",
        },
        TestUnit {
            func: Self::test_unix_stream_pair,
            name: "test_unix_stream_pair",
        },
        TestUnit {
            func: Self::test_cpumask,
            name: "test_cpumask",
        },
    ];

    pub fn run_all_tests() {
        for unit in Self::TEST_UNITS {
            info!("[ start ] {}", unit.name);
            if let Err(e) = (unit.func)() {
                warn!("[ fault ] {}, err: {e}", unit.name);
            } else {
                info!("[success] {}", unit.name);
            }
        }
    }
}

fn main() -> std::io::Result<()> {
    env_logger::Builder::new().filter(None, log::LevelFilter::Debug).init();

    Test::run_all_tests();

    Ok(())
}
