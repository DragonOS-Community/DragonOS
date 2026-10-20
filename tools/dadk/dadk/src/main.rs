use dadk::dadk_main;

fn main() {
    logger_init();
    dadk_main();
}

fn logger_init() {
    env_logger::init_from_env(env_logger::Env::default().default_filter_or("info"));
}
