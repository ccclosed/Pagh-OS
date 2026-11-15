//! Клиентская библиотека протокола PDP/1.
//!
//! Клиент подключается к сокету композитора, проходит `hello`, создаёт окна, шлёт
//! кадры и читает `configure`/`input`/`focus`/`close`.

use crate::geometry::Size;
use crate::protocol::{self, Decoder, Message, VERSION};
use crate::surface::Surface;
use std::io::{self, Read};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

/// Информация из `welcome`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Welcome {
    pub client_id: u64,
    pub screen: Size,
    pub flags: u32,
}

/// Подключение к desktop-серверу.
pub struct Client {
    stream: UnixStream,
    decoder: Decoder,
    welcome: Option<Welcome>,
}

impl Client {
    pub fn connect(path: impl AsRef<Path>) -> io::Result<Self> {
        let stream = UnixStream::connect(path.as_ref())?;
        stream.set_read_timeout(Some(Duration::from_millis(100)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        Ok(Self {
            stream,
            decoder: Decoder::new(),
            welcome: None,
        })
    }

    /// Подключиться с повторами: сервер может ещё не успеть создать сокет.
    pub fn connect_retry(path: impl AsRef<Path>, attempts: u32) -> io::Result<Self> {
        let mut last_error = None;
        for attempt in 0..attempts.max(1) {
            match Self::connect(path.as_ref()) {
                Ok(client) => return Ok(client),
                Err(err) => {
                    last_error = Some(err);
                    std::thread::sleep(Duration::from_millis(20 + attempt as u64 * 10));
                }
            }
        }
        Err(last_error.unwrap_or_else(|| io::Error::other("нет попыток")))
    }

    pub fn welcome(&self) -> Option<&Welcome> {
        self.welcome.as_ref()
    }

    pub fn send(&mut self, message: &Message) -> io::Result<()> {
        protocol::write_message(&mut self.stream, message)
    }

    /// Рукопожатие: отправить `hello` и дождаться `welcome`.
    pub fn hello(&mut self, app: &str) -> io::Result<Welcome> {
        self.send(&Message::Hello {
            version: VERSION,
            pid: std::process::id(),
            app: app.to_string(),
        })?;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if Instant::now() > deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "сервер не ответил welcome",
                ));
            }
            match self.read_next()? {
                Some(Message::Welcome {
                    version,
                    client_id,
                    width,
                    height,
                    flags,
                }) => {
                    if version != VERSION {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("несовместимая версия протокола {version}"),
                        ));
                    }
                    let welcome = Welcome {
                        client_id,
                        screen: Size::new(width, height),
                        flags,
                    };
                    self.welcome = Some(welcome.clone());
                    return Ok(welcome);
                }
                Some(Message::Error { code, message }) => {
                    return Err(io::Error::other(format!(
                        "сервер вернул ошибку {code}: {message}"
                    )));
                }
                Some(_) => continue,
                None => std::thread::sleep(Duration::from_millis(2)),
            }
        }
    }

    pub fn create_window(
        &mut self,
        window_id: u32,
        width: u32,
        height: u32,
        format: crate::surface::PixelFormat,
        title: &str,
    ) -> io::Result<()> {
        self.send(&Message::Create {
            window_id,
            width,
            height,
            format: format.to_wire(),
            title: title.to_string(),
        })
    }

    pub fn commit(&mut self, window_id: u32, surface: &Surface) -> io::Result<()> {
        self.send(&Message::Commit {
            window_id,
            width: surface.width,
            height: surface.height,
            format: surface.format.to_wire(),
            stride: surface.stride(),
            pixels: surface.to_wire(),
        })
    }

    pub fn damage(&mut self, window_id: u32, rects: &[(i32, i32, u16, u16)]) -> io::Result<()> {
        self.send(&Message::Damage {
            window_id,
            rects: rects.to_vec(),
        })
    }

    pub fn close_window(&mut self, window_id: u32) -> io::Result<()> {
        self.send(&Message::Close {
            window_id,
            reason: protocol::CLOSE_CLIENT,
        })
    }

    pub fn ping(&mut self, nonce: u32) -> io::Result<()> {
        self.send(&Message::Ping { nonce })
    }

    /// Прочитать следующее сообщение без ожидания (таймаут сокета 100 мс).
    pub fn read_next(&mut self) -> io::Result<Option<Message>> {
        if let Some(message) = self.decoder.next_message().map_err(protocol_error)? {
            return Ok(Some(message));
        }
        let mut buffer = [0u8; 64 * 1024];
        loop {
            match self.stream.read(&mut buffer) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "сервер закрыл соединение",
                    ))
                }
                Ok(n) => {
                    self.decoder.push(&buffer[..n]).map_err(protocol_error)?;
                    if let Some(message) = self.decoder.next_message().map_err(protocol_error)? {
                        return Ok(Some(message));
                    }
                }
                Err(err)
                    if err.kind() == io::ErrorKind::WouldBlock
                        || err.kind() == io::ErrorKind::TimedOut =>
                {
                    return Ok(None)
                }
                Err(err) => return Err(err),
            }
        }
    }

    /// Читать сообщения до истечения `timeout`; вернуть первое полученное.
    pub fn next_message(&mut self, timeout: Duration) -> io::Result<Option<Message>> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(message) = self.read_next()? {
                return Ok(Some(message));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
        }
    }

    /// Блокирующее ожидание следующего сообщения (с общим deadline).
    pub fn expect_message(&mut self, timeout: Duration) -> io::Result<Message> {
        self.next_message(timeout)?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "сообщение не пришло за отведённое время",
            )
        })
    }
}

fn protocol_error(err: protocol::ProtocolError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_to_missing_socket_fails() {
        match Client::connect("/tmp/pagh-desktop-no-such-socket-12345") {
            Ok(_) => panic!("подключение к несуществующему сокету должно падать"),
            Err(err) => assert!(matches!(
                err.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            )),
        }
    }
}
