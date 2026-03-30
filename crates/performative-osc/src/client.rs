use anyhow::{Context, Result};
use rosc::{OscMessage, OscPacket, encoder, decoder};
use std::net::SocketAddr;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::time::{timeout, Duration};

const SC_ADDR: &str = "127.0.0.1:57110";
const RECV_ADDR: &str = "127.0.0.1:0"; // OS-assigned port for receiving

pub struct OscClient {
    socket: UdpSocket,
    sc_addr: SocketAddr,
}

impl OscClient {
    pub async fn new() -> Result<Self> {
        let socket = UdpSocket::bind(RECV_ADDR)
            .await
            .context("failed to bind UDP socket")?;
        let sc_addr: SocketAddr = SC_ADDR.parse().unwrap();
        Ok(Self { socket, sc_addr })
    }

    pub async fn send(&self, msg: OscMessage) -> Result<()> {
        let packet = OscPacket::Message(msg);
        let bytes = encoder::encode(&packet).context("failed to encode OSC message")?;
        self.socket
            .send_to(&bytes, self.sc_addr)
            .await
            .context("failed to send OSC message")?;
        Ok(())
    }

    /// Send and wait for a matching reply (by address prefix).
    pub async fn send_recv(
        &self,
        msg: OscMessage,
        reply_addr: &str,
        wait_ms: u64,
    ) -> Result<OscMessage> {
        self.send(msg).await?;
        let deadline = Duration::from_millis(wait_ms);
        let mut buf = vec![0u8; 4096];
        let result = timeout(deadline, async {
            loop {
                let (n, _) = self.socket.recv_from(&mut buf).await?;
                if let Ok((_, OscPacket::Message(reply))) = decoder::decode_udp(&buf[..n]) {
                    if reply.addr.starts_with(reply_addr) {
                        return Ok::<OscMessage, anyhow::Error>(reply);
                    }
                }
            }
        })
        .await
        .context("timed out waiting for OSC reply")?;
        result
    }

    /// Poll /status until scsynth responds or timeout.
    pub async fn wait_for_ready(&self, timeout_ms: u64) -> Result<()> {
        let deadline = Duration::from_millis(timeout_ms);
        let start = std::time::Instant::now();
        loop {
            if start.elapsed() > deadline {
                anyhow::bail!("scsynth did not respond within {}ms", timeout_ms);
            }
            let msg = OscMessage {
                addr: "/status".to_string(),
                args: vec![],
            };
            if self.send(msg).await.is_ok() {
                let mut buf = vec![0u8; 4096];
                match timeout(Duration::from_millis(200), self.socket.recv_from(&mut buf)).await {
                    Ok(Ok((n, _))) => {
                        if let Ok((_, OscPacket::Message(reply))) = decoder::decode_udp(&buf[..n]) {
                            if reply.addr == "/status.reply" {
                                return Ok(());
                            }
                        }
                    }
                    _ => {}
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Receive a single OSC message (used for async response handling).
    pub async fn recv(&self) -> Result<OscMessage> {
        let mut buf = vec![0u8; 65535];
        loop {
            let (n, _) = self.socket.recv_from(&mut buf).await?;
            if let Ok((_, OscPacket::Message(msg))) = decoder::decode_udp(&buf[..n]) {
                return Ok(msg);
            }
        }
    }

    /// Spawn a background listener that forwards all messages to a channel.
    pub fn spawn_listener(socket: UdpSocket, tx: mpsc::Sender<OscMessage>) {
        tokio::spawn(async move {
            let mut buf = vec![0u8; 65535];
            loop {
                if let Ok((n, _)) = socket.recv_from(&mut buf).await {
                    if let Ok((_, OscPacket::Message(msg))) = decoder::decode_udp(&buf[..n]) {
                        let _ = tx.send(msg).await;
                    }
                }
            }
        });
    }
}
