//! The server address the user types or scans: `host:port` (spec §6).

use std::fmt;
use std::net::{IpAddr, Ipv6Addr};

use rustls_pki_types::{DnsName, ServerName};

/// `skywaved`'s default listen port, assumed when the address has none.
pub const DEFAULT_PORT: u16 = 7443;

/// A parsed server address. The host doubles as the TLS server name: an IP
/// address is verified against the certificate's IP SANs, a name against
/// its DNS SANs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerAddress {
    host: Host,
    port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Host {
    Ip(IpAddr),
    /// Lowercase, validated DNS name.
    Dns(String),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid server address {0:?}: expected host:port, e.g. myhost.local:7443")]
pub struct AddressError(pub String);

impl ServerAddress {
    /// Parses `host:port`, `host` (port [`DEFAULT_PORT`]), `[v6]:port`,
    /// `[v6]` or a bare IPv6 address.
    pub fn parse(input: &str) -> Result<ServerAddress, AddressError> {
        let text = input.trim();
        let bad = || AddressError(input.to_owned());
        let (host, port) = if let Some(rest) = text.strip_prefix('[') {
            let (host, after) = rest.split_once(']').ok_or_else(bad)?;
            let ip: Ipv6Addr = host.parse().map_err(|_| bad())?;
            let port = match after {
                "" => None,
                p => Some(p.strip_prefix(':').ok_or_else(bad)?),
            };
            (Host::Ip(IpAddr::V6(ip)), port)
        } else if let Ok(ip) = text.parse::<Ipv6Addr>() {
            (Host::Ip(IpAddr::V6(ip)), None)
        } else {
            let (host, port) = match text.rsplit_once(':') {
                Some((host, port)) => (host, Some(port)),
                None => (text, None),
            };
            let host = match host.parse::<IpAddr>() {
                Ok(ip) => Host::Ip(ip),
                Err(_) => {
                    let name = host.to_ascii_lowercase();
                    DnsName::try_from(name.as_str()).map_err(|_| bad())?;
                    Host::Dns(name)
                }
            };
            (host, port)
        };
        let port = match port {
            None => DEFAULT_PORT,
            Some(p) => p.parse::<u16>().ok().filter(|p| *p != 0).ok_or_else(bad)?,
        };
        Ok(ServerAddress { host, port })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// The name the server certificate must cover.
    pub(crate) fn server_name(&self) -> ServerName<'static> {
        match &self.host {
            Host::Ip(ip) => ServerName::IpAddress((*ip).into()),
            Host::Dns(name) => {
                ServerName::DnsName(DnsName::try_from(name.clone()).expect("validated when parsed"))
            }
        }
    }

    /// Host as passed to the resolver (no brackets).
    pub(crate) fn host(&self) -> String {
        match &self.host {
            Host::Ip(ip) => ip.to_string(),
            Host::Dns(name) => name.clone(),
        }
    }
}

/// Canonical `host:port`, IPv6 bracketed: what the core stores and sends as
/// the HTTP `Host`.
impl fmt::Display for ServerAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.host {
            Host::Ip(IpAddr::V6(ip)) => write!(f, "[{ip}]:{}", self.port),
            Host::Ip(ip) => write!(f, "{ip}:{}", self.port),
            Host::Dns(name) => write!(f, "{name}:{}", self.port),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canon(input: &str) -> String {
        ServerAddress::parse(input).unwrap().to_string()
    }

    #[test]
    fn parses_the_address_forms() {
        assert_eq!(canon(" MyHost.local:7000 "), "myhost.local:7000");
        assert_eq!(canon("myhost"), "myhost:7443");
        assert_eq!(canon("10.0.0.7:1"), "10.0.0.7:1");
        assert_eq!(canon("[fd00::7]:7443"), "[fd00::7]:7443");
        assert_eq!(canon("[fd00::7]"), "[fd00::7]:7443");
        assert_eq!(canon("fd00::7"), "[fd00::7]:7443");
        assert!(matches!(
            ServerAddress::parse("10.0.0.7").unwrap().server_name(),
            ServerName::IpAddress(_)
        ));
        assert!(matches!(
            ServerAddress::parse("myhost").unwrap().server_name(),
            ServerName::DnsName(_)
        ));
    }

    #[test]
    fn rejects_garbage() {
        for bad in [
            "",
            "host:",
            "host:0",
            "host:99999",
            "[fd00::7",
            "[x]:1",
            "a b:1",
            "h:p",
        ] {
            assert!(ServerAddress::parse(bad).is_err(), "{bad:?}");
        }
    }
}
