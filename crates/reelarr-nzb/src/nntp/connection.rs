use super::{ArticleBody, Error, ServerConfig};
use rustls::{ClientConfig, pki_types::ServerName};
use std::{future::Future, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
    time::timeout,
};
use tokio_rustls::{TlsConnector, client::TlsStream};

pub(super) async fn deadline<T>(
    duration: Duration,
    scope: &'static str,
    future: impl Future<Output = Result<T, Error>>,
) -> Result<T, Error> {
    timeout(duration, future)
        .await
        .map_err(|_| Error::Timeout(scope))?
}
fn transport(e: std::io::Error) -> Error {
    Error::Transport(e.kind())
}

pub(super) struct Connection {
    stream: BufReader<TlsStream<TcpStream>>,
    config: Arc<ServerConfig>,
}
impl Connection {
    pub(super) async fn connect(
        config: Arc<ServerConfig>,
        tls: Arc<ClientConfig>,
    ) -> Result<Self, Error> {
        let name = ServerName::try_from(config.host.clone()).map_err(|_| Error::Config("host"))?;
        let stream = deadline(config.timeouts.connect, "connect", async {
            let tcp = TcpStream::connect((config.host.as_str(), config.port))
                .await
                .map_err(transport)?;
            TlsConnector::from(tls)
                .connect(name, tcp)
                .await
                .map_err(|e| {
                    // InvalidData includes failed identity/certificate validation.
                    // Do not retain rustls/server-originated diagnostics.
                    if e.kind() == std::io::ErrorKind::InvalidData {
                        Error::Tls
                    } else {
                        transport(e)
                    }
                })
        })
        .await?;
        let mut conn = Self {
            stream: BufReader::new(stream),
            config,
        };
        match conn.status().await?.code {
            200 | 201 => (),
            code => return Err(Error::response(code)),
        }
        if let Some(creds) = conn.config.credentials.clone() {
            conn.command(&format!("AUTHINFO USER {}\r\n", creds.username.0))
                .await?;
            match conn.status().await?.code {
                281 => (),
                381 => {
                    conn.command(&format!("AUTHINFO PASS {}\r\n", creds.password.0))
                        .await?;
                    match conn.status().await?.code {
                        281 => (),
                        code => return Err(Error::response(code)),
                    }
                }
                code => return Err(Error::response(code)),
            }
        }
        tracing::debug!(target: "reelarr_nzb::nntp", "verified NNTP connection ready");
        Ok(conn)
    }
    async fn command(&mut self, command: &str) -> Result<(), Error> {
        // Deliberately no wire tracing: peers can echo credentials in any reply.
        deadline(self.config.timeouts.io, "write", async {
            self.stream
                .get_mut()
                .write_all(command.as_bytes())
                .await
                .map_err(transport)?;
            self.stream.get_mut().flush().await.map_err(transport)
        })
        .await
    }
    async fn line(&mut self, limit: usize) -> Result<Vec<u8>, Error> {
        deadline(self.config.timeouts.io, "read", async {
            let mut line = Vec::new();
            loop {
                // Never use unbounded read_until/read_line on untrusted input.
                let available = self.stream.fill_buf().await.map_err(transport)?;
                if available.is_empty() {
                    return Err(Error::Incomplete);
                }
                let n = available
                    .iter()
                    .position(|b| *b == b'\n')
                    .map_or(available.len(), |i| i + 1);
                if n > limit.saturating_sub(line.len()) {
                    return Err(Error::Limit("line bytes"));
                }
                let done = available[n - 1] == b'\n';
                line.extend_from_slice(&available[..n]);
                self.stream.consume(n);
                if done {
                    if !line.ends_with(b"\r\n")
                        || line[..line.len() - 2]
                            .iter()
                            .any(|b| matches!(b, 0 | b'\r' | b'\n'))
                    {
                        return Err(Error::Protocol("CRLF framing"));
                    }
                    return Ok(line);
                }
            }
        })
        .await
    }
    async fn status(&mut self) -> Result<Status, Error> {
        let line = self.line(512).await?;
        let bytes = &line[..line.len() - 2];
        if bytes.len() < 3
            || !(b'1'..=b'5').contains(&bytes[0])
            || !bytes[1..3].iter().all(u8::is_ascii_digit)
            || (bytes.len() > 3 && bytes[3] != b' ')
        {
            return Err(Error::Protocol("status code"));
        }
        let code = u16::from(bytes[0] - b'0') * 100
            + u16::from(bytes[1] - b'0') * 10
            + u16::from(bytes[2] - b'0');
        Ok(Status { code, line })
    }
    pub(super) async fn body(&mut self, id: &str) -> Result<ArticleBody, Error> {
        self.command(&format!("BODY <{id}>\r\n")).await?;
        let status = self.status().await?;
        if status.code != 222 {
            return Err(Error::response(status.code));
        }
        status.article(id)?;
        let mut bytes = Vec::new();
        loop {
            let line = self.line(self.config.limits.max_line_bytes).await?;
            if line == b".\r\n" {
                return Ok(ArticleBody(bytes));
            }
            if line.starts_with(b".") && !line.starts_with(b"..") {
                return Err(Error::Protocol("dot stuffing"));
            }
            if line.len()
                > self
                    .config
                    .limits
                    .max_article_bytes
                    .saturating_sub(bytes.len())
            {
                return Err(Error::Limit("article bytes"));
            }
            bytes.extend_from_slice(&line);
        }
    }
    pub(super) async fn stat(&mut self, id: &str) -> Result<(), Error> {
        self.command(&format!("STAT <{id}>\r\n")).await?;
        let status = self.status().await?;
        if status.code != 223 {
            return Err(Error::response(status.code));
        }
        status.article(id)
    }
    pub(super) async fn quit(mut self) -> Result<(), Error> {
        self.command("QUIT\r\n").await?;
        let status = self.status().await?;
        if status.code != 205 {
            return Err(Error::response(status.code));
        }
        deadline(self.config.timeouts.io, "write", async {
            self.stream.get_mut().shutdown().await.map_err(transport)
        })
        .await
    }
}
struct Status {
    code: u16,
    line: Vec<u8>,
}
impl Status {
    fn article(&self, requested: &str) -> Result<(), Error> {
        let mut args = self.line[3..self.line.len() - 2]
            .split(|b| *b == b' ')
            .filter(|s| !s.is_empty());
        let number = args.next().ok_or(Error::Protocol("article number"))?;
        if !number.iter().all(u8::is_ascii_digit) {
            return Err(Error::Protocol("article number"));
        }
        let id = args.next().ok_or(Error::Protocol("article identity"))?;
        if id.len() != requested.len() + 2
            || id.first() != Some(&b'<')
            || id.last() != Some(&b'>')
            || &id[1..id.len() - 1] != requested.as_bytes()
        {
            return Err(Error::Protocol("article identity"));
        }
        Ok(())
    }
}

pub(super) fn validate_id(id: &str) -> Result<(), Error> {
    // Segment fields are public; do not rely solely on the XML parser's checks.
    // RFC 3977 §3.6: 3..250 octets with brackets, printable ASCII, no embedded <>.
    if id.is_empty()
        || id.len() > 248
        || !id
            .bytes()
            .all(|b| b.is_ascii_graphic() && b != b'<' && b != b'>')
    {
        return Err(Error::Config("message ID"));
    }
    Ok(())
}
