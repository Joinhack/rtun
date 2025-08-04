use log::error;
use std::io;
use std::net::SocketAddr;
use std::os::fd::AsRawFd;

pub fn set_ip_bound_if<S: AsRawFd>(socket: &S, _addr: &SocketAddr, iface: &str) -> io::Result<()> {
    unsafe {
        let ifa = iface.as_bytes();
        let ret = libc::setsockopt(
            socket.as_raw_fd().as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_BINDTODEVICE,
            ifa.as_ptr() as *const libc::c_void,
            ifa.len() as libc::socklen_t,
        );
        if ret < 0 {
            let err = io::Error::last_os_error();
            error!(
                "set IF_BOUND_IF/IPV6_BOUND_IF ifname: {} error: {}",
                iface, err
            );
            return Err(err);
        }
        Ok(())
    }
}
