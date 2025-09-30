use env_logger::Env;
use librtun::TunBuilder;

#[cfg(feature = "tcmalloc")]
#[global_allocator]
static ALLOC: tcmalloc::TCMalloc = tcmalloc::TCMalloc;

#[tokio::main]
async fn main() {
    env_logger::Builder::from_env(Env::default().default_filter_or("info")).init();
    let tun = TunBuilder::new().build().unwrap();
    let _ = tun.run().await;
}
