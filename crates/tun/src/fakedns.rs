use anyhow::{Result, bail};
use log::debug;
use std::{
    collections::HashMap,
    net::{Ipv4Addr, SocketAddr},
};
use tokio::sync::RwLock;
use trust_dns_proto::{
    op::{Message, MessageType, OpCode, ResponseCode},
    rr::{DNSClass, RData, RecordType, rdata::A, resource::Record},
};

pub struct FakeDNS(RwLock<FakeDNSInner>);

pub enum DNSProcessResult {
    Response(Vec<u8>),
    // upstream dns will continue process packet.
    Upstream,
}

impl FakeDNS {
    pub fn new() -> Result<Self> {
        Ok(Self(RwLock::new(FakeDNSInner::new()?)))
    }

    pub async fn process_dns_packet(&self, query_vec: &[u8]) -> Result<DNSProcessResult> {
        self.0.write().await.process_dns_packet(query_vec)
    }

    pub async fn add_filter(&self, domain: &str) {
        self.0.write().await.add_filter(domain);
    }

    pub async fn accept(&self, domain: &str) -> bool {
        self.0.read().await.accept(domain)
    }

    pub async fn is_fake_ip(&self, addr: SocketAddr) -> bool {
        self.0.read().await.is_fake_ip(addr)
    }

    pub async fn query_ip(&self, domain: &str) -> Option<Ipv4Addr> {
        self.0.read().await.query_ip(domain)
    }

    pub async fn query_domain(&self, ip: u32) -> Option<String> {
        self.0.read().await.query_domain(ip)
    }
}

#[derive(Default)]
struct FakeDNSInner {
    ip_2_domain: HashMap<u32, String>,
    domain_2_ip: HashMap<String, u32>,
    cursor: u32,
    ttl: u32,
    fake_ip_start: u32,
    fake_ip_end: u32,
    filter: Vec<String>,
}

impl FakeDNSInner {
    fn new() -> Result<Self> {
        let fake_ip_start = Ipv4Addr::new(192, 18, 0, 0).to_bits();
        let fake_ip_end = Ipv4Addr::new(192, 18, 255, 255).to_bits();
        let mut inst = Self {
            fake_ip_start,
            fake_ip_end,
            cursor: fake_ip_start,
            ttl: 1,
            ..Default::default()
        };
        inst.prepare_next_cursor()?;
        Ok(inst)
    }

    /// prepare the next cursor
    fn prepare_next_cursor(&mut self) -> Result<()> {
        for _ in 0..3 {
            self.cursor += 1;
            if self.cursor >= self.fake_ip_end {
                self.cursor = self.fake_ip_start;
            }
            match self.cursor.to_be_bytes()[3] {
                0 | 255 => continue,
                _ => return Ok(()),
            };
        }
        bail!("can't prepare the next cursor, please check the fake ip range.")
    }

    fn is_fake_ip(&self, addr: SocketAddr) -> bool {
        let addr = match addr {
            SocketAddr::V4(addr) => addr.ip().to_bits(),
            SocketAddr::V6(_) => return false,
        };
        self.ip_2_domain.contains_key(&addr)
    }

    fn process_dns_packet(&mut self, query_vec: &[u8]) -> Result<DNSProcessResult> {
        let query_msg = match Message::from_vec(&query_vec) {
            Ok(msg) => msg,
            Err(_) => {
                debug!("invalid dns packet.");
                return Ok(DNSProcessResult::Upstream);
            }
        };

        if query_msg.queries().is_empty() {
            debug!("dns query is empty.");
            return Ok(DNSProcessResult::Upstream);
        }
        let query = &query_msg.queries()[0];

        if query.query_class() != DNSClass::IN {
            debug!("unsupport query class {}", query.query_class());
            return Ok(DNSProcessResult::Upstream);
        }
        match query.query_type() {
            RecordType::A | RecordType::AAAA | RecordType::HTTPS => (),
            _ => {
                debug!("unsupport query type");
                return Ok(DNSProcessResult::Upstream);
            }
        };

        let qname = query.name();
        let raw_name = qname.to_ascii();
        if !self.accept(&raw_name) {
            return Ok(DNSProcessResult::Upstream);
        }
        let domain = if qname.is_fqdn() {
            raw_name[..raw_name.len() - 1].to_string()
        } else {
            raw_name
        };
        let ipaddr = match self.query_ip(&domain) {
            Some(ipaddr) => ipaddr,
            None => self.alloc_fake_ip(&domain)?,
        };
        let mut resp = Message::new();
        resp.set_id(query_msg.id())
            .set_message_type(MessageType::Response)
            .set_op_code(query_msg.op_code());
        if resp.op_code() == OpCode::Query {
            resp.set_recursion_available(query_msg.recursion_available());
            resp.set_recursion_desired(query_msg.recursion_desired());
        }
        resp.set_response_code(ResponseCode::NoError);
        resp.add_query(query.clone());
        if query.query_type() == RecordType::A {
            let mut ans = Record::new();
            ans.set_name(qname.clone())
                .set_ttl(self.ttl)
                .set_rr_type(RecordType::A)
                .set_dns_class(DNSClass::IN)
                .set_data(Some(RData::A(A(ipaddr))));
            resp.add_answer(ans);
        }
        Ok(DNSProcessResult::Response(resp.to_vec()?))
    }

    fn alloc_fake_ip(&mut self, domain: &str) -> Result<Ipv4Addr> {
        if let Some(d) = self.ip_2_domain.insert(self.cursor, domain.to_string()) {
            self.domain_2_ip.remove(&d);
        }
        self.domain_2_ip.insert(domain.to_string(), self.cursor);
        let ipaddr = Ipv4Addr::from_bits(self.cursor);
        self.prepare_next_cursor()?;
        Ok(ipaddr)
    }

    fn add_filter(&mut self, domain: &str) {
        self.filter.push(domain.to_string());
    }

    fn accept(&self, domain: &str) -> bool {
        for f in self.filter.iter() {
            if domain.contains(f) || f == "*" {
                return true;
            }
        }
        return false;
    }

    fn query_domain(&self, ip: u32) -> Option<String> {
        self.ip_2_domain.get(&ip).map(|u| u.to_string())
    }

    fn query_ip(&self, domain: &str) -> Option<Ipv4Addr> {
        self.domain_2_ip
            .get(domain)
            .map(|u| Ipv4Addr::from_bits(*u))
    }
}
