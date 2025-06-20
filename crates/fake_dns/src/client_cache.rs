use std::{
    collections::{HashMap, VecDeque, hash_map::Entry},
    io::Result as IOResult,
    net::IpAddr,
    time::Duration,
};

use hickory_resolver::proto::{ProtoError, ProtoErrorKind, op::Message};
use tokio::sync::Mutex;

use crate::{options::ConnectOptions, upstream::DnsClient};

type Address = (IpAddr, u16);
#[derive(Hash, PartialEq, Eq, Clone)]
pub enum DnsKey {
    TcpRemote(Address),
    UdpRemote(Address),
}

pub struct DnsClientCache {
    clients: Mutex<HashMap<DnsKey, VecDeque<DnsClient>>>,
    timeout: Duration,
    retry_times: u8,
}

impl DnsClientCache {
    pub fn new(timeout: Duration, retry_times: u8) -> Self {
        Self {
            clients: Mutex::new(HashMap::new()),
            timeout,
            retry_times,
        }
    }

    /// lookup the msg thought the DnsKey, ConnectionOptions and Message
    /// DnsKey: specify the dns server is tcp remote or udp remote, it contain the remote address and port.
    pub async fn lookup_timeout(
        &self,
        r: DnsKey,
        opts: &ConnectOptions,
        msg: Message,
    ) -> Result<Message, ProtoError> {
        let create_fn = async {
            match r {
                DnsKey::TcpRemote(r) => DnsClient::connect_tcp(opts, r).await,
                DnsKey::UdpRemote(r) => DnsClient::connect_udp(opts, r).await,
            }
        };
        let mut dns_client: DnsClient = self.get_or_create_client(&r, create_fn).await?;
        let mut last_err = ProtoErrorKind::NoError.into();
        for _ in 0..self.retry_times {
            match dns_client
                .lookup_timeout(msg.clone(), self.timeout.clone())
                .await
            {
                msg @ Ok(_) => {
                    self.save_dns_client(r, dns_client).await;
                    return msg;
                }
                Err(e) => last_err = e,
            }
        }
        //must have the value, if lookup success already return.
        Err(last_err)
    }

    async fn save_dns_client(&self, key: DnsKey, client: DnsClient) {
        let mut guard = self.clients.lock().await;
        match guard.entry(key) {
            Entry::Occupied(ref mut o) => {
                o.get_mut().push_back(client);
            }
            Entry::Vacant(v) => {
                let mut q = VecDeque::new();
                q.push_back(client);
                v.insert(q);
            }
        };
    }

    pub async fn get_or_create_client<CF>(
        &self,
        remote: &DnsKey,
        create_fn: CF,
    ) -> IOResult<DnsClient>
    where
        CF: Future<Output = IOResult<DnsClient>>,
    {
        let mut guard = self.clients.lock().await;
        if let Some(q) = guard.get_mut(remote) {
            while let Some(client) = q.pop_front() {
                // get the availd connect
                if client.check_connect().await {
                    return Ok(client);
                }
            }
        }
        create_fn.await
    }
}
