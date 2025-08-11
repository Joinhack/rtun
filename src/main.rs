use librtun::TunBuilder;

#[cfg(feature = "tcmalloc")]
#[global_allocator]
static ALLOC: tcmalloc::TCMalloc = tcmalloc::TCMalloc;

#[tokio::main]
async fn main() {
    env_logger::init();
    let tun = TunBuilder::new().build().unwrap();
    let _ = tun.run().await;
}
