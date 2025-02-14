use tokio::io::{split, AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf};
use std::pin::Pin;
use tokio::net::TcpStream;
use tokio_rustls::TlsStream;

// LOTS, Lord Of The Stream, one stream to rule them all
pub enum UnifiedStream {
    Plain(TcpStream),
    Tls(TlsStream<TcpStream>),
}

pub enum ReadHalfStream {
    Plain(ReadHalf<TcpStream>),
    Tls(ReadHalf<TlsStream<TcpStream>>),
}

impl ReadHalfStream {
    pub async fn read_to_end(&mut self, buf: &mut Vec<u8>) -> tokio::io::Result<usize> {
        match self {
            ReadHalfStream::Plain(ref mut stream) => stream.read_to_end(buf).await,
            ReadHalfStream::Tls(ref mut stream) => Pin::new(stream).read_to_end(buf).await,
        }
    }

    pub async fn read_exact(&mut self, buf: &mut [u8]) -> tokio::io::Result<usize> {
        match self {
            ReadHalfStream::Plain(ref mut stream) => stream.read_exact(buf).await,
            ReadHalfStream::Tls(ref mut stream) => Pin::new(stream).read_exact(buf).await,
        }
    }
}

pub enum WriteHalfStream {
    Plain(WriteHalf<TcpStream>),
    Tls(WriteHalf<TlsStream<TcpStream>>),
}

impl WriteHalfStream {
    pub async fn write_all(&mut self, buf: &[u8]) -> tokio::io::Result<()> {
        match self {
            WriteHalfStream::Plain(ref mut stream) => stream.write_all(buf).await,
            WriteHalfStream::Tls(ref mut stream) => {
                    let mut pinned = Pin::new(stream);            
                    let _ = pinned.write_all(buf).await;
                    pinned.flush().await
            },
        }
    }

    pub async fn shutdown(&mut self) -> tokio::io::Result<()>{
        match self {
            WriteHalfStream::Plain(ref mut stream) => stream.shutdown().await,
            WriteHalfStream::Tls(ref mut stream) => {
                let mut pinned = Pin::new(stream);
                // Ensure all data is written before shutting down
                if let Err(e) = pinned.flush().await {
                    eprintln!("Error flushing TLS stream: {}", e);
                }
                pinned.shutdown().await
            }
        }
    }
}

impl UnifiedStream {
    pub async fn split(self) -> (ReadHalfStream, WriteHalfStream) {
        match self {
            UnifiedStream::Plain(stream) => {
                let (r, w) = split(stream);
                (ReadHalfStream::Plain(r), WriteHalfStream::Plain(w))
            },
            UnifiedStream::Tls(stream) => {
                let (r, w) = split(stream);
                (ReadHalfStream::Tls(r), WriteHalfStream::Tls(w))

            },
        }
    }

    pub async fn read(&mut self, buf: &mut [u8]) -> tokio::io::Result<usize> {
        match self {
            UnifiedStream::Plain(ref mut stream) => stream.read(buf).await,
            UnifiedStream::Tls(ref mut stream) => Pin::new(stream).read(buf).await,
        }
    }

    // pub async fn read_exact(&mut self, buf: &mut [u8]) -> tokio::io::Result<usize> {
    //     match self {
    //         UnifiedStream::Plain(ref mut stream) => stream.read_exact(buf).await,
    //         UnifiedStream::Tls(ref mut stream) => Pin::new(stream).read_exact(buf).await,
    //     }
    // }

    pub async fn write_all(&mut self, buf: &[u8]) -> tokio::io::Result<()> {
        match self {
            UnifiedStream::Plain(ref mut stream) => stream.write_all(buf).await,
            UnifiedStream::Tls(ref mut stream) => {
                    let mut pinned = Pin::new(stream);            
                    let _ = pinned.write_all(buf).await;
                    pinned.flush().await
            },
        }
    }

    pub async fn shutdown(&mut self) -> tokio::io::Result<()> {
        match self {
            UnifiedStream::Plain(ref mut stream) => stream.shutdown().await,
            UnifiedStream::Tls(ref mut stream) => {
                let mut pinned = Pin::new(stream);
                // Ensure all data is written before shutting down
                if let Err(e) = pinned.flush().await {
                    eprintln!("Error flushing TLS stream: {}", e);
                }
                pinned.shutdown().await
            }
        }
    }


}


