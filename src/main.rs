use librtun::TunBuilder;

#[tokio::main]
async fn main() {
    env_logger::init();
    let tun = TunBuilder::new()
        .address("192.16.0.1")
        .destination("192.16.0.1")
        .netmask("255.255.255.0")
        .build()
        .unwrap();
    let _ = tun.run().await;
}
