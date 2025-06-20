use std::net::IpAddr;

use fake_dns::{DnsServer, DnsServerBuilder};
#[tokio::main]
async fn main() {
    let dns_reomte_addr: IpAddr = "8.8.8.8".parse().unwrap();
    let server: DnsServer = DnsServerBuilder::new(("127.0.0.1".parse().unwrap(), 1553))
        .udp_remote(Some((dns_reomte_addr, 53)))
        .build()
        .await
        .unwrap();
    let _rs = server.run().await;
}
