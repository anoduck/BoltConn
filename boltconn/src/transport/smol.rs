use crate::adapter::Connector;
use crate::common::buf_pool::{PktBufHandle, PktBufPool, MAX_PKT_SIZE};
use crate::proxy::ConnAbortHandle;
use bytes::{Bytes, BytesMut};
use dashmap::mapref::entry::Entry;
use dashmap::DashMap;
use flume::TryRecvError;
use smoltcp::iface::{Interface, SocketHandle, SocketSet};
use smoltcp::phy::{Device, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::{
    tcp::Socket as SmolTcpSocket, udp::PacketBuffer as UdpSocketBuffer,
    udp::PacketMetadata as UdpPacketMetadata, udp::Socket as SmolUdpSocket,
};
use smoltcp::socket::{tcp::SocketBuffer as TcpSocketBuffer, tcp::State as TcpState};
use smoltcp::time::Instant as SmolInstant;
use smoltcp::wire::{IpCidr, IpEndpoint};
use std::io;
use std::io::ErrorKind;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Notify};

struct TcpConnTask {
    back_tx: mpsc::Sender<Bytes>,
    rx: flume::Receiver<Bytes>,
    handle: SocketHandle,
    abort_handle: ConnAbortHandle,
    remain_to_send: Option<(PktBufHandle, usize)>, // buffer, start_offset
    allocator: PktBufPool,
}

impl TcpConnTask {
    pub fn new(
        connector: Connector,
        handle: SocketHandle,
        abort_handle: ConnAbortHandle,
        allocator: PktBufPool,
        notify: Arc<Notify>,
    ) -> Self {
        let Connector {
            tx: back_tx,
            rx: mut back_rx,
        } = connector;
        let (tx, rx) = flume::unbounded();
        // notify smol when new message comes
        tokio::spawn(async move {
            while let Some(buf) = back_rx.recv().await {
                let _ = tx.send(buf);
                notify.notify_one();
            }
        });
        Self {
            back_tx,
            rx,
            handle,
            abort_handle,
            remain_to_send: None,
            allocator,
        }
    }

    pub async fn try_send(&mut self, socket: &mut SmolTcpSocket<'_>) -> io::Result<bool> {
        let mut has_activity = false;
        // Send data
        if socket.can_send() {
            if let Some((buf, start)) = self.remain_to_send.take() {
                if let Ok(sent) = socket.send_slice(&buf.as_ready()[start..]) {
                    // successfully sent
                    has_activity = true;
                    if start + sent < buf.len {
                        self.remain_to_send = Some((buf, sent + start));
                    } else {
                        self.allocator.release(buf)
                    }
                } else {
                    return Err(ErrorKind::ConnectionAborted.into());
                }
            } else {
                // fetch new data
                match self.rx.try_recv() {
                    Ok(buf) => {
                        if let Ok(sent) = socket.send_slice(buf.as_ready()) {
                            // successfully sent
                            has_activity = true;
                            if sent < buf.len {
                                self.remain_to_send = Some((buf, sent));
                            } else {
                                self.allocator.release(buf);
                            }
                        } else {
                            return Err(ErrorKind::ConnectionAborted.into());
                        }
                    }
                    Err(TryRecvError::Empty) => {}
                    Err(_) => return Err(ErrorKind::ConnectionAborted.into()),
                }
            }
        }
        if socket.state() == TcpState::CloseWait {
            socket.close();
        }
        Ok(has_activity)
    }

    pub async fn try_recv(&self, socket: &mut SmolTcpSocket<'_>) -> bool {
        // Receive data
        if socket.can_recv() && self.back_tx.capacity() > 0 {
            let mut buf = self.allocator.obtain().await;
            if let Ok(size) = socket.recv_slice(buf.as_uninited()) {
                buf.len = size;
                // must not fail because there is only 1 sender
                let _ = self.back_tx.send(buf).await;
                return true;
            }
        }
        false
    }
}

struct UdpConnTask {
    back_tx: mpsc::Sender<PktBufHandle>,
    rx: flume::Receiver<PktBufHandle>,
    handle: SocketHandle,
    abort_handle: ConnAbortHandle,
    allocator: PktBufPool,
    dest: IpEndpoint,
    last_active: Instant,
}

impl UdpConnTask {
    pub fn new(
        connector: Connector,
        dest: IpEndpoint,
        handle: SocketHandle,
        abort_handle: ConnAbortHandle,
        allocator: PktBufPool,
        notify: Arc<Notify>,
    ) -> Self {
        let Connector {
            tx: back_tx,
            rx: mut back_rx,
        } = connector;
        let (tx, rx) = flume::unbounded();
        tokio::spawn(async move {
            while let Some(buf) = back_rx.recv().await {
                let _ = tx.send(buf);
                notify.notify_one();
            }
        });
        Self {
            back_tx,
            rx,
            handle,
            abort_handle,
            allocator,
            dest,
            last_active: Instant::now(),
        }
    }

    pub async fn try_send(&mut self, socket: &mut SmolUdpSocket<'_>) -> io::Result<bool> {
        let mut has_activity = false;
        // Send data
        if socket.can_send() {
            // fetch new data
            match self.rx.try_recv() {
                Ok(buf) => {
                    has_activity = true;
                    if socket.send_slice(buf.as_ready(), self.dest).is_ok() {
                        self.allocator.release(buf);
                        self.last_active = Instant::now();
                    } else {
                        return Err(ErrorKind::ConnectionAborted.into());
                    }
                }
                Err(TryRecvError::Empty) => {}
                Err(_) => return Err(ErrorKind::ConnectionAborted.into()),
            }
        }
        Ok(has_activity)
    }

    pub async fn try_recv(&mut self, socket: &mut SmolUdpSocket<'_>) -> bool {
        // Receive data
        if socket.can_recv() && self.back_tx.capacity() > 0 {
            let mut buf = self.allocator.obtain().await;
            if let Ok((size, ep)) = socket.recv_slice(buf.as_uninited()) {
                if ep == self.dest {
                    buf.len = size;
                    self.last_active = Instant::now();
                    // must not fail because there is only 1 sender
                    let _ = self.back_tx.send(buf).await;
                    return true;
                }
                // discard mismatched packet
            }
        }
        false
    }
}

//          Program -- TCP/UDP -> SmolStack -> IP -- Internet
//                   \ TCP/UDP <- SmolStack <- IP /
pub struct SmolStack {
    tcp_conn: DashMap<u16, TcpConnTask>,
    udp_conn: DashMap<u16, UdpConnTask>,
    allocator: PktBufPool,
    ip_addr: IpAddr,
    ip_device: VirtualIpDevice,
    iface: Interface,
    socket_set: SocketSet<'static>,
    udp_timeout: Duration,
}

impl SmolStack {
    pub fn new(
        iface_ip: IpAddr,
        mut ip_device: VirtualIpDevice,
        allocator: PktBufPool,
        udp_timeout: Duration,
    ) -> Self {
        let config = smoltcp::iface::Config::default();
        let mut iface = Interface::new(config, &mut ip_device);
        iface.update_ip_addrs(|v| {
            let _ = v.insert(0, IpCidr::new(iface_ip.into(), 32));
        });
        Self {
            tcp_conn: Default::default(),
            udp_conn: Default::default(),
            allocator,
            ip_addr: iface_ip,
            ip_device,
            iface,
            socket_set: SocketSet::new(vec![]),
            udp_timeout,
        }
    }

    pub fn drive_iface(&mut self) -> bool {
        self.iface.poll(
            SmolInstant::now(),
            &mut self.ip_device,
            &mut self.socket_set,
        )
    }

    pub fn open_tcp(
        &mut self,
        local_port: u16,
        remote_addr: SocketAddr,
        connector: Connector,
        abort_handle: ConnAbortHandle,
        notify: Arc<Notify>,
    ) -> io::Result<()> {
        match self.tcp_conn.entry(local_port) {
            Entry::Occupied(_) => Err(ErrorKind::AddrInUse.into()),
            Entry::Vacant(e) => {
                // create socket resource
                let tx_buf = TcpSocketBuffer::new(vec![0u8; MAX_PKT_SIZE]);
                let rx_buf = TcpSocketBuffer::new(vec![0u8; MAX_PKT_SIZE]);
                let mut client_socket = SmolTcpSocket::new(rx_buf, tx_buf);
                // Since we are behind kernel's TCP/IP stack, no second Nagle is needed.
                client_socket.set_nagle_enabled(false);

                // connect to remote
                client_socket
                    .connect(
                        self.iface.context(),
                        remote_addr,
                        SocketAddr::new(self.ip_addr, local_port),
                    )
                    .map_err(|_| io::Error::from(ErrorKind::ConnectionRefused))?;

                let handle = self.socket_set.add(client_socket);
                e.insert(TcpConnTask::new(
                    connector,
                    handle,
                    abort_handle,
                    self.allocator.clone(),
                    notify,
                ));
                Ok(())
            }
        }
    }

    pub fn open_udp(
        &mut self,
        local_port: u16,
        remote_addr: SocketAddr,
        connector: Connector,
        abort_handle: ConnAbortHandle,
        notify: Arc<Notify>,
    ) -> io::Result<()> {
        match self.udp_conn.entry(local_port) {
            Entry::Occupied(_) => Err(ErrorKind::AddrInUse.into()),
            Entry::Vacant(e) => {
                let remote_addr = IpEndpoint::from(remote_addr);
                // create socket resource
                let tx_buf = UdpSocketBuffer::new(
                    vec![UdpPacketMetadata::EMPTY; 16],
                    vec![0u8; MAX_PKT_SIZE],
                );
                let rx_buf = UdpSocketBuffer::new(
                    vec![UdpPacketMetadata::EMPTY; 16],
                    vec![0u8; MAX_PKT_SIZE],
                );
                let mut client_socket = SmolUdpSocket::new(rx_buf, tx_buf);

                client_socket
                    .bind(remote_addr)
                    .map_err(|_| io::Error::from(ErrorKind::ConnectionRefused))?;
                let handle = self.socket_set.add(client_socket);

                e.insert(UdpConnTask::new(
                    connector,
                    remote_addr,
                    handle,
                    abort_handle,
                    self.allocator.clone(),
                    notify,
                ));
                Ok(())
            }
        }
    }

    pub async fn poll_all_tcp(&mut self) -> bool {
        let mut has_activity = false;
        for mut item in self.tcp_conn.iter_mut() {
            let socket = self.socket_set.get_mut::<SmolTcpSocket>(item.handle);
            if socket.state() != TcpState::Closed {
                match item.try_send(socket).await {
                    Ok(v) => has_activity |= v,
                    Err(_) => {
                        socket.close();
                        item.abort_handle.cancel().await;
                    }
                }
                has_activity |= item.try_recv(socket).await;
            }
        }
        has_activity
    }

    pub async fn poll_all_udp(&mut self) -> bool {
        let mut has_activity = false;
        for mut item in self.udp_conn.iter_mut() {
            let socket = self.socket_set.get_mut::<SmolUdpSocket>(item.handle);
            if socket.is_open() {
                match item.try_send(socket).await {
                    Ok(v) => has_activity |= v,
                    Err(_) => {
                        socket.close();
                        item.abort_handle.cancel().await;
                    }
                }
                has_activity |= item.try_recv(socket).await;
            }
        }
        has_activity
    }

    pub fn purge_closed_tcp(&mut self) {
        self.tcp_conn.retain(|_port, task| {
            let socket = self.socket_set.get_mut::<SmolTcpSocket>(task.handle);
            if socket.state() == TcpState::Closed {
                self.socket_set.remove(task.handle);
                // Here we only abort normally closed sockets. Maybe unnecessary?
                let c = task.abort_handle.clone();
                tokio::spawn(async move { c.cancel().await });
                false
            } else {
                true
            }
        });
    }

    pub fn purge_timeout_udp(&mut self) {
        self.udp_conn.retain(|_port, task| {
            if task.last_active.elapsed() > self.udp_timeout {
                self.socket_set.remove(task.handle);
                let c = task.abort_handle.clone();
                tokio::spawn(async move { c.cancel().await });
                false
            } else {
                true
            }
        });
    }
}

// -----------------------------------------------------------------------------------

/// Virtual IP device
pub struct VirtualIpDevice {
    mtu: usize,
    outbound: flume::Sender<BytesMut>,
    packet_queue: flume::Receiver<BytesMut>,
}

impl VirtualIpDevice {
    pub fn new(
        mtu: usize,
        inbound: flume::Receiver<BytesMut>,
        outbound: flume::Sender<BytesMut>,
    ) -> Self {
        Self {
            mtu,
            outbound,
            packet_queue: inbound,
        }
    }
}

impl Device for VirtualIpDevice {
    type RxToken<'a> = VirtualRxToken;
    type TxToken<'a> = VirtualTxToken;

    fn receive(
        &mut self,
        _timestamp: SmolInstant,
    ) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        match self.packet_queue.try_recv() {
            Ok(item) => Some((
                VirtualRxToken { buf: item },
                VirtualTxToken {
                    sender: self.outbound.clone(),
                },
            )),
            Err(_) => None,
        }
    }

    fn transmit(&mut self, _timestamp: SmolInstant) -> Option<Self::TxToken<'_>> {
        Some(VirtualTxToken {
            sender: self.outbound.clone(),
        })
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut cap = DeviceCapabilities::default();
        cap.medium = Medium::Ip;
        cap.max_transmission_unit = self.mtu;
        cap
    }
}

pub struct VirtualRxToken {
    buf: BytesMut,
}

impl RxToken for VirtualRxToken {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(mut self, f: F) -> R {
        f(&mut self.buf)
    }
}

pub struct VirtualTxToken {
    sender: flume::Sender<BytesMut>,
}

impl TxToken for VirtualTxToken {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        let mut buf = BytesMut::with_capacity(len);
        // Safety: f exactly writes _len_ bytes, so all bytes are initialized.
        unsafe {
            buf.set_len(len);
        }
        let r = f(&mut buf);
        let _ = self.sender.send(buf);
        r
    }
}
