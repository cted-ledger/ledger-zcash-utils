use anyhow::{anyhow, Result};
use std::time::Duration;
use tonic::transport::Channel;
use zcash_client_backend::proto::service::{
    compact_tx_streamer_client::CompactTxStreamerClient, ChainSpec,
};

/// Timeout for establishing the TCP+TLS connection.
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Timeout applied to every unary RPC call (GetLatestBlock, GetTransaction, …).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Establish a TLS-secured gRPC channel to a lightwalletd / Zaino endpoint.
///
/// # Errors
///
/// Returns an error if the URL is invalid, the TLS handshake fails, or the
/// connection cannot be established within [`CONNECT_TIMEOUT`].
pub async fn connect(grpc_url: &str) -> Result<Channel> {
    tonic::transport::Channel::from_shared(grpc_url.to_owned())
        .map_err(|e| anyhow!("invalid gRPC URL: {}", e))?
        .tls_config(tonic::transport::ClientTlsConfig::new().with_enabled_roots())
        .map_err(|e| anyhow!("TLS config failed: {}", e))?
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .connect()
        .await
        .map_err(|e| anyhow!("gRPC connect failed: {}", e))
}

/// Query the current chain tip height from a lightwalletd endpoint.
///
/// # Errors
///
/// Returns an error if the connection fails or the RPC call is rejected.
pub async fn chain_tip(grpc_url: String) -> Result<u32> {
    let channel = connect(&grpc_url).await?;
    let mut client: CompactTxStreamerClient<Channel> = CompactTxStreamerClient::new(channel);
    let latest = client
        .get_latest_block(ChainSpec {})
        .await
        .map_err(|e| anyhow!("GetLatestBlock failed: {}", e))?
        .into_inner();
    Ok(latest.height as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn connect_rejects_malformed_url() {
        let err = connect("definitely not a url !!!").await.unwrap_err();
        assert!(
            err.to_string().contains("invalid gRPC URL"),
            "unexpected error: {err}"
        );
    }

    /// Verifies that `connect()` fails promptly with a clear error when the
    /// TCP port is not listening (ECONNREFUSED), instead of hanging.
    #[tokio::test]
    async fn connect_fails_fast_on_refused_port() {
        // Bind then immediately drop to get a port guaranteed to be closed.
        let addr = {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let a = l.local_addr().unwrap();
            drop(l);
            a
        };
        let url = format!("https://127.0.0.1:{}", addr.port());

        let err = connect(&url).await.unwrap_err();
        assert!(
            err.to_string().contains("gRPC connect failed"),
            "unexpected error: {err}"
        );
    }

    /// Verifies that `connect()` does not hang indefinitely when a server
    /// accepts the TCP connection but never completes the TLS handshake.
    /// The connect timeout must fire within [`CONNECT_TIMEOUT`].
    #[tokio::test]
    async fn connect_times_out_when_server_stalls_after_tcp_accept() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        // Accept connections but never send a TLS ServerHello.
        tokio::spawn(async move {
            loop {
                if let Ok((_sock, _)) = listener.accept().await {
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                }
            }
        });

        tokio::time::pause();

        let connect_fut = tokio::spawn(connect(
            format!("https://127.0.0.1:{port}").leak(),
        ));

        // Let the task start and register its timers, then advance past CONNECT_TIMEOUT.
        tokio::task::yield_now().await;
        tokio::time::advance(CONNECT_TIMEOUT + Duration::from_millis(1)).await;
        tokio::task::yield_now().await;

        let err = connect_fut.await.unwrap().unwrap_err();
        assert!(
            err.to_string().contains("gRPC connect failed"),
            "unexpected error: {err}"
        );
    }
}
