mod cmd;
mod net;
mod option;
mod tcp;
mod udp;

use anyhow::Result;
use futures::{SinkExt, StreamExt};
use log::error;
use std::env;
use std::future::Future;
use std::{io, pin::Pin};
use tun::{
    AbstractDevice, AsyncDevice, Configuration as TunConfiguration, ToAddress, create_as_async,
};

use netstack_lwip as netstack;

use crate::cmd::{add_default_ipv4_route, delete_default_ipv4_route, get_default_gw_iface};
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

struct PostTunSetup {
    gw: String,
    iface: String,
    default_gw: String,
    default_iface: String,
}

impl PostTunSetup {
    fn new(gw: String, iface: String) -> Result<Self> {
        let (default_gw, default_iface) = get_default_gw_iface()?;
        Ok(Self {
            gw,
            iface,
            default_gw,
            default_iface,
        })
    }

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
}

impl Drop for PostTunSetup {
    fn drop(&mut self) {
        self.unsetup().unwrap();
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

        let tun_setup = PostTunSetup::new(destination.to_string(), iface).unwrap();
        tun_setup.setup().unwrap();
        if let Err(_) = env::var(OUTBOUND_INTERFACES_NAME) {
            unsafe {
                env::set_var(
                    OUTBOUND_INTERFACES_NAME,
                    tun_setup.default_iface.to_string(),
                );
            }
        }

        let (stack, mut tcp_listner, udp_socket) =
            netstack::NetStack::with_buffer_size(*NETSTACK_BUFF_SIZE, *NETSTACK_UDP_BUFF_SIZE)?;
        let mut futs: Vec<Pin<Box<dyn Future<Output = ()>>>> = Vec::new();
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
        let tcp_listener_fut = Box::pin(async move {
            while let Some((stream, local_addr, remote_addr)) = tcp_listner.next().await {
                let tcp_handle = TcpHandle::new();
                let _ = tcp_handle
                    .handle_tcp_stream(local_addr, remote_addr, stream)
                    .await;
            }
        });
        futs.push(tcp_listener_fut);
        let udp_socket_fut = Box::pin(async move {
            let udp_handle = UdpHandle::new();
            let _ = udp_handle.handle_udp_socket(udp_socket).await;
        });
        futs.push(udp_socket_fut);

        futs.push(Box::pin(async {
            let _ = tokio::signal::ctrl_c().await;
        }));

        futures::future::select_all(futs).await;
        Ok(())
    }
}
