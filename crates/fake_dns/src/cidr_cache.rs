use ipnet::{Ipv4Net, Ipv6Net};
use iprange::IpRange;
use std::{
    io::{self, BufRead, BufReader},
    net::Ipv4Addr,
    path::Path,
};

pub struct CidrCache {
    ipv4: IpRange<Ipv4Net>,
    ipv6: IpRange<Ipv6Net>,
}

impl CidrCache {
    pub fn new() -> Self {
        Self {
            ipv4: IpRange::new(),
            ipv6: IpRange::new(),
        }
    }

    #[inline(always)]
    pub fn add_ipv4(&mut self, v: Ipv4Net) {
        self.ipv4.add(v);
    }

    #[inline(always)]
    pub fn contains_ipv4net(&self, v: &Ipv4Addr) -> bool {
        self.ipv4.contains(v)
    }

    #[inline(always)]
    pub fn contains_ipv6net(&self, v: &Ipv6Net) -> bool {
        self.ipv6.contains(v)
    }

    #[inline(always)]
    pub fn add_ipv6(&mut self, v: Ipv6Net) {
        self.ipv6.add(v);
    }

    pub fn load_from_file<P: AsRef<Path>>(&mut self, path: P) -> io::Result<()> {
        let file = std::fs::File::open(path)?;
        let file = BufReader::new(file);
        for line in file.lines() {
            let line = line?;
            if line.contains(":") {
                let line: Result<Ipv6Net, _> = line.parse();
                let addr = line.map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
                self.add_ipv6(addr);
            } else {
                let line: Result<Ipv4Net, _> = line.parse();
                let addr = line.map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
                self.add_ipv4(addr);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_load() {
        let mut cache = CidrCache::new();
        cache
            .load_from_file("/Users/join/Downloads/CN-ip-cidr.txt")
            .unwrap();
        assert!(cache.contains_ipv4net(&"171.212.241.127".parse::<Ipv4Addr>().unwrap()) == true)
    }
}
