//! The server address the user types or scans: `host:port` (architecture.md "Pairing").

use std::fmt;
use std::net::{IpAddr, Ipv6Addr};
use std::str::FromStr;

use rustls_pki_types::{DnsName, ServerName};

/// `skywaved`'s default listen port, assumed when the address has none.
pub const DEFAULT_PORT: u16 = 7443;

/// A parsed server address. The host doubles as the TLS server name: an IP
/// address is verified against the certificate's IP SANs, a name against
/// its DNS SANs.
///
/// Equal (and hashed) by its canonical form, the [`Display`](fmt::Display)
/// one: every spelling of the same address (`MyHost`, `myhost:7443`) is one
/// value. Serialized as that canonical string.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ServerAddress {
    host: Host,
    port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
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

impl FromStr for ServerAddress {
    type Err = AddressError;

    fn from_str(input: &str) -> Result<ServerAddress, AddressError> {
        ServerAddress::parse(input)
    }
}

impl serde::Serialize for ServerAddress {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> serde::Deserialize<'de> for ServerAddress {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = <std::borrow::Cow<'de, str>>::deserialize(deserializer)?;
        ServerAddress::parse(&text).map_err(serde::de::Error::custom)
    }
}

/// The paired server's addresses: the one the device connects to, and the
/// others it was reached at, to switch back to without typing them again
/// (architecture.md "Pairing"). The others never hold the current one, nor
/// one twice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerAddresses {
    current: ServerAddress,
    others: Vec<ServerAddress>,
}

impl ServerAddresses {
    /// Just `current`, as pairing with a new server leaves them.
    pub fn new(current: ServerAddress) -> ServerAddresses {
        ServerAddresses {
            current,
            others: Vec::new(),
        }
    }

    /// `current` with `others` in their order, less `current` and repeats.
    pub(crate) fn with_others(
        current: ServerAddress,
        others: impl IntoIterator<Item = ServerAddress>,
    ) -> ServerAddresses {
        let mut addresses = ServerAddresses::new(current);
        for other in others {
            if other != addresses.current && !addresses.others.contains(&other) {
                addresses.others.push(other);
            }
        }
        addresses
    }

    /// The address the device connects to.
    pub fn current(&self) -> &ServerAddress {
        &self.current
    }

    pub fn others(&self) -> &[ServerAddress] {
        &self.others
    }

    /// Makes `address` the current one. The one it replaces joins the
    /// others at the end, and `address` leaves them; nothing changes if it
    /// is already the current one.
    pub fn switch_to(&mut self, address: &ServerAddress) {
        if self.current == *address {
            return;
        }
        self.others.retain(|other| other != address);
        let old = std::mem::replace(&mut self.current, address.clone());
        self.others.push(old);
    }

    /// Drops `address` from the others; `false` if it was not there (the
    /// current one never is).
    pub fn forget(&mut self, address: &ServerAddress) -> bool {
        let before = self.others.len();
        self.others.retain(|other| other != address);
        self.others.len() != before
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
    fn every_spelling_of_an_address_is_one_value() {
        let canonical = ServerAddress::parse("myhost:7443").unwrap();
        for spelling in ["MyHost", " myhost ", "MYHOST:7443"] {
            assert_eq!(
                ServerAddress::parse(spelling).unwrap(),
                canonical,
                "{spelling:?}"
            );
        }
        assert_eq!(
            ServerAddress::parse("[fd00:0::7]:7443").unwrap(),
            ServerAddress::parse("fd00::7").unwrap()
        );
        let json = serde_json::to_string(&ServerAddress::parse("MyHost").unwrap()).unwrap();
        assert_eq!(json, r#""myhost:7443""#);
        assert_eq!(
            serde_json::from_str::<ServerAddress>(&json).unwrap(),
            canonical
        );
        assert!(serde_json::from_str::<ServerAddress>(r#""h:p""#).is_err());
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

    #[test]
    fn switching_keeps_the_old_address_until_forgotten() {
        let addr = |text: &str| ServerAddress::parse(text).unwrap();
        let mut addresses = ServerAddresses::new(addr("home"));
        addresses.switch_to(&addr("vpn"));
        addresses.switch_to(&addr("public"));
        // The same address spelt otherwise: not a fourth one.
        addresses.switch_to(&addr("HOME:7443"));
        assert_eq!(addresses.current(), &addr("home"));
        assert_eq!(addresses.others(), [addr("vpn"), addr("public")]);
        addresses.switch_to(&addr("home"));
        assert_eq!(addresses.others(), [addr("vpn"), addr("public")]);

        assert!(!addresses.forget(&addr("home")), "the current one");
        assert!(addresses.forget(&addr("vpn")));
        assert!(!addresses.forget(&addr("vpn")));
        assert_eq!(addresses.others(), [addr("public")]);

        let loaded = ServerAddresses::with_others(
            addr("home"),
            [addr("vpn"), addr("HOME"), addr("vpn"), addr("public")],
        );
        assert_eq!(loaded.others(), [addr("vpn"), addr("public")]);
    }
}
