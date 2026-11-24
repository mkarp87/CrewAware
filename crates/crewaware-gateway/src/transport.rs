//! Byte transports to the companion radio: a USB serial port, or TCP (WiFi companion firmware,
//! a serial-to-network bridge, or the fake radio).

use tokio::io::{AsyncRead, AsyncWrite};

pub trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}
pub type BoxIo = Box<dyn Io>;

#[derive(Debug, PartialEq)]
pub enum Target {
    Tcp(String),
    Serial { path: String, baud: u32 },
}

/// `tcp://host:port`, `serial:/dev/ttyACM0` (optionally `@115200`), or a bare device path.
pub fn parse_target(s: &str) -> Result<Target, String> {
    let s = s.trim();
    if let Some(addr) = s.strip_prefix("tcp://") {
        if addr.is_empty() || !addr.contains(':') {
            return Err("tcp target needs host:port".into());
        }
        return Ok(Target::Tcp(addr.to_string()));
    }
    let dev = s.strip_prefix("serial:").unwrap_or(s);
    if dev.is_empty() {
        return Err("empty radio target".into());
    }
    let looks_like_device = dev.starts_with('/') || dev.to_ascii_uppercase().starts_with("COM");
    if !looks_like_device && !s.starts_with("serial:") {
        return Err("radio target must be tcp://host:port or a serial device path".into());
    }
    match dev.rsplit_once('@') {
        Some((path, baud)) => Ok(Target::Serial {
            path: path.into(),
            baud: baud.parse().map_err(|_| "bad baud rate".to_string())?,
        }),
        None => Ok(Target::Serial {
            path: dev.into(),
            baud: 115_200,
        }),
    }
}

pub async fn open(t: &Target) -> std::io::Result<BoxIo> {
    match t {
        Target::Tcp(addr) => {
            let s = tokio::net::TcpStream::connect(addr).await?;
            s.set_nodelay(true).ok();
            Ok(Box::new(s))
        }
        Target::Serial { path, baud } => {
            use tokio_serial::SerialPortBuilderExt;
            let port = tokio_serial::new(path, *baud).open_native_async()?;
            Ok(Box::new(port))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets() {
        assert_eq!(
            parse_target("tcp://10.0.0.5:5000"),
            Ok(Target::Tcp("10.0.0.5:5000".into()))
        );
        assert_eq!(
            parse_target("/dev/ttyACM0"),
            Ok(Target::Serial {
                path: "/dev/ttyACM0".into(),
                baud: 115_200
            })
        );
        assert_eq!(
            parse_target("serial:COM5@57600"),
            Ok(Target::Serial {
                path: "COM5".into(),
                baud: 57_600
            })
        );
        assert!(parse_target("tcp://nohost").is_err());
        assert!(parse_target("banana").is_err());
        assert!(parse_target("").is_err());
    }
}
