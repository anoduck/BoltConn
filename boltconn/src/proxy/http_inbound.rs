use crate::config::AuthData;
use crate::proxy::Dispatcher;
use anyhow::anyhow;
use base64::Engine;
use httparse::Request;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::sync::atomic::AtomicU8;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

pub struct HttpInbound {
    port: u16,
    server: TcpListener,
    auth: Option<String>,
    dispatcher: Arc<Dispatcher>,
}

impl HttpInbound {
    pub async fn new(
        port: u16,
        auth: Option<AuthData>,
        dispatcher: Arc<Dispatcher>,
    ) -> io::Result<Self> {
        let server =
            TcpListener::bind(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), port)).await?;
        Ok(Self {
            port,
            server,
            auth: auth.map(|auth| auth.username + ":" + auth.password.as_str()),
            dispatcher,
        })
    }

    pub async fn run(self) {
        tracing::info!("[HTTP] Listen proxy at 127.0.0.1:{}, running...", self.port);
        loop {
            match self.server.accept().await {
                Ok((socket, addr)) => {
                    let disp = self.dispatcher.clone();
                    let auth = self.auth.clone();
                    tokio::spawn(Self::serve_connection(socket, auth, addr, disp, None));
                }
                Err(err) => {
                    tracing::error!("HTTP inbound failed to accept: {}", err);
                    return;
                }
            }
        }
    }

    pub(super) async fn serve_connection(
        socket: TcpStream,
        auth: Option<String>,
        addr: SocketAddr,
        dispatcher: Arc<Dispatcher>,
        first_byte: Option<String>,
    ) -> anyhow::Result<()> {
        // get response
        let mut buf_reader = BufReader::new(socket);
        let mut req = if let Some(byte) = first_byte {
            byte
        } else {
            String::new()
        };
        while !req.ends_with("\r\n\r\n") {
            if buf_reader.read_line(&mut req).await? == 0 {
                return Err(anyhow!("EOF"));
            }
            if req.len() > 4096 {
                return Err(anyhow!("Too long resp"));
            }
        }
        let mut socket = buf_reader.into_inner();
        let mut buf = [httparse::EMPTY_HEADER; 16];
        let mut req_struct = Request::new(buf.as_mut());
        req_struct.parse(req.as_bytes())?;
        if req_struct.method.map_or(false, |m| m == "CONNECT")
            // HTTP/1.1
            && req_struct.version.map_or(false, |v| v == 1)
        {
            if let Some(Ok(dest)) = req_struct.path.map(|p| p.parse()) {
                let authorized = if let Some(auth) = auth {
                    // let's verify the auth
                    let mut r = false;
                    for hdr in req_struct.headers.iter() {
                        if hdr.name.eq_ignore_ascii_case("proxy-authorization") {
                            let Ok(value) = std::str::from_utf8(hdr.value)else{
                                break;
                            };
                            // manually split
                            if value.is_ascii() && value.len() > 6 {
                                let (left, right) = value.split_at(6);
                                if left.eq_ignore_ascii_case("basic ") {
                                    let b64decoder = base64::engine::general_purpose::STANDARD;
                                    if let Ok(code) = b64decoder.decode(right) {
                                        if std::str::from_utf8(code.as_slice())
                                            .map(|s| s == auth.as_str())
                                            == Ok(true)
                                        {
                                            r = true;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    r
                } else {
                    true
                };
                if !authorized {
                    socket.write_all(Self::response403().as_bytes()).await?;
                    return Err(anyhow!("Invalid CONNECT request"));
                }
                socket.write_all(Self::response200().as_bytes()).await?;
                let _ = dispatcher
                    .submit_tcp(addr, dest, Arc::new(AtomicU8::new(2)), socket)
                    .await;
                return Ok(());
            }
        }
        socket.write_all(Self::response403().as_bytes()).await?;
        Err(anyhow!("Invalid CONNECT request"))
    }

    const fn response403() -> &'static str {
        "HTTP/1.1 403 Forbidden\r\n\r\n"
    }

    const fn response200() -> &'static str {
        "HTTP/1.1 200 OK\r\n\r\n"
    }
}
