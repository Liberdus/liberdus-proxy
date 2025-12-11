#[cfg(test)]
mod tests {
    use super::super::proxy::{handle_request, is_monitor_route, PayloadCache};
    use crate::config;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    // Mock stream that writes to a buffer and reads from a buffer
    struct MockStream {
        read_data: std::io::Cursor<Vec<u8>>,
        write_data: Vec<u8>,
    }

    impl MockStream {
        fn new(input: Vec<u8>) -> Self {
            Self {
                read_data: std::io::Cursor::new(input),
                write_data: Vec::new(),
            }
        }
    }

    impl tokio::io::AsyncRead for MockStream {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::pin::Pin::new(&mut self.read_data).poll_read(cx, buf)
        }
    }

    impl tokio::io::AsyncWrite for MockStream {
        fn poll_write(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<Result<usize, std::io::Error>> {
            let len = buf.len();
            self.write_data.extend_from_slice(buf);
            std::task::Poll::Ready(Ok(len))
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), std::io::Error>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), std::io::Error>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[test]
    fn test_is_monitor_route() {
        assert!(is_monitor_route("/style.css"));
        assert!(is_monitor_route("/app.js"));
        assert!(is_monitor_route("/version.js"));
        assert!(is_monitor_route("/fabric.js"));
        assert!(is_monitor_route("/auth.js"));
        assert!(is_monitor_route("/favicon.ico"));
        assert!(is_monitor_route("/axios.min.js"));
        assert!(is_monitor_route("/popmotion.min.js"));
        assert!(is_monitor_route("/api/report"));
        assert!(is_monitor_route("/api/status"));
        assert!(is_monitor_route("/api/version"));
        assert!(is_monitor_route("/milky-way2.png"));
        assert!(is_monitor_route("/logo.png"));
        assert!(is_monitor_route("/"));
        assert!(!is_monitor_route("/unknown"));
        assert!(!is_monitor_route("/api/unknown"));
    }

    #[test]
    fn test_payload_cache() {
        let mut cache = PayloadCache::new(1000);
        assert!(cache.buffer.is_none());
        assert_eq!(cache.lifespan, 1000);
        assert_eq!(cache.timestamp, 0);

        let payload = vec![1, 2, 3];
        cache.set(payload.clone());
        assert_eq!(cache.buffer, Some(payload));
        assert!(cache.timestamp > 0);

        let clone = cache.get();
        assert_eq!(clone.buffer, cache.buffer);
        assert_eq!(clone.timestamp, cache.timestamp);
        assert_eq!(clone.lifespan, cache.lifespan);
    }

    #[tokio::test]
    async fn test_handle_request_cold_fetch_success() {
        // Start a mock upstream server
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0; 1024];
            let _ = socket.read(&mut buf).await;
            let response = "HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nHello";
            let _ = socket.write_all(response.as_bytes()).await;
        });

        let mut config = config::Config::default();
        config.shardus_monitor.upstream_ip = "127.0.0.1".to_string();
        config.shardus_monitor.upstream_port = port;
        let config = Arc::new(config);

        let mut client_stream = MockStream::new(vec![]);

        // Use a route that exists in the cache map
        let route = "/api/status".to_string();

        let res = handle_request(vec![], route.clone(), &mut client_stream, config).await;
        assert!(res.is_ok());

        let response = String::from_utf8(client_stream.write_data).unwrap();
        assert!(response.contains("Hello"));
        assert!(response.contains("HTTP/1.1 200 OK"));
    }

    #[tokio::test]
    async fn test_handle_request_cached() {
        // Start a mock upstream server
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0; 1024];
            let _ = socket.read(&mut buf).await;
            let response = "HTTP/1.1 200 OK\r\nContent-Length: 6\r\n\r\nCached";
            let _ = socket.write_all(response.as_bytes()).await;
        });

        let mut config = config::Config::default();
        config.shardus_monitor.upstream_ip = "127.0.0.1".to_string();
        config.shardus_monitor.upstream_port = port;
        let config = Arc::new(config);

        let mut client_stream = MockStream::new(vec![]);
        let route = "/api/version".to_string(); // Use a different route to avoid conflict/race if run parallel

        // First call populates cache
        let _ = handle_request(vec![], route.clone(), &mut client_stream, config.clone()).await;

        // Reset stream
        let mut client_stream_2 = MockStream::new(vec![]);

        // Second call should use cache (lifespan is long enough)
        // We can't easily verify it didn't hit the network without more sophisticated mocking or checking the server didn't get a connection.
        // But we can check it returns the data.
        let res = handle_request(vec![], route.clone(), &mut client_stream_2, config).await;
        assert!(res.is_ok());

        let response = String::from_utf8(client_stream_2.write_data).unwrap();
        assert!(response.contains("Cached"));
    }

    #[tokio::test]
    async fn test_handle_request_connection_refused() {
        let mut config = config::Config::default();
        config.shardus_monitor.upstream_ip = "127.0.0.1".to_string();
        config.shardus_monitor.upstream_port = 1; // Unlikely to be open
        config.max_http_timeout_ms = 100;
        let config = Arc::new(config);

        let mut client_stream = MockStream::new(vec![]);
        let route = "/style.css".to_string();

        let res = handle_request(vec![], route, &mut client_stream, config).await;
        assert!(res.is_err());

        // Should respond with timeout or internal error depending on where it fails (connect vs timeout)
        let response = String::from_utf8(client_stream.write_data).unwrap();
        assert!(!response.is_empty());
    }
}
