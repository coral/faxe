use crate::{Error, G711, Result};
use std::net::{IpAddr, SocketAddr};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Audio,
    T38,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RemoteMedia {
    Audio {
        address: SocketAddr,
        codec: G711,
    },
    T38 {
        address: SocketAddr,
        max_datagram: usize,
        bit_rate: u32,
    },
}

impl RemoteMedia {
    pub fn kind(&self) -> Kind {
        match self {
            Self::Audio { .. } => Kind::Audio,
            Self::T38 { .. } => Kind::T38,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct LocalMedia {
    pub ip: IpAddr,
    pub audio_port: u16,
    pub fax_port: u16,
    pub session: u64,
    pub version: u64,
}

impl LocalMedia {
    fn header(&self) -> String {
        let family = match self.ip {
            IpAddr::V4(_) => "IP4",
            IpAddr::V6(_) => "IP6",
        };
        format!(
            "v=0\r\no=faxe {} {} IN {family} {}\r\ns=Faxe\r\nc=IN {family} {}\r\nt=0 0\r\n",
            self.session, self.version, self.ip, self.ip
        )
    }

    pub fn offer(&self, kind: Kind) -> String {
        let mut result = self.header();
        result.push_str(&match kind {
            Kind::Audio => format!("m=audio {} RTP/AVP 8 0\r\na=rtpmap:8 PCMA/8000\r\na=rtpmap:0 PCMU/8000\r\na=ptime:20\r\na=sendrecv\r\n", self.audio_port),
            Kind::T38 => self.t38(14400),
        });
        result
    }

    fn t38(&self, bit_rate: u32) -> String {
        format!(
            "m=image {} udptl t38\r\na=T38FaxVersion:0\r\na=T38MaxBitRate:{bit_rate}\r\na=T38FaxRateManagement:transferredTCF\r\na=T38FaxMaxBuffer:2000\r\na=T38FaxMaxDatagram:1200\r\na=T38FaxUdpEC:t38UDPRedundancy\r\n",
            self.fax_port
        )
    }

    pub fn answer(&self, offer: &str, allow_audio: bool, allow_t38: bool) -> Result<String> {
        let parsed = Session::parse(offer)?;
        let selected = parsed.select(allow_audio, allow_t38)?;
        let mut result = self.header();
        for (index, media) in parsed.media.iter().enumerate() {
            if index != selected.0 {
                result.push_str(&format!(
                    "m={} 0 {} {}\r\n",
                    media.name,
                    media.protocol,
                    media.formats.join(" ")
                ));
                continue;
            }
            result.push_str(&match selected.1 {
                RemoteMedia::Audio { codec, .. } => {
                    let name = match codec { G711::Pcma => "PCMA", G711::Pcmu => "PCMU" };
                    let pt = codec.payload_type();
                    format!("m=audio {} RTP/AVP {pt}\r\na=rtpmap:{pt} {name}/8000\r\na=ptime:20\r\na=sendrecv\r\n", self.audio_port)
                }
                RemoteMedia::T38 { bit_rate, .. } => self.t38(bit_rate),
            });
        }
        Ok(result)
    }
}

pub(crate) fn remote(sdp: &str) -> Result<RemoteMedia> {
    Session::parse(sdp)?
        .select(true, true)
        .map(|(_, media)| media)
}

struct Session {
    connection: Option<IpAddr>,
    media: Vec<Media>,
}
struct Media {
    name: String,
    port: u16,
    protocol: String,
    formats: Vec<String>,
    connection: Option<IpAddr>,
    attributes: Vec<String>,
}

impl Session {
    fn parse(sdp: &str) -> Result<Self> {
        let mut session = Self {
            connection: None,
            media: Vec::new(),
        };
        for line in sdp.lines().map(str::trim) {
            match line.split_once('=') {
                Some(("m", value)) => {
                    let parts: Vec<_> = value.split_whitespace().collect();
                    if parts.len() < 4 {
                        return Err(Error::Sip("Malformed SDP media line".into()));
                    }
                    session.media.push(Media {
                        name: parts[0].into(),
                        port: parts[1]
                            .parse()
                            .map_err(|_| Error::Sip("Invalid SDP port".into()))?,
                        protocol: parts[2].into(),
                        formats: parts[3..].iter().map(|s| (*s).into()).collect(),
                        connection: None,
                        attributes: Vec::new(),
                    });
                }
                Some(("c", value)) => {
                    let address = value
                        .split_whitespace()
                        .nth(2)
                        .and_then(|s| s.parse().ok())
                        .ok_or_else(|| Error::Sip("SDP requires a unicast IP address".into()))?;
                    match session.media.last_mut() {
                        Some(media) => media.connection = Some(address),
                        None => session.connection = Some(address),
                    }
                }
                Some(("a", value)) => {
                    if let Some(media) = session.media.last_mut() {
                        media.attributes.push(value.into());
                    }
                }
                _ => (),
            }
        }
        Ok(session)
    }

    fn select(&self, allow_audio: bool, allow_t38: bool) -> Result<(usize, RemoteMedia)> {
        for (index, media) in self.media.iter().enumerate() {
            if media.port == 0
                || media
                    .attributes
                    .iter()
                    .any(|a| a == "inactive" || a == "sendonly")
            {
                continue;
            }
            let Some(ip) = media.connection.or(self.connection) else {
                continue;
            };
            if ip.is_unspecified() || ip.is_multicast() {
                continue;
            }
            let address = SocketAddr::new(ip, media.port);
            match (
                media.name.as_str(),
                media.protocol.to_ascii_lowercase().as_str(),
            ) {
                ("image", "udptl")
                    if allow_t38 && media.formats.iter().any(|s| s.eq_ignore_ascii_case("t38")) =>
                {
                    if media
                        .attr("T38FaxRateManagement")
                        .is_some_and(|s| !s.eq_ignore_ascii_case("transferredTCF"))
                        || media
                            .attr("T38FaxUdpEC")
                            .is_some_and(|s| !s.eq_ignore_ascii_case("t38UDPRedundancy"))
                    {
                        continue;
                    }
                    let max_datagram = media
                        .attr("T38FaxMaxDatagram")
                        .unwrap_or("400")
                        .parse::<usize>()
                        .map_err(|_| Error::Sip("Invalid T.38 datagram limit".into()))?
                        .min(1200);
                    if max_datagram < 64 {
                        continue;
                    }
                    let maximum = media
                        .attr("T38MaxBitRate")
                        .unwrap_or("14400")
                        .parse::<u32>()
                        .map_err(|_| Error::Sip("Invalid T.38 bit rate".into()))?;
                    let bit_rate = match maximum {
                        14400.. => 14400,
                        9600.. => 9600,
                        4800.. => 4800,
                        _ => continue,
                    };
                    return Ok((
                        index,
                        RemoteMedia::T38 {
                            address,
                            max_datagram,
                            bit_rate,
                        },
                    ));
                }
                ("audio", "rtp/avp") if allow_audio => {
                    for format in &media.formats {
                        let codec = match format.as_str() {
                            "8" => G711::Pcma,
                            "0" => G711::Pcmu,
                            _ => continue,
                        };
                        return Ok((index, RemoteMedia::Audio { address, codec }));
                    }
                }
                _ => (),
            }
        }
        Err(Error::Sip(
            "Peer offered no compatible G.711 or T.38 media".into(),
        ))
    }
}

impl Media {
    fn attr(&self, name: &str) -> Option<&str> {
        self.attributes
            .iter()
            .filter_map(|attribute| attribute.split_once(':'))
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value)
    }
}
