use crate::adapter::{established_tcp, Connector, TcpOutBound, UdpOutBound};
use crate::common::buf_pool::PktBufPool;
use crate::common::duplex_chan::DuplexChan;
use crate::common::io_err;
use crate::network::dns::Dns;
use crate::network::egress::Egress;
use crate::proxy::NetworkAddr;
use io::Result;
use shadowsocks::config::ServerType;
use shadowsocks::{ProxyClientStream, ServerAddr, ServerConfig};
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::task::JoinHandle;

#[derive(Clone)]
pub struct SSOutbound {
    iface_name: String,
    dst: NetworkAddr,
    allocator: PktBufPool,
    dns: Arc<Dns>,
    config: ServerConfig,
}

impl SSOutbound {
    pub fn new(
        iface_name: &str,
        dst: NetworkAddr,
        allocator: PktBufPool,
        dns: Arc<Dns>,
        config: ServerConfig,
    ) -> Self {
        Self {
            iface_name: iface_name.to_string(),
            dst,
            allocator,
            dns,
            config,
        }
    }

    async fn run(self, inbound: Connector) -> Result<()> {
        let target_addr = match self.dst {
            NetworkAddr::Raw(s) => shadowsocks::relay::Address::from(s),
            NetworkAddr::DomainName { domain_name, port } => {
                shadowsocks::relay::Address::from((domain_name, port))
            }
        };
        // ss configs
        let context = shadowsocks::context::Context::new_shared(ServerType::Local);
        let (resolved_config, server_addr) = match self.config.addr().clone() {
            ServerAddr::SocketAddr(p) => (self.config, p),
            ServerAddr::DomainName(domain_name, port) => {
                let resp = self
                    .dns
                    .genuine_lookup(domain_name.as_str())
                    .await
                    .ok_or(io_err("dns not found"))?;
                let addr = SocketAddr::new(resp, port);
                (
                    ServerConfig::new(addr, self.config.password(), self.config.method()),
                    addr.clone(),
                )
            }
        };
        let tcp_conn = match server_addr {
            SocketAddr::V4(_) => {
                Egress::new(&self.iface_name)
                    .tcpv4_stream(server_addr)
                    .await?
            }
            SocketAddr::V6(_) => {
                Egress::new(&self.iface_name)
                    .tcpv6_stream(server_addr)
                    .await?
            }
        };
        let ss_stream =
            ProxyClientStream::from_stream(context, tcp_conn, &resolved_config, target_addr);
        established_tcp(inbound, ss_stream, self.allocator).await;
        Ok(())
    }
}

impl TcpOutBound for SSOutbound {
    fn spawn_tcp(&self, inbound: Connector) -> JoinHandle<Result<()>> {
        tokio::spawn(self.clone().run(inbound))
    }

    fn spawn_tcp_with_chan(&self) -> (DuplexChan, JoinHandle<Result<()>>) {
        let (inner, outer) = Connector::new_pair(10);
        (
            DuplexChan::new(self.allocator.clone(), inner),
            tokio::spawn(self.clone().run(outer)),
        )
    }
}

impl UdpOutBound for SSOutbound {
    fn spawn_udp(&self, inbound: Connector) -> JoinHandle<Result<()>> {
        todo!()
    }

    fn spawn_udp_with_chan(&self) -> (DuplexChan, JoinHandle<Result<()>>) {
        todo!()
    }
}
