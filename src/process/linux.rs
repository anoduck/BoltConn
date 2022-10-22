use crate::process::{NetworkType, ProcessInfo};
use netlink_packet_sock_diag::{
    constants::*,
    inet::{ExtensionFlags, InetRequest, SocketId, StateFlags},
    NetlinkBuffer, NetlinkHeader, NetlinkMessage, NetlinkPayload, SockDiagMessage,
};
use netlink_sys::protocols::NETLINK_SOCK_DIAG;
use netlink_sys::Socket;
use std::{
    fs::DirEntry,
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    path::PathBuf,
};
use std::{io, mem::MaybeUninit};
use std::{io::Result, os::unix::prelude::MetadataExt};

fn get_inode_and_uid(addr: SocketAddr, net_type: NetworkType) -> Result<(u32, u32)> {
    // use sock_diag to get inode and uid
    let mut diag_sock = Socket::new(NETLINK_SOCK_DIAG)?;
    diag_sock.bind_auto()?;
    diag_sock.connect(&netlink_sys::SocketAddr::new(0, 0))?;

    let mut packet = NetlinkMessage {
        header: NetlinkHeader {
            flags: NLM_F_REQUEST | NLM_F_DUMP,
            ..Default::default()
        },
        payload: SockDiagMessage::InetRequest(InetRequest {
            family: AF_INET,
            protocol: match net_type {
                NetworkType::TCP => IPPROTO_TCP,
                NetworkType::UDP => IPPROTO_UDP,
            },
            extensions: ExtensionFlags::empty(),
            states: StateFlags::all(),
            socket_id: SocketId {
                source_port: addr.port(),
                source_address: addr.ip(),
                ..Default::default()
            },
        }),
    };
    packet.finalize();
    let mut buf = vec![0; packet.header.length as usize];
    packet.serialize(&mut buf[..]);

    let mut receive_buffer = vec![0; 4096];
    let mut offset = 0;
    while let Ok(size) = diag_sock.recv(&mut &mut receive_buffer[..], 0) {
        loop {
            let bytes = &receive_buffer[offset..];
            let rx_packet = <NetlinkMessage<SockDiagMessage>>::deserialize(bytes).unwrap();

            match rx_packet.payload {
                NetlinkPayload::Noop | NetlinkPayload::Ack(_) => {}
                NetlinkPayload::InnerMessage(SockDiagMessage::InetResponse(response)) => {
                    return Ok((response.header.inode, response.header.uid));
                }
                _ => return Err(io::ErrorKind::InvalidData),
            }

            offset += rx_packet.header.length as usize;
            if offset == size || rx_packet.header.length == 0 {
                offset = 0;
                break;
            }
        }
    }
    Err(io::ErrorKind::InvalidData)
}

fn read_proc(fd: Result<DirEntry>, name: &str) -> Result<bool> {
    let fd = fd?;
    let meta = fd.metadata()?;
    if meta.is_symlink() {
        let link = std::fs::read_link(fd.path())?;
        if link.to_string_lossy() == name {
            return Ok(true);
        }
    }
    Ok(false)
}

pub fn get_pid(addr: SocketAddr, net_type: NetworkType) -> Result<libc::pid_t> {
    let (inode, uid) = get_inode_and_uid(addr, net_type)?;
    let target_name = format!("socket:[{}]", inode);
    for proc in std::fs::read_dir("/proc")? {
        if let Ok(proc) = proc {
            if !proc
                .file_name()
                .to_string_lossy()
                .chars()
                .all(char::is_numeric)
            {
                continue;
            }
            if let Ok(meta) = proc.metadata() {
                if !(meta.uid() == uid && meta.is_dir()) {
                    continue;
                }
                // read fds to search for socket:[]
                let fd_path = proc.path();
                fd_path.push("fd");
                if let Ok(internal) = std::fs::read_dir(fd_path) {
                    for fd in internal {
                        if let Ok(true) = read_proc(fd, &target_name) {
                            return Ok(proc
                                .file_name()
                                .to_string_lossy()
                                .chars()
                                .as_str()
                                .parse()
                                .unwrap());
                        }
                    }
                }
            }
        }
    }
}

pub fn get_process_info(pid: i32) -> Option<ProcessInfo> {
    let exe_link = format!("/proc/{}/exe", pid);
    if let Ok(path) = std::fs::read_link(exe_link) {
        let path = path.into_os_string().into_string().unwrap();
        if let Ok(name) = std::fs::read(format!("/proc/{}/comm", pid)) {
            return Some(ProcessInfo {
                pid,
                path,
                name: String::from_utf8_lossy(name.as_slice()),
            });
        }
    }
    None
}

