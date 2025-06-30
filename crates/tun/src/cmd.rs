use anyhow::Result;
use std::{io::BufRead, process::Command};

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
