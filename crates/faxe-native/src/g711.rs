use crate::FRAME_SAMPLES;
use spandsp::g711::{alaw_to_linear, linear_to_alaw, linear_to_ulaw, ulaw_to_linear};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum G711 {
    Pcma,
    Pcmu,
}

impl G711 {
    pub(crate) fn decode_sample(self, sample: u8) -> i16 {
        match self {
            Self::Pcma => alaw_to_linear(sample),
            Self::Pcmu => ulaw_to_linear(sample),
        }
    }
    pub fn payload_type(self) -> u8 {
        match self {
            Self::Pcma => 8,
            Self::Pcmu => 0,
        }
    }

    pub fn encode(self, samples: [i16; FRAME_SAMPLES]) -> [u8; FRAME_SAMPLES] {
        samples.map(match self {
            Self::Pcma => linear_to_alaw,
            Self::Pcmu => linear_to_ulaw,
        })
    }

    pub fn decode(self, samples: [u8; FRAME_SAMPLES]) -> [i16; FRAME_SAMPLES] {
        samples.map(match self {
            Self::Pcma => alaw_to_linear,
            Self::Pcmu => ulaw_to_linear,
        })
    }
}
