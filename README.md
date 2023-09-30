<h1 align="center">
  <img src="./assets/icon.svg" alt="BoltConn" width="192">
    <br/>
    BoltConn
    <br/>
</h1>



<p align="center">
<a href="https://github.com/XOR-op/BoltConn/actions">
<img src="https://img.shields.io/github/actions/workflow/status/XOR-op/BoltConn/check.yml" alt="GitHub Actions">
</a>
<a href="./LICENSE">
<img src="https://img.shields.io/badge/license-GPLv3-blue.svg" alt="License: GPLv3">
</a>
<a href="https://github.com/XOR-op/BoltConn/releases">
<img src="https://img.shields.io/github/v/release/XOR-op/BoltConn?color=00b4f0" alt="Release">
</a>
</p>

A go-to solution for transparent application proxy & firewall with tunneling and MitM, designed with privacy and security in mind. 
All efforts made to make you fully control your network. Experimental webui & desktop client is available in [XOR-op/BoltBoard](https://github.com/XOR-op/BoltBoard).


## Features
- **Fine-grained Traffic Control**: Allow VPN-style global control, or dedicated http/socks5 per-inbound control.
- **Rule-based Blocking**: Block ad/tracking traffic on a per-process/per-website/flexible way.
- **Rule-based Tunneling**: Flexible way to tunnel traffic through http/socks5/shadowsocks/trojan/wireguard outbounds.
- **Audit Traffic**: Audit traffic history by accessing API or dumping into SQLite.
- **Modify HTTPS Data**: Manipulate requests and responses inside HTTPS traffic to redirect, block or modify them. Able to use compatible rules from Clash community.

For the full features, see [features.md](./docs/features.md).

## Getting Started

*Note: more friendly getting-started instructions and relevant codebase are coming soon.*

To get started with BoltConn, follow these simple steps:

1. Download pre-built binaries from [release](https://github.com/XOR-op/BoltConn/releases) or build yourself.
2. Add the path of the binary to `$PATH`.
3. Run BoltConn by typing `sudo boltconn start` in your terminal.

To generate CA certificate:

```bash
boltconn cert -p <your_desired_path>
```

For more information, use `boltconn --help`.

## Documentations
Learn more about BoltConn's architecture, RESTful API, and how it compares to other related projects:

- [design.md](./docs/design.md) explains BoltConn's architecture.
- [restful.md](./docs/restful.md) covers BoltConn's RESTful API.
- [comparison.md](./docs/comparison.md) compares BoltConn with other related projects.
- [features.md](./docs/features.md) lists full features of BoltConn.

## Future Plan
- More rules
  - Wi-Fi SSID
- More MitM configurations
  - modify HTTP body
  - custom scripts
- IPv6 support
- Windows support with Wintun driver

## License
This software is released under the GPL-3.0 license.