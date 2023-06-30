use crate::dispatch::InboundInfo;
use crate::proxy::manager::SessionManager;
use crate::proxy::{Dispatcher, NetworkAddr};
use crate::Dns;
use std::io::Result;
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::net::TcpListener;

pub struct TunTcpInbound {
    nat_addr: SocketAddr,
    session_mgr: Arc<SessionManager>,
    dispatcher: Arc<Dispatcher>,
    dns: Arc<Dns>,
}

impl TunTcpInbound {
    pub fn new(
        addr: SocketAddr,
        session_mgr: Arc<SessionManager>,
        dispatcher: Arc<Dispatcher>,
        dns: Arc<Dns>,
    ) -> Self {
        Self {
            nat_addr: addr,
            session_mgr,
            dispatcher,
            dns,
        }
    }

    pub async fn run(&self) -> Result<()> {
        let tcp_listener = TcpListener::bind(self.nat_addr).await?;
        tracing::event!(
            tracing::Level::INFO,
            "[NAT] Listen TCP at {}, running...",
            self.nat_addr
        );
        loop {
            // tracing::debug!("Wait for new NAT connection");
            let (socket, addr) = match tcp_listener.accept().await {
                Ok(r) => r,
                Err(e) => {
                    tracing::error!("[NAT] Failed to accept TCP: {}", e);
                    Err(e)?
                }
            };
            // tracing::debug!("Accepted new NAT connection");
            if let Ok((src_addr, dst_addr, indicator)) =
                self.session_mgr.lookup_tcp_session(addr.port())
            {
                let dst_addr = match self.dns.fake_ip_to_domain(dst_addr.ip()) {
                    None => NetworkAddr::Raw(dst_addr),
                    Some(s) => NetworkAddr::DomainName {
                        domain_name: s,
                        port: dst_addr.port(),
                    },
                };
                // tracing::debug!("[NAT] received new connection {}->{}", src_addr, dst_addr);
                if self
                    .dispatcher
                    .submit_tcp(
                        InboundInfo::Tun,
                        src_addr,
                        dst_addr,
                        indicator.clone(),
                        socket,
                    )
                    .await
                    .is_err()
                {
                    indicator.store(0, Ordering::Relaxed)
                };
            } else {
                tracing::warn!("Unexpected: no record found by port {}", addr.port())
            }
        }
    }
}
