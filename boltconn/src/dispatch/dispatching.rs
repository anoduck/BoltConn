use crate::adapter::{HttpConfig, ShadowSocksConfig, Socks5Config};
use crate::config::{
    LoadedConfig, ProxySchema, RawProxyGroupCfg, RawProxyLocalCfg, RawProxyProviderOption,
    RawServerAddr, RawServerSockAddr, RawState, RuleAction, RuleConfigLine,
};
use crate::dispatch::action::{Action, SubDispatch};
use crate::dispatch::proxy::ProxyImpl;
use crate::dispatch::rule::{RuleBuilder, RuleOrAction};
use crate::dispatch::ruleset::RuleSet;
use crate::dispatch::temporary::TemporaryList;
use crate::dispatch::{GeneralProxy, Proxy, ProxyGroup, RuleSetTable};
use crate::external::MmdbReader;
use crate::network::dns::Dns;
use crate::platform::process::{NetworkType, ProcessInfo};
use crate::proxy::NetworkAddr;
use crate::transport::trojan::TrojanConfig;
use crate::transport::wireguard::WireguardConfig;
use anyhow::anyhow;
use arc_swap::ArcSwap;
use base64::Engine;
use linked_hash_map::LinkedHashMap;
use regex::Regex;
use shadowsocks::crypto::CipherKind;
use shadowsocks::ServerAddr;
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::str::FromStr;
use std::sync::Arc;
use trust_dns_resolver::config::{NameServerConfig, Protocol, ResolverConfig};

#[derive(Clone, Debug, PartialEq)]
pub enum InboundInfo {
    Tun,
    HttpAny,
    Socks5Any,
    Http(Option<String>),
    Socks5(Option<String>),
}

impl InboundInfo {
    pub fn is_subset_of(&self, rhs: &InboundInfo) -> bool {
        self == rhs
            || match self {
                InboundInfo::Http(_) => matches!(rhs, InboundInfo::HttpAny),
                InboundInfo::Socks5(_) => matches!(rhs, InboundInfo::Socks5Any),
                _ => false,
            }
    }
}

impl FromStr for InboundInfo {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "tun" => Ok(Self::Tun),
            "http" => Ok(Self::HttpAny),
            "socks5" => Ok(Self::Socks5Any),
            s => {
                if s.ends_with("/http") {
                    s.split_once("/http")
                        .map(|(p, _)| Some(p.to_string()))
                        .map(Self::Http)
                        .ok_or(())
                } else if s.ends_with("/socks5") {
                    s.split_once("/socks5")
                        .map(|(p, _)| Some(p.to_string()))
                        .map(Self::Socks5)
                        .ok_or(())
                } else {
                    Err(())
                }
            }
        }
    }
}

pub struct ConnInfo {
    pub src: SocketAddr,
    pub dst: NetworkAddr,
    pub inbound: InboundInfo,
    pub resolved_dst: Option<SocketAddr>,
    pub connection_type: NetworkType,
    pub process_info: Option<ProcessInfo>,
}

impl ConnInfo {
    pub fn socketaddr(&self) -> Option<&SocketAddr> {
        if let NetworkAddr::Raw(s) = &self.dst {
            Some(s)
        } else {
            self.resolved_dst.as_ref()
        }
    }
}

pub struct Dispatching {
    temporary_list: ArcSwap<TemporaryList>,
    templist_builder: DispatchingBuilder,
    proxies: HashMap<String, Arc<Proxy>>,
    groups: LinkedHashMap<String, Arc<ProxyGroup>>,
    snippet: DispatchingSnippet,
}

impl Dispatching {
    pub async fn matches(
        &self,
        info: &mut ConnInfo,
        verbose: bool,
    ) -> (Arc<ProxyImpl>, Option<String>) {
        if let Some(r) = self.temporary_list.load().matches(info, verbose).await {
            r
        } else {
            self.snippet.matches(info, verbose).await
        }
    }

    pub fn update_temporary_list(&self, list: &[RuleConfigLine]) -> anyhow::Result<()> {
        let list = self.templist_builder.build_temporary_list(list)?;
        self.temporary_list.store(Arc::new(list));
        Ok(())
    }

    pub fn set_group_selection(&self, group: &str, proxy: &str) -> anyhow::Result<()> {
        for (name, g) in self.groups.iter() {
            if name == group {
                return g.set_selection(proxy);
            }
        }
        Err(anyhow!("Group not found"))
    }

    pub fn get_group_list(&self) -> Vec<Arc<ProxyGroup>> {
        self.groups.values().cloned().collect()
    }
}

fn stringfy_process(info: &ConnInfo) -> &str {
    match &info.process_info {
        None => "UNKNOWN",
        Some(s) => s.name.as_str(),
    }
}

#[derive(Clone)]
pub struct DispatchingBuilder {
    proxies: HashMap<String, Arc<Proxy>>,
    groups: HashMap<String, Arc<ProxyGroup>>,
    rulesets: HashMap<String, Arc<RuleSet>>,
    group_order: Vec<String>,
    dns: Arc<Dns>,
    mmdb: Option<Arc<MmdbReader>>,
}

impl DispatchingBuilder {
    pub fn empty(dns: Arc<Dns>, mmdb: Option<Arc<MmdbReader>>) -> Self {
        let mut builder = Self {
            proxies: Default::default(),
            groups: Default::default(),
            rulesets: Default::default(),
            group_order: Default::default(),
            dns,
            mmdb,
        };
        builder.proxies.insert(
            "DIRECT".into(),
            Arc::new(Proxy::new("DIRECT", ProxyImpl::Direct)),
        );
        builder.proxies.insert(
            "REJECT".into(),
            Arc::new(Proxy::new("REJECT", ProxyImpl::Reject)),
        );
        builder
    }

    pub fn new(
        dns: Arc<Dns>,
        mmdb: Option<Arc<MmdbReader>>,
        loaded_config: &LoadedConfig,
        ruleset: &RuleSetTable,
    ) -> anyhow::Result<Self> {
        let mut builder = Self::empty(dns, mmdb);
        // start init
        let LoadedConfig {
            config,
            state,
            proxy_schema,
            ..
        } = loaded_config;
        // read all proxies
        builder.parse_proxies(config.proxy_local.iter())?;
        for proxies in proxy_schema.values() {
            builder.parse_proxies(proxies.proxies.iter().map(|c| (&c.name, &c.cfg)))?;
        }

        // read proxy groups
        let mut wg_history = HashMap::new();
        let mut queued_groups = HashSet::new();
        builder.group_order = loaded_config.config.proxy_group.keys().cloned().collect();
        for (name, group) in &config.proxy_group {
            builder.parse_group(
                name,
                state,
                group,
                &config.proxy_group,
                proxy_schema,
                &mut queued_groups,
                &mut wg_history,
                false,
            )?;
        }
        builder.rulesets = ruleset.clone();
        Ok(builder)
    }

    pub fn build_temporary_list(&self, list: &[RuleConfigLine]) -> anyhow::Result<TemporaryList> {
        let (list, fallback) = self.build_rules_loosely(list)?;
        if fallback.is_none() {
            Ok(TemporaryList::new(list))
        } else {
            Err(anyhow::anyhow!("Unexpected Fallback"))
        }
    }

    pub fn build(self, loaded_config: &LoadedConfig) -> anyhow::Result<Dispatching> {
        let (rules, fallback) = self.build_rules(loaded_config.config.rule_local.as_slice())?;

        let groups = {
            let mut g = LinkedHashMap::new();
            for name in &self.group_order {
                // Chain will not be included
                if let Some(val) = self.groups.get(name) {
                    g.insert(name.clone(), val.clone());
                }
            }
            g
        };
        let temporary_list = if let Some(list) = &loaded_config.state.temporary_list {
            self.build_temporary_list(list)?
        } else {
            TemporaryList::empty()
        };
        let proxies = self.proxies.clone();
        Ok(Dispatching {
            temporary_list: ArcSwap::new(Arc::new(temporary_list)),
            templist_builder: self,
            proxies,
            groups,
            snippet: DispatchingSnippet { rules, fallback },
        })
    }

    fn build_rules_loosely(
        &self,
        rules: &[RuleConfigLine],
    ) -> anyhow::Result<(Vec<RuleOrAction>, Option<GeneralProxy>)> {
        let mut rule_builder = RuleBuilder::new(
            self.dns.clone(),
            self.mmdb.clone(),
            &self.proxies,
            &self.groups,
            &self.rulesets,
        );
        for (idx, line) in rules.iter().enumerate() {
            match line {
                RuleConfigLine::Complex(action) => match action {
                    RuleAction::LocalResolve => rule_builder.append_local_resolve(),
                    RuleAction::SubDispatch(sub) => {
                        match rule_builder.parse_incomplete(sub.matches.as_str()) {
                            Ok(matches) => {
                                let (sub_rules, sub_fallback) =
                                    self.build_rules(sub.subrules.as_slice())?;
                                rule_builder.append(RuleOrAction::Action(Action::SubDispatch(
                                    SubDispatch::new(
                                        matches,
                                        DispatchingSnippet {
                                            rules: sub_rules,
                                            fallback: sub_fallback,
                                        },
                                    ),
                                )))
                            }
                            Err(err) => {
                                return Err(anyhow!("Invalid matches {}:{}", sub.matches, err))
                            }
                        }
                    }
                },
                RuleConfigLine::Simple(r) => {
                    if idx == rules.len() - 1 {
                        // check Fallback
                        if let Ok(fallback) = rule_builder.parse_fallback(r.as_str()) {
                            return Ok((rule_builder.emit_all(), Some(fallback)));
                        }
                    }
                    rule_builder
                        .append_literal(r.as_str())
                        .map_err(|e| anyhow!("{} ({:?})", r, e))?;
                }
            }
        }
        Ok((rule_builder.emit_all(), None))
    }

    fn build_rules(
        &self,
        rules: &[RuleConfigLine],
    ) -> anyhow::Result<(Vec<RuleOrAction>, GeneralProxy)> {
        let (list, fallback) = self.build_rules_loosely(rules)?;
        if let Some(fallback) = fallback {
            Ok((list, fallback))
        } else {
            Err(anyhow::anyhow!("Bad rules: missing fallback"))
        }
    }

    /// Build a filter dispatching: for all encountered rule, return DIRECT; otherwise REJECT
    pub fn build_filter(
        self,
        rules: &[String],
        ruleset: &RuleSetTable,
    ) -> anyhow::Result<Dispatching> {
        let mut rule_builder = RuleBuilder::new(
            self.dns.clone(),
            self.mmdb.clone(),
            &self.proxies,
            &self.groups,
            ruleset,
        );
        for r in rules.iter() {
            rule_builder.append_literal((r.clone() + ", DIRECT").as_str())?;
        }
        let rules = rule_builder.emit_all();
        let groups = self
            .groups
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let proxies = self.proxies.clone();
        Ok(Dispatching {
            temporary_list: ArcSwap::new(Arc::new(TemporaryList::empty())),
            templist_builder: self,
            proxies,
            groups,
            snippet: DispatchingSnippet {
                rules,
                fallback: GeneralProxy::Single(Arc::new(Proxy::new("REJECT", ProxyImpl::Reject))),
            },
        })
    }

    fn parse_proxies<'a, I: Iterator<Item = (&'a String, &'a RawProxyLocalCfg)>>(
        &mut self,
        proxies: I,
    ) -> anyhow::Result<()> {
        for (name, proxy) in proxies {
            // avoid duplication
            if self.proxies.contains_key(name) || self.groups.contains_key(name) {
                return Err(anyhow!("Duplicate proxy name:{}", *name));
            }
            let p = match proxy {
                RawProxyLocalCfg::Http { server, port, auth } => Arc::new(Proxy::new(
                    name.clone(),
                    ProxyImpl::Http(HttpConfig {
                        server_addr: NetworkAddr::from(server, *port),
                        auth: auth.clone(),
                    }),
                )),
                RawProxyLocalCfg::Socks5 {
                    server,
                    port,
                    auth,
                    udp,
                } => Arc::new(Proxy::new(
                    name.clone(),
                    ProxyImpl::Socks5(Socks5Config {
                        server_addr: NetworkAddr::from(server, *port),
                        auth: auth.clone(),
                        udp: *udp,
                    }),
                )),
                RawProxyLocalCfg::Shadowsocks {
                    server,
                    port,
                    password,
                    cipher,
                    udp,
                } => {
                    let cipher_kind = match cipher.as_str() {
                        "chacha20-ietf-poly1305" => CipherKind::CHACHA20_POLY1305,
                        "aes-256-gcm" => CipherKind::AES_256_GCM,
                        "aes-128-gcm" => CipherKind::AES_128_GCM,
                        _ => {
                            return Err(anyhow!("Bad Shadowsocks {}: unsupported cipher", *name));
                        }
                    };
                    let addr = match server {
                        RawServerAddr::IpAddr(ip) => {
                            ServerAddr::SocketAddr(SocketAddr::new(*ip, *port))
                        }
                        RawServerAddr::DomainName(dn) => ServerAddr::DomainName(dn.clone(), *port),
                    };
                    Arc::new(Proxy::new(
                        name.clone(),
                        ProxyImpl::Shadowsocks(ShadowSocksConfig {
                            server_addr: addr,
                            password: password.clone(),
                            cipher_kind,
                            udp: *udp,
                        }),
                    ))
                }
                RawProxyLocalCfg::Trojan {
                    server,
                    port,
                    sni,
                    password,
                    skip_cert_verify,
                    websocket_path,
                    udp,
                } => {
                    let addr = match server {
                        RawServerAddr::IpAddr(ip) => NetworkAddr::Raw(SocketAddr::new(*ip, *port)),
                        RawServerAddr::DomainName(dn) => NetworkAddr::DomainName {
                            domain_name: dn.clone(),
                            port: *port,
                        },
                    };
                    Arc::new(Proxy::new(
                        name.clone(),
                        ProxyImpl::Trojan(TrojanConfig {
                            server_addr: addr,
                            password: password.clone(),
                            sni: sni.clone(),
                            skip_cert_verify: *skip_cert_verify,
                            websocket_path: websocket_path.clone(),
                            udp: *udp,
                        }),
                    ))
                }
                RawProxyLocalCfg::Wireguard {
                    local_addr,
                    private_key,
                    public_key,
                    endpoint,
                    mtu,
                    preshared_key,
                    keepalive,
                    dns,
                    reserved,
                } => {
                    let endpoint = match endpoint {
                        RawServerSockAddr::Ip(addr) => NetworkAddr::Raw(*addr),
                        RawServerSockAddr::Domain(a) => {
                            let parts = a.split(':').collect::<Vec<&str>>();
                            let Some(port_str) = parts.get(1) else {
                                return Err(anyhow!("No port"));
                            };
                            let port = port_str.parse::<u16>()?;
                            #[allow(clippy::get_first)]
                            NetworkAddr::DomainName {
                                domain_name: parts.get(0).unwrap().to_string(),
                                port,
                            }
                        }
                    };
                    // parse key
                    let b64decoder = base64::engine::general_purpose::STANDARD;
                    let private_key = {
                        let val = b64decoder.decode(private_key)?;
                        let val: [u8; 32] =
                            val.try_into().map_err(|_| anyhow!("Decode private key"))?;
                        x25519_dalek::StaticSecret::from(val)
                    };
                    let public_key = {
                        let val = b64decoder.decode(public_key)?;
                        let val: [u8; 32] =
                            val.try_into().map_err(|_| anyhow!("Decode public key"))?;
                        x25519_dalek::PublicKey::from(val)
                    };
                    let preshared_key = if let Some(v) = preshared_key {
                        let val = b64decoder.decode(v)?;
                        let val: [u8; 32] = val.try_into().map_err(|_| anyhow!("Decode PSK"))?;
                        Some(val)
                    } else {
                        None
                    };
                    let dns = {
                        let list = String::from("[") + dns.as_str() + "]";
                        let list: Vec<IpAddr> = serde_yaml::from_str(list.as_str())?;
                        let group: Vec<NameServerConfig> = list
                            .into_iter()
                            .map(|i| NameServerConfig::new(SocketAddr::new(i, 53), Protocol::Udp))
                            .collect();
                        ResolverConfig::from_parts(None, vec![], group)
                    };

                    Arc::new(Proxy::new(
                        name.clone(),
                        ProxyImpl::Wireguard(WireguardConfig {
                            ip_addr: *local_addr,
                            private_key,
                            public_key,
                            endpoint,
                            mtu: *mtu,
                            preshared_key,
                            keepalive: *keepalive,
                            dns,
                            reserved: *reserved,
                        }),
                    ))
                }
            };
            self.proxies.insert(name.to_string(), p);
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    // recursion for topological order
    fn parse_group(
        &mut self,
        name: &str,
        state: &RawState,
        proxy_group: &RawProxyGroupCfg,
        proxy_group_list: &LinkedHashMap<String, RawProxyGroupCfg>,
        proxy_schema: &HashMap<String, ProxySchema>,
        queued_groups: &mut HashSet<String>,
        wg_history: &mut HashMap<String, bool>,
        dup_as_error: bool,
    ) -> anyhow::Result<()> {
        if self.groups.contains_key(name)
            || self.proxies.contains_key(name)
            || queued_groups.contains(name)
        {
            return if dup_as_error {
                Err(anyhow!("Duplicate group name {}", name))
            } else {
                // has been processed, just skip
                Ok(())
            };
        }
        if !proxy_group.roughly_validate() {
            return Err(anyhow!("Invalid group {}", name));
        }
        if let Some(chains) = &proxy_group.chains {
            // not proxy group, just chains
            let mut contents = vec![];
            for p in chains.iter().rev() {
                let proxy = self.parse_one_proxy(
                    p,
                    name,
                    state,
                    proxy_group_list,
                    proxy_schema,
                    queued_groups,
                    wg_history,
                )?;
                if let GeneralProxy::Single(px) = &proxy {
                    if px.get_impl().simple_description() == "wireguard"
                        && wg_history.insert(p.clone(), true).is_some()
                    {
                        tracing::warn!("Wireguard {} should not appear in different chains", p);
                    }
                }
                contents.push(proxy);
            }
            self.proxies.insert(
                name.to_string(),
                Arc::new(Proxy::new(name.to_string(), ProxyImpl::Chain(contents))),
            );
            Ok(())
        } else {
            // Genuine proxy group, including only proxies and providers
            let mut arr = Vec::new();
            let mut selection = None;
            // proxies
            for p in proxy_group.proxies.as_ref().unwrap_or(&vec![]) {
                let content = self.parse_one_proxy(
                    p,
                    name,
                    state,
                    proxy_group_list,
                    proxy_schema,
                    queued_groups,
                    wg_history,
                )?;
                if let GeneralProxy::Single(px) = &content {
                    if px.get_impl().simple_description() == "wireguard"
                        && wg_history.insert(p.clone(), false) == Some(true)
                    {
                        tracing::warn!("Wireguard {} should not appear in different chains", p);
                    }
                }
                if p == state.group_selection.get(name).unwrap_or(&String::new()) {
                    selection = Some(content.clone());
                }
                arr.push(content);
            }

            // used providers
            for p in proxy_group.providers.as_ref().unwrap_or(&vec![]) {
                let valid_proxies: Vec<&str> = match p {
                    RawProxyProviderOption::Name(name) => proxy_schema
                        .get(name)
                        .ok_or_else(|| anyhow!("Provider {} not found", name))?
                        .proxies
                        .iter()
                        .map(|entry| entry.name.as_str())
                        .collect(),
                    RawProxyProviderOption::Filter { name, filter } => {
                        let regex = Regex::new(filter).map_err(|_| {
                            anyhow!("provider {} has bad filter: '{}'", name, filter)
                        })?;
                        proxy_schema
                            .get(name)
                            .ok_or_else(|| anyhow!("Provider {} not found", name))?
                            .proxies
                            .iter()
                            .filter_map(|entry| {
                                if regex.is_match(entry.name.as_str()) {
                                    Some(entry.name.as_str())
                                } else {
                                    None
                                }
                            })
                            .collect()
                    }
                };
                for p in valid_proxies {
                    let content = if let Some(single) = self.proxies.get(p) {
                        GeneralProxy::Single(single.clone())
                    } else {
                        return Err(anyhow!("No [{}] in group [{}]", p, name));
                    };
                    if p == state.group_selection.get(name).unwrap_or(&String::new()) {
                        selection = Some(content.clone());
                    }
                    arr.push(content);
                }
            }
            if arr.is_empty() {
                // No available proxies, skip
                return Ok(());
            }

            let first = arr.first().unwrap().clone();
            // If there is no selection now, select the first.
            self.groups.insert(
                name.to_string(),
                Arc::new(ProxyGroup::new(
                    name.to_string(),
                    arr,
                    selection.unwrap_or(first),
                    proxy_group.interface.clone(),
                )),
            );
            Ok(())
        }
    }

    // Just to avoid code duplication
    #[allow(clippy::too_many_arguments)]
    fn parse_one_proxy(
        &mut self,
        p: &str,
        name: &str,
        state: &RawState,
        proxy_group_list: &LinkedHashMap<String, RawProxyGroupCfg>,
        proxy_schema: &HashMap<String, ProxySchema>,
        queued_groups: &mut HashSet<String>,
        wg_history: &mut HashMap<String, bool>,
    ) -> anyhow::Result<GeneralProxy> {
        Ok(if let Some(single) = self.proxies.get(p) {
            GeneralProxy::Single(single.clone())
        } else if let Some(group) = self.groups.get(p) {
            GeneralProxy::Group(group.clone())
        } else {
            // toposort
            queued_groups.insert(name.to_string());

            if let Some(sub) = proxy_group_list.get(p) {
                self.parse_group(
                    p,
                    state,
                    sub,
                    proxy_group_list,
                    proxy_schema,
                    queued_groups,
                    wg_history,
                    true,
                )?;
            } else {
                return Err(anyhow!("No [{}] in group [{}]", p, name));
            }

            queued_groups.remove(name);
            if let Some(group) = self.groups.get(p) {
                GeneralProxy::Group(group.clone())
            } else {
                GeneralProxy::Single(self.proxies.get(p).unwrap().clone())
            }
        })
    }
}

pub struct DispatchingSnippet {
    rules: Vec<RuleOrAction>,
    fallback: GeneralProxy,
}

impl DispatchingSnippet {
    pub async fn matches(
        &self,
        info: &mut ConnInfo,
        verbose: bool,
    ) -> (Arc<ProxyImpl>, Option<String>) {
        for v in &self.rules {
            match v {
                RuleOrAction::Rule(v) => {
                    if let Some(proxy) = v.matches(info) {
                        return Self::proxy_filtering(
                            &proxy,
                            info,
                            v.to_string().as_str(),
                            verbose,
                        );
                    }
                }
                RuleOrAction::Action(a) => match a {
                    Action::LocalResolve(r) => r.resolve_to(info).await,
                    Action::SubDispatch(sub) => {
                        if let Some(r) = sub.matches(info, verbose).await {
                            return r;
                        }
                    }
                },
            }
        }
        Self::proxy_filtering(&self.fallback, info, "Fallback", verbose)
    }

    pub fn proxy_filtering(
        proxy: &GeneralProxy,
        info: &ConnInfo,
        rule_str: &str,
        verbose: bool,
    ) -> (Arc<ProxyImpl>, Option<String>) {
        let (proxy_impl, iface) = proxy.get_impl();
        if !proxy_impl.support_udp() && info.connection_type == NetworkType::Udp {
            if verbose {
                tracing::info!(
                    "[{}]({}) {} => {}: Failed(UDP disabled)",
                    rule_str,
                    stringfy_process(info),
                    info.dst,
                    proxy
                );
            }
            return (Arc::new(ProxyImpl::Reject), None);
        }
        if verbose {
            tracing::info!(
                "[{}]({}) {} => {}",
                rule_str,
                stringfy_process(info),
                info.dst,
                proxy
            );
        }
        (proxy_impl, iface)
    }
}
