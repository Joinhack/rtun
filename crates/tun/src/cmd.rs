use anyhow::Result;
use std::{io::BufRead, net::IpAddr, process::Command};

#[cfg(target_os = "macos")]
pub use macos::*;

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    pub fn get_default_gw_iface() -> Result<(String, String)> {
        let out = Command::new("route").args(&["-n", "get", "1"]).output()?;
        let mut gw = String::new();
        let mut iface = String::new();
        for line in out.stdout.lines() {
            let line = line?;
            if line.contains("gateway") {
                let rs: Vec<_> = line.split_whitespace().collect();
                gw = rs[1].into();
            }
            if line.contains("interface") {
                let rs: Vec<_> = line.split_whitespace().collect();
                iface = rs[1].into();
            }
        }
        Ok((gw, iface))
    }

    pub fn get_if_addr(ifc: &str) -> Result<IpAddr> {
        let out = Command::new("ipconfig")
            .args(&["getifaddr", ifc])
            .output()?;
        let ip = String::from_utf8_lossy(&out.stdout.trim_ascii());
        Ok(ip.parse()?)
    }

    pub fn delete_default_ipv4_route(ifscope: Option<&str>) -> Result<()> {
        let args: &[&str] = if let Some(ifscope) = ifscope {
            &["delete", "-inet", "default", "-ifscope", ifscope]
        } else {
            &["delete", "-inet", "default"]
        };
        Command::new("route")
            .args(args)
            .status()
            .expect("delete route fail.");
        Ok(())
    }

    pub fn add_default_ipv4_route(gw: &str, iface: &str, primary: bool) -> Result<()> {
        if primary {
            Command::new("route")
                .args(&["add", "-inet", "default", &gw])
                .status()
                .expect("set the route error.");
        } else {
            Command::new("route")
                .args(&["add", "-inet", "default", &gw, "-ifscope", &iface])
                .status()
                .expect("set the route error.");
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
pub use linux::*;

#[cfg(target_os = "linux")]
mod linux {

    use super::*;

    pub fn get_default_gw_iface() -> Result<(String, String)> {
        let out = Command::new("ip").args(&["route", "get", "1"]).output()?;
        let mut gw = String::new();
        let mut iface = String::new();
        for line in out.stdout.lines() {
            let line = line?;
            if line.contains("via") {
                let rs = line.split_whitespace().collect::<Vec<_>>();
                gw = rs[2].to_string();
                iface = rs[4].to_string();
            }
        }
        return Ok((gw.to_string(), iface.to_string()));
    }

    pub fn get_if_addr(ifc: &str) -> Result<IpAddr> {
        let out = Command::new("ip")
            .args(&["addr", "show", "dev", ifc])
            .output()?;
        let mut ip = String::new();
        for line in out.stdout.lines() {
            let line = line?;
            if line.contains("inet") {
                let ips = line.split_whitespace().nth(1);
                let ips = ips.unwrap();
                ip = ips.split("/").nth(0).unwrap().to_string();
            }
        }
        Ok(ip.parse()?)
    }

    pub fn add_default_ipv4_route(gw: &str, iface: &str, primary: bool) -> Result<()> {
        if primary {
            Command::new("ip")
                .arg("route")
                .arg("add")
                .arg("default")
                .arg("via")
                .arg(gw.to_string())
                .arg("table")
                .arg("main")
                .status()
                .expect("failed to execute command");
        } else {
            Command::new("ip")
                .arg("route")
                .arg("add")
                .arg("default")
                .arg("via")
                .arg(gw.to_string())
                .arg("dev")
                .arg(iface)
                .arg("table")
                .arg("default")
                .status()
                .expect("failed to execute command");
        }
        Ok(())
    }

    pub fn delete_default_ipv4_route(ifscope: Option<&str>) -> Result<()> {
        if let Some(_ifscope) = ifscope {
            Command::new("ip")
                .arg("route")
                .arg("del")
                .arg("default")
                .arg("table")
                .arg("default")
                .status()
                .expect("failed to execute command");
        } else {
            Command::new("ip")
                .arg("route")
                .arg("del")
                .arg("default")
                .arg("table")
                .arg("main")
                .status()
                .expect("failed to execute command");
        };
        Ok(())
    }
}
