use librtun::TunBuilder;

#[tokio::main]
async fn main() {
    env_logger::init();
    let tun = TunBuilder::new().build().unwrap();

    let _ = tun.run().await;
}
