//! A bounded, dependency-free HTTP probe; never opens the database or starts the UI.
use std::{
    io::{BufRead, BufReader, Write},
    net::{SocketAddr, TcpStream},
    time::Duration,
};

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let port = std::env::var("REELARR_PORT").unwrap_or_else(|_| "7979".into());
    let addr = SocketAddr::from(([127, 0, 0, 1], port.parse::<u16>()?));
    probe(addr)?;
    Ok(())
}

fn probe(addr: SocketAddr) -> std::io::Result<()> {
    let timeout = Duration::from_secs(2);
    let mut stream = TcpStream::connect_timeout(&addr, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    write!(
        stream,
        "GET /health HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"
    )?;
    // Bound memory use even if the response is not HTTP. A 200 from /health
    // means startup and database migrations completed before the listener opened.
    let mut status = Vec::new();
    use std::io::Read;
    BufReader::new(stream)
        .take(256)
        .read_until(b'\n', &mut status)?;
    if status.starts_with(b"HTTP/1.1 200 ") || status.starts_with(b"HTTP/1.0 200 ") {
        Ok(())
    } else {
        Err(std::io::Error::other("/health did not return HTTP 200"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn server(response: &'static str) -> (SocketAddr, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = String::new();
            let mut reader = BufReader::new(&stream);
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                request.push_str(&line);
                if line == "\r\n" {
                    break;
                }
            }
            assert!(request.starts_with("GET /health HTTP/1.1\r\n"));
            stream.write_all(response.as_bytes()).unwrap();
        });
        (addr, handle)
    }

    #[test]
    fn healthy_server() {
        let (addr, handle) = server("HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
        probe(addr).unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn failed_or_invalid_response() {
        for response in [
            "HTTP/1.1 503 Unavailable\r\n\r\n",
            "HTTP/1.1 404 Not Found\r\n\r\n",
            "not HTTP\r\n",
            "",
        ] {
            let (addr, handle) = server(response);
            assert!(probe(addr).is_err());
            handle.join().unwrap();
        }
    }

    #[test]
    fn server_not_listening() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        assert!(probe(addr).is_err());
    }
}
