#![allow(dead_code)]

extern crate core;

use crate::config::{LinkedState, ProxySchema, RawRootCfg, RawState, RuleSchema};
use crate::dispatch::{Dispatching, DispatchingBuilder};
use crate::eavesdrop::{EavesdropModifier, HeaderModManager, UrlModManager};
use crate::external::{ApiServer, StreamLoggerHandle};
use crate::network::configure::TunConfigure;
use crate::network::dns::{extract_address, new_bootstrap_resolver, parse_dns_config};
use crate::proxy::{AgentCenter, HttpCapturer, HttpInbound, Socks5Inbound, TunUdpInbound};
use ipnet::Ipv4Net;
use is_root::is_root;
use network::tun_device::TunDevice;
use network::{
    dns::Dns,
    packet::transport_layer::{TcpPkt, TransLayerPkt, UdpPkt},
};
use platform::get_default_route;
use proxy::Dispatcher;
use proxy::{SessionManager, TunTcpInbound};
use rcgen::{Certificate, CertificateParams, KeyPair};
use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;
use std::{fs, io};
use structopt::StructOpt;
use tokio::select;
use tracing::{event, Level};

mod adapter;
mod common;
mod config;
mod dispatch;
mod eavesdrop;
mod external;
mod network;
mod platform;
mod proxy;
mod transport;

#[derive(Debug, StructOpt)]
#[structopt(name = "boltconn", about = "BoltConn core binary")]
struct Args {
    /// Path of configutation. Default to $HOME/.config/boltconn
    #[structopt(short, long)]
    pub config: Option<PathBuf>,
    /// Path of certificate. Default to ${config}/cert
    #[structopt(long)]
    pub cert: Option<PathBuf>,
}

fn parse_config_path(
    config: &Option<PathBuf>,
    cert: &Option<PathBuf>,
) -> anyhow::Result<(PathBuf, PathBuf)> {
    let config_path = match config {
        None => {
            let home = PathBuf::from(std::env::var("HOME")?);
            home.join(".config").join("boltconn")
        }
        Some(p) => p.clone(),
    };
    let cert_path = match cert {
        None => config_path.join("cert"),
        Some(p) => p.clone(),
    };
    Ok((config_path, cert_path))
}

fn load_cert_and_key(cert_path: &Path) -> anyhow::Result<Certificate> {
    let cert_str = fs::read_to_string(cert_path.join("crt.pem"))?;
    let key_str = fs::read_to_string(cert_path.join("key.pem"))?;
    let key_pair = KeyPair::from_pem(key_str.as_str())?;
    let params = CertificateParams::from_ca_cert_pem(cert_str.as_str(), key_pair)?;
    let cert = Certificate::from_params(params)?;
    Ok(cert)
}

fn state_path(config_path: &Path) -> PathBuf {
    config_path.join("state.yml")
}

async fn load_config(
    config_path: &Path,
) -> anyhow::Result<(
    RawRootCfg,
    RawState,
    HashMap<String, RuleSchema>,
    HashMap<String, ProxySchema>,
)> {
    let config_text = fs::read_to_string(config_path.join("config.yml"))?;
    let raw_config: RawRootCfg = serde_yaml::from_str(&config_text)?;
    let state_text = fs::read_to_string(state_path(config_path))?;
    let raw_state: RawState = serde_yaml::from_str(&state_text)?;

    let rule_schema = tokio::join!(config::read_rule_schema(
        config_path,
        &raw_config.rule_provider,
        false
    ))
    .0?;
    let proxy_schema = tokio::join!(config::read_proxy_schema(
        config_path,
        &raw_config.proxy_provider,
        false
    ))
    .0?;
    Ok((raw_config, raw_state, rule_schema, proxy_schema))
}

fn mapping_rewrite(list: &[String]) -> anyhow::Result<(Vec<String>, Vec<String>)> {
    let mut url_list = vec![];
    let mut header_list = vec![];
    for s in list.iter() {
        if s.starts_with("url,") {
            url_list.push(s.clone());
        } else if s.starts_with("header-req,") || s.starts_with("header-resp,") {
            header_list.push(s.clone());
        } else {
            return Err(anyhow::anyhow!("Unexpected: {}", s));
        }
    }
    Ok((url_list, header_list))
}

fn main() -> ExitCode {
    if !is_root() {
        eprintln!("BoltConn must be run with root privilege.");
        return ExitCode::from(1);
    }
    let args: Args = Args::from_args();
    let (config_path, cert_path) =
        parse_config_path(&args.config, &args.cert).expect("Invalid config path");

    // tokio and tracing
    let rt = tokio::runtime::Runtime::new().expect("Tokio failed to initialize");

    let stream_logger = StreamLoggerHandle::new();
    external::init_tracing(&stream_logger);

    // interface
    let (_, real_iface_name) = get_default_route().expect("failed to get default route");

    // guards
    let _guard = rt.enter();
    let fake_dns_server = "198.18.99.88".parse().unwrap();

    // config-independent components
    let manager = Arc::new(SessionManager::new());
    let stat_center = Arc::new(AgentCenter::new());
    let http_capturer = Arc::new(HttpCapturer::new());
    let hcap_copy = http_capturer.clone();
    let (tun_udp_tx, tun_udp_rx) = flume::unbounded();
    let (udp_tun_tx, udp_tun_rx) = flume::unbounded();

    // Read initial config
    let (config, state, rule_schema, proxy_schema) = match rt.block_on(load_config(&config_path)) {
        Ok((config, state, rs, ps)) => (config, state, rs, ps),
        Err(e) => {
            eprintln!("Load config from {:?} failed: {}", &config_path, e);
            return ExitCode::from(1);
        }
    };

    // initialize resources
    let (dns, dns_ips) = {
        let bootstrap = new_bootstrap_resolver(config.dns.bootstrap.as_slice()).unwrap();
        let group = match rt.block_on(parse_dns_config(&config.dns.nameserver, Some(bootstrap))) {
            Ok(g) => g,
            Err(e) => {
                eprintln!("Parse dns config failed: {}", e);
                return ExitCode::from(1);
            }
        };
        let dns_ips = if config.dns.force_direct_dns {
            Some(extract_address(&group))
        } else {
            None
        };
        (
            Arc::new(Dns::with_config(group).expect("DNS failed to initialize")),
            dns_ips,
        )
    };

    let outbound_iface = if config.interface != "auto" {
        config.interface.clone()
    } else {
        tracing::info!("Auto detected interface: {}", real_iface_name);
        real_iface_name
    };

    let tun = rt.block_on(async {
        let mut tun = TunDevice::open(
            manager.clone(),
            outbound_iface.as_str(),
            dns.clone(),
            fake_dns_server,
            tun_udp_tx,
            udp_tun_rx,
        )
        .expect("fail to create TUN");
        // create tun device
        event!(Level::INFO, "TUN Device {} opened.", tun.get_name());
        tun.set_network_address(Ipv4Net::new(Ipv4Addr::new(198, 18, 0, 1), 16).unwrap())
            .expect("TUN failed to set address");
        tun.up().expect("TUN failed to up");
        tun
    });

    let tun_configure = Arc::new(std::sync::Mutex::new(TunConfigure::new(
        fake_dns_server,
        tun.get_name(),
    )));
    tun_configure
        .lock()
        .unwrap()
        .enable()
        .expect("Failed to enable global setting");

    let nat_addr = SocketAddr::new(
        platform::get_iface_address(tun.get_name()).expect("failed to get tun address"),
        9961,
    );

    let dispatching = {
        let mut builder = DispatchingBuilder::new();
        if let Some(list) = dns_ips {
            builder.direct_prioritize("DNS-PRIO", list);
        }
        let result = match builder.build(&config, &state, &rule_schema, &proxy_schema) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("Parse routing rules failed: {}", e);
                return ExitCode::from(1);
            }
        };
        Arc::new(result)
    };

    // external controller
    let api_dispatching_handler = Arc::new(tokio::sync::RwLock::new(dispatching.clone()));
    let api_port = config.api_port;
    let (sender, mut receiver) = tokio::sync::mpsc::channel::<()>(1);
    let api_server = ApiServer::new(
        config.api_key,
        manager.clone(),
        stat_center.clone(),
        Some(http_capturer.clone()),
        api_dispatching_handler.clone(),
        tun_configure.clone(),
        sender,
        LinkedState {
            state_path: state_path(&config_path),
            state,
        },
        stream_logger,
    );

    let dispatcher = {
        // tls mitm
        let cert = match load_cert_and_key(&cert_path) {
            Ok(cert) => cert,
            Err(e) => {
                eprintln!("Load certs from path {:?} failed: {}", cert_path, e);
                return ExitCode::from(1);
            }
        };
        let eavesdrop_filter = match {
            let builder = DispatchingBuilder::new();
            if let Some(eavesdrop_rules) = config.eavesdrop_rule {
                builder.build_filter(eavesdrop_rules.as_slice(), &rule_schema)
            } else {
                builder.build_filter(vec![].as_slice(), &rule_schema)
            }
        } {
            Ok(m) => m,
            Err(e) => {
                eprintln!("Load eavesdrop rules failed: {}", e);
                return ExitCode::from(1);
            }
        };
        let (url_modifier, hdr_modifier) = if let Some(rewrite_cfg) = &config.rewrite {
            let (url_mod, hdr_mod) = match mapping_rewrite(rewrite_cfg.as_slice()) {
                Ok((url_mod, hdr_mod)) => (url_mod, hdr_mod),
                Err(e) => {
                    eprintln!("Parse url modifier rules, syntax failed: {}", e);
                    return ExitCode::from(1);
                }
            };
            (
                Arc::new(match UrlModManager::new(url_mod.as_slice()) {
                    Ok(m) => m,
                    Err(e) => {
                        eprintln!("Parse url modifier rules, invalid regexes: {}", e);
                        return ExitCode::from(1);
                    }
                }),
                Arc::new(match HeaderModManager::new(hdr_mod.as_slice()) {
                    Ok(m) => m,
                    Err(e) => {
                        eprintln!("Parse header modifier rules, invalid regexes: {}", e);
                        return ExitCode::from(1);
                    }
                }),
            )
        } else {
            (
                Arc::new(UrlModManager::empty()),
                Arc::new(HeaderModManager::empty()),
            )
        };
        Arc::new(Dispatcher::new(
            outbound_iface.as_str(),
            dns.clone(),
            stat_center,
            dispatching,
            cert,
            Box::new(move |pi| {
                Arc::new(EavesdropModifier::new(
                    hcap_copy.clone(),
                    url_modifier.clone(),
                    hdr_modifier.clone(),
                    pi,
                ))
            }),
            Arc::new(eavesdrop_filter),
        ))
    };
    let tun_inbound_tcp = Arc::new(TunTcpInbound::new(
        nat_addr,
        manager.clone(),
        dispatcher.clone(),
        dns.clone(),
    ));
    let tun_inbound_udp = TunUdpInbound::new(
        tun_udp_rx,
        udp_tun_tx,
        dispatcher.clone(),
        manager.clone(),
        dns.clone(),
    );

    // run
    let _mgr_flush_handle = manager.flush_with_interval(Duration::from_secs(30));
    let _tun_inbound_tcp_handle = rt.spawn(async move { tun_inbound_tcp.run().await });
    let _tun_inbound_udp_handle = rt.spawn(async move { tun_inbound_udp.run().await });
    let _tun_handle = rt.spawn(async move { tun.run(nat_addr).await });
    let _api_handle = rt.spawn(async move { api_server.run(api_port).await });
    if let Some(http_port) = config.http_port {
        let dispatcher = dispatcher.clone();
        rt.spawn(async move {
            let http_proxy = HttpInbound::new(http_port, None, dispatcher).await?;
            http_proxy.run().await;
            Ok::<(), io::Error>(())
        });
    }
    if let Some(socks5_port) = config.socks5_port {
        let dispatcher = dispatcher.clone();
        rt.spawn(async move {
            let socks_proxy = Socks5Inbound::new(socks5_port, None, dispatcher).await?;
            socks_proxy.run().await;
            Ok::<(), io::Error>(())
        });
    }

    rt.block_on(async move {
        loop {
            select! {
                _ = tokio::signal::ctrl_c()=>return,
                restart = receiver.recv() => {
                    if restart.is_some(){
                        // try restarting components
                        match reload(&config_path,dns.clone()).await{
                            Ok((dispatching, eavesdrop_filter,url_rewriter,header_rewriter)) => {
                                *api_dispatching_handler.write().await = dispatching.clone();
                                let hcap2 = http_capturer.clone();
                                dispatcher.replace_dispatching(dispatching);
                                dispatcher.replace_eavesdrop_filter(eavesdrop_filter);
                                dispatcher.replace_modifier(Box::new(move |pi| Arc::new(EavesdropModifier::new(hcap2.clone(),url_rewriter.clone(),header_rewriter.clone(),pi))));
                                tracing::info!("Reloaded config successfully");
                            }
                            Err(err)=>{
                                tracing::warn!("Reloading config failed: {}",err);
                            }
                        }
                    } else {
                        return;
                    }
                }
            }
        }
    });
    tracing::info!("Exiting...");
    tun_configure.lock().unwrap().disable();
    rt.shutdown_background();
    ExitCode::from(0)
}

async fn reload(
    config_path: &Path,
    dns: Arc<Dns>,
) -> anyhow::Result<(
    Arc<Dispatching>,
    Arc<Dispatching>,
    Arc<UrlModManager>,
    Arc<HeaderModManager>,
)> {
    let (config, state, rule_schema, proxy_schema) = load_config(config_path).await?;
    let (url_mod, hdr_mod) = if let Some(rewrite) = &config.rewrite {
        let (url, hdr) = mapping_rewrite(rewrite.as_slice())?;
        (
            Arc::new(UrlModManager::new(url.as_slice())?),
            Arc::new(HeaderModManager::new(hdr.as_slice())?),
        )
    } else {
        (
            Arc::new(UrlModManager::empty()),
            Arc::new(HeaderModManager::empty()),
        )
    };
    let bootstrap = new_bootstrap_resolver(config.dns.bootstrap.as_slice())?;
    let group = parse_dns_config(&config.dns.nameserver, Some(bootstrap)).await?;
    let dispatching = {
        let mut builder = DispatchingBuilder::new();
        if config.dns.force_direct_dns {
            builder.direct_prioritize("DNS_PRIO", extract_address(&group));
        }
        Arc::new(builder.build(&config, &state, &rule_schema, &proxy_schema)?)
    };
    let eavesdrop_filter = {
        let builder = DispatchingBuilder::new();
        if let Some(eavesdrop_rule) = config.eavesdrop_rule {
            Arc::new(builder.build_filter(eavesdrop_rule.as_slice(), &rule_schema)?)
        } else {
            Arc::new(builder.build_filter(vec![].as_slice(), &rule_schema)?)
        }
    };
    dns.replace_resolvers(group).await?;
    Ok((dispatching, eavesdrop_filter, url_mod, hdr_mod))
}
