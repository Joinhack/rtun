mod cancelable;
mod cmd;
mod dns_proxy;
mod fakedns;
mod net;
mod option;
mod socks5;
mod tcp;
mod udp;

use anyhow::Result;
use futures::{SinkExt, StreamExt};
use if_watch::IfEvent;
use if_watch::tokio::IfWatcher;
use log::error;
use std::env;
use std::future::Future;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;
use std::{io, pin::Pin};
use tokio::sync::broadcast;
use tun::{
    AbstractDevice, AsyncDevice, Configuration as TunConfiguration, ToAddress, create_as_async,
};

use netstack_lwip as netstack;

use crate::cancelable::{Cancelable, CancelableResult};
use crate::cmd::{
    add_default_ipv4_route, delete_default_ipv4_route, get_default_gw_iface, get_if_addr,
};
use crate::fakedns::{FakeDNS, parse_rules};
use crate::option::{
    NETSTACK_BUFF_SIZE, NETSTACK_UDP_BUFF_SIZE, OUTBOUND_INTERFACES_NAME, TUN_ADDRESS,
    TUN_DEFAULT_NAME, TUN_GATEWAY, TUN_NETMASK,
};
use crate::tcp::TcpHandle;
use crate::udp::UdpHandle;

pub struct TunBuilder {
    tun_config: TunConfiguration,
}

impl TunBuilder {
    pub fn new() -> Self {
        let mut tun_config = TunConfiguration::default();
        tun_config.address(*TUN_ADDRESS);
        tun_config.destination(*TUN_GATEWAY);
        tun_config.netmask(*TUN_NETMASK);
        tun_config.tun_name(TUN_DEFAULT_NAME.as_str());
        tun_config.up();
        Self { tun_config }
    }
    pub fn address<Addr: ToAddress>(&mut self, addr: Addr) -> &mut TunBuilder {
        self.tun_config.address(addr);
        self
    }

    pub fn mtu(&mut self, mtu: u16) -> &mut TunBuilder {
        self.tun_config.mtu(mtu);
        self
    }

    pub fn netmask<Addr: ToAddress>(&mut self, netmask: Addr) -> &mut TunBuilder {
        self.tun_config.netmask(netmask);
        self
    }

    pub fn destination<Addr: ToAddress>(&mut self, destination: Addr) -> &mut TunBuilder {
        self.tun_config.destination(destination);
        self
    }

    pub fn build(&self) -> Result<Tun> {
        let device = match create_as_async(&self.tun_config) {
            Ok(device) => Ok(device),
            Err(tun::Error::Io(e)) => Err(e),
            Err(e) => Err(io::Error::new(io::ErrorKind::Other, e.to_string())),
        }?;
        Ok(Tun::new(device))
    }
}

pub struct Tun {
    device: AsyncDevice,
}

#[derive(Debug)]
struct PostTunSetup {
    gw: String,
    iface: String,
    default_gw: String,
    default_iface: String,
    default_iface_addr: IpAddr,
    notify_tx: broadcast::Sender<()>,
}

impl PostTunSetup {
    fn new(gw: String, iface: String, notify_tx: broadcast::Sender<()>) -> Result<Self> {
        let (default_gw, default_iface) = get_default_gw_iface()?;
        let default_iface_addr = get_if_addr(&default_iface)?;
        Ok(Self {
            gw,
            iface,
            notify_tx,
            default_gw,
            default_iface,
            default_iface_addr,
        })
    }

    /// setup the route
    fn setup(&self) -> Result<()> {
        delete_default_ipv4_route(None)?;
        add_default_ipv4_route(&self.gw, &self.iface, true)?;
        add_default_ipv4_route(&self.default_gw, &self.default_iface, false)?;
        Ok(())
    }

    fn unsetup(&self) -> Result<()> {
        delete_default_ipv4_route(Some(&self.default_iface))?;
        add_default_ipv4_route(&self.default_gw, &self.default_iface, true)?;
        Ok(())
    }

    /// if the if ip changed, reset the the route.
    async fn monitor(self, mut ip_abort_watcher: Cancelable<IfWatcher>) {
        let mut tun_setup = self;
        let mut default_ipaddr = tun_setup.default_iface_addr;
        let mut default_iface_up = true;
        while let Some(CancelableResult::Result(Ok(event))) = ip_abort_watcher.next().await {
            match event {
                IfEvent::Up(_) => {
                    if !default_iface_up {
                        match get_if_addr(&tun_setup.default_iface) {
                            Ok(ip) => {
                                // When the event is received, attempting to get the interface address immediately may fail.
                                tokio::time::sleep(Duration::from_millis(500)).await;
                                if let Ok(rt) = PostTunSetup::new(
                                    tun_setup.gw.clone(),
                                    tun_setup.iface.clone(),
                                    tun_setup.notify_tx.clone(),
                                ) {
                                    default_ipaddr = ip;
                                    default_iface_up = true;
                                    tun_setup = rt;
                                    if let Err(e) = tun_setup.setup() {
                                        error!("error {}", e);
                                    }
                                }
                            }
                            Err(_) => (),
                        };
                    }
                }
                IfEvent::Down(ip_net) => {
                    if default_ipaddr == ip_net.addr() {
                        tun_setup.notify_tx.send(()).unwrap();
                        default_iface_up = false;
                    }
                }
            }
        }
        let _ = tun_setup.unsetup();
    }
}

impl Tun {
    pub fn new(device: AsyncDevice) -> Self {
        Self { device }
    }

    pub async fn run(self) -> Result<()> {
        let destination = match self.device.destination() {
            Ok(a) => a,
            Err(err) => {
                error!("[TUN] failed to get device peer address, error: {}", err);
                return Err(err.into());
            }
        };

        let iface = match self.device.tun_name() {
            Ok(a) => a,
            Err(err) => {
                error!("[TUN] failed to get device name, error: {}", err);
                return Err(err.into());
            }
        };
        let (notify_tx, tcp_notify_rx) = tokio::sync::broadcast::channel(1);
        let udp_notify_rx = notify_tx.subscribe();
        let tun_setup = PostTunSetup::new(destination.to_string(), iface, notify_tx).unwrap();
        tun_setup.setup().unwrap();
        if let Err(_) = env::var(OUTBOUND_INTERFACES_NAME) {
            unsafe {
                env::set_var(
                    OUTBOUND_INTERFACES_NAME,
                    tun_setup.default_iface.to_string(),
                );
            }
        }

        let ip_watcher = IfWatcher::new()?;
        let (ip_abort_watcher, abort_handle) = Cancelable::new(ip_watcher);
        // ip monitor if the ip changed, reset the route with command.
        let monitor_joinable = tokio::spawn(tun_setup.monitor(ip_abort_watcher));
        // initialize the tcp/ip stack, get netstack,tcp,udp stack
        let (stack, mut tcp_listner, udp_socket) =
            netstack::NetStack::with_buffer_size(*NETSTACK_BUFF_SIZE, *NETSTACK_UDP_BUFF_SIZE)?;
        let mut futs: Vec<Pin<Box<dyn Future<Output = ()> + Send>>> = Vec::new();
        let (mut stack_sink, mut stack_stream) = stack.split();
        let dev_frame = self.device.into_framed();
        let (mut tun_sink, mut tun_stream) = dev_frame.split();

        let stack_stream_fut = Box::pin(async move {
            while let Some(pkg) = stack_stream.next().await {
                match pkg {
                    Ok(pkg) => {
                        if let Err(e) = tun_sink.send(pkg).await {
                            error!("device send error :{}", e);
                            return;
                        }
                    }
                    Err(e) => {
                        error!("stack stream next error :{}", e);
                        return;
                    }
                };
            }
        });
        futs.push(stack_stream_fut);
        // read from the tunnel, send the packet from tunnel to the tcp/ip stack.
        let tun_stream_fut = Box::pin(async move {
            while let Some(pkg) = tun_stream.next().await {
                match pkg {
                    Ok(pkg) => {
                        if let Err(e) = stack_sink.send(pkg).await {
                            error!("stack send error :{}", e);
                            return;
                        }
                    }
                    Err(e) => {
                        error!("tun stream next error :{}", e);
                        return;
                    }
                };
            }
        });
        futs.push(tun_stream_fut);
        let fake_dns = FakeDNS::new()?;
        fake_dns.set_filter(parse_rules()?).await;
        let fake_dns = Arc::new(fake_dns);
        let fake_dns_cl = fake_dns.clone();
        // tcp process, recv from tun and process the tcp.
        let tcp_listener_fut = Box::pin(async move {
            let mut tcp_handle = TcpHandle::new(fake_dns_cl, tcp_notify_rx);
            while let Some((stream, local_addr, remote_addr)) = tcp_listner.next().await {
                let _ = tcp_handle
                    .handle_tcp_stream(local_addr, remote_addr, stream)
                    .await;
            }
        });
        futs.push(tcp_listener_fut);
        // udp process, recv from tun and process the udp.
        let udp_socket_fut = Box::pin(async move {
            let udp_handle = UdpHandle::new(fake_dns, udp_notify_rx);
            let _ = udp_handle.handle_udp_socket(udp_socket).await;
        });
        futs.push(udp_socket_fut);

        futs.push(Box::pin(async {
            let _ = tokio::signal::ctrl_c().await;
        }));

        futures::future::select_all(futs).await;
        abort_handle.cancel();
        let _ = monitor_joinable.await;
        Ok(())
    }
}
