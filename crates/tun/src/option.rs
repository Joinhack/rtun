use lazy_static::lazy_static;
use std::{
    env,
    net::{Ipv4Addr, SocketAddr},
    str::FromStr,
};

pub const OUTBOUND_INTERFACES_NAME: &str = "OUTBOUND_INTERFACES";

#[cfg(target_os = "linux")]
static IF_NAME: &str = "tun200";

#[cfg(target_os = "macos")]
static IF_NAME: &str = "utun200";

lazy_static! {
    pub static ref NETSTACK_BUFF_SIZE: usize = get_env_var_or("NETSTACK_BUFF_SIZE", 512);
    pub static ref NETSTACK_UDP_BUFF_SIZE: usize = get_env_var_or("NETSTACK_UDP_BUFF_SIZE", 256);
    pub static ref UDP_RECV_CH_SIZE: usize = get_env_var_or("UDP_RECV_CH_SIZE", 256);
    pub static ref OUTBOUND_CONNECT_TIMEOUT: u64 = get_env_var_or("TCP_CONNECT_TIMEOUT", 10);
    pub static ref TUN_DEFAULT_NAME: String = get_env_var_or("TUN_DEFAULT_NAME", IF_NAME.into());
    pub static ref LINK_COPY_TIMEOUT: u64 = get_env_var_or("LINK_COPY_TIMEOUT", 15);
    pub static ref LINK_BUFFER_SIZE: usize = get_env_var_or("LINK_BUFFER_SIZE", 2);
    pub static ref MAX_UDP_UPSTREAM: u8 = get_env_var_or("MAX_UDP_UPSTREAM", 3);
    pub static ref UDP_SESSION_TIMEOUT: u64 = get_env_var_or("UDP_SESSION_TIMEOUT", 15);
    pub static ref DNS_SESSION_TIMEOUT: u64 = get_env_var_or("DNS_SESSION_TIMEOUT", 5);
    pub static ref TCP_KEEPALIVE_TIMEOUT: u64 = get_env_var_or("TCP_KEEPALIVE_TIMEOUT", 600);
    pub static ref TCP_KEEPALIVE_INTERVAL: u64 = get_env_var_or("TCP_KEEPALIVE_INTERVAL", 60);
    pub static ref TCP_KEEPALIVE_RETRIES: u32 = get_env_var_or("TCP_KEEPALIVE_RETRIES", 5);
    pub static ref GFW_RULE_PATH: String = get_env_var_or("GFW_RULE_PATH", "gfw.txt".to_string());
    pub static ref NETSTACK_OUTPUT_CHANNEL_SIZE: usize =
        get_env_var_or("NETSTACK_OUTPUT_CHANNEL_SIZE", 512);
    pub static ref NETSTACK_TCP_UPLINK_CHANNEL_SIZE: usize =
        get_env_var_or("NETSTACK_TCP_UPLINK_CHANNEL_SIZE", 256);
    pub static ref NETSTACK_UDP_UPLINK_CHANNEL_SIZE: usize =
        get_env_var_or("NETSTACK_UDP_UPLINK_CHANNEL_SIZE", 256);
    pub static ref SOCKS5_ADDR: SocketAddr =
        get_env_var_or("SOCKS5_ADDR", "127.0.0.1:1086".parse().unwrap());
    pub static ref TUN_ADDRESS: Ipv4Addr =
        get_env_var_or("TUN_ADDRESS", "192.16.0.1".parse().unwrap());
    pub static ref TUN_GATEWAY: Ipv4Addr =
        get_env_var_or("TUN_GATEWAY", "192.16.0.1".parse().unwrap());
    pub static ref TUN_NETMASK: Ipv4Addr =
        get_env_var_or("TUN_NETMASK", "255.255.255.0".parse().unwrap());
    pub static ref OUTBOUND_INTERFACES: Vec<String> = {
        let mut vec = Vec::new();
        if let Ok(interfaces) = env::var("OUTBOUND_INTERFACES") {
            vec = interfaces
                .split(",")
                .map(|s| s.trim())
                .map(String::from)
                .collect()
        }
        vec
    };
}

fn get_env_var_or<T>(key: &str, default: T) -> T
where
    T: FromStr,
{
    if let Ok(v) = env::var(key) {
        if let Ok(v) = v.parse() {
            return v;
        }
    }
    default
}
