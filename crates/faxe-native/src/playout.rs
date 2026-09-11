//! Timestamp-indexed G.711 sample ring. Socket callbacks only queue packets;
//! the fax worker reorders and plays samples against a separate receive clock.
use crate::{Error, FRAME_SAMPLES, G711, Result};
use std::time::{Duration, Instant};

pub const DEFAULT_AUDIO_PLAYOUT_DELAY_MS: u16 = 200;

pub fn validate_audio_playout_delay(delay_ms: u16) -> Result<()> {
    if !(40..=1000).contains(&delay_ms) {
        return Err(Error::Invalid(
            "Audio receive delay must be between 40 and 1000 ms".into(),
        ));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ReceiveStats {
    pub late_samples: u64,
    pub overflow_samples: u64,
    pub duplicate_samples: u64,
    pub missing_samples: u64,
}

pub(crate) struct Playout {
    samples: Vec<Option<i16>>,
    anchor: Option<u32>,
    cursor: i64,
    end: i64,
    delay: Duration,
    next_playout: Option<Instant>,
    started: bool,
    pub stats: ReceiveStats,
}

impl Playout {
    pub fn new(delay_ms: u16) -> Result<Self> {
        validate_audio_playout_delay(delay_ms)?;
        Ok(Self {
            // The delay plus one second of headroom bounds memory while
            // retaining bursts during worker stalls. Never progressively drop
            // samples to reduce latency: that damages fax modem waveforms.
            samples: vec![None; (usize::from(delay_ms) + 1000) * 8],
            anchor: None,
            cursor: 0,
            end: 0,
            delay: Duration::from_millis(u64::from(delay_ms)),
            next_playout: None,
            started: false,
            stats: ReceiveStats::default(),
        })
    }

    /// A signaled endpoint change may introduce a new RTP clock and SSRC.
    /// Retain cumulative quality counters, but discard the previous stream.
    pub fn reset_stream(&mut self) {
        self.samples.fill(None);
        self.anchor = None;
        self.cursor = 0;
        self.end = 0;
        self.next_playout = None;
        self.started = false;
    }

    pub fn insert(&mut self, timestamp: u32, payload: &[u8], codec: G711, now: Instant) {
        if payload.is_empty() {
            return;
        }
        let anchor = *self.anchor.get_or_insert_with(|| {
            self.next_playout = Some(now + self.delay);
            timestamp
        });
        let offset = i64::from(timestamp.wrapping_sub(anchor) as i32);
        let capacity = self.samples.len() as i64;
        // The first packet can itself be out of order. Before playout starts,
        // allow preceding samples without moving the established deadline.
        if !self.started && offset < self.cursor && self.end - offset <= capacity {
            self.cursor = offset;
        }
        for (index, byte) in payload.iter().enumerate() {
            let position = offset + index as i64;
            if position < self.cursor {
                self.stats.late_samples += 1;
            } else if position >= self.cursor + capacity {
                self.stats.overflow_samples += 1;
            } else {
                let slot = &mut self.samples[position.rem_euclid(capacity) as usize];
                if slot.is_some() {
                    self.stats.duplicate_samples += 1;
                } else {
                    *slot = Some(codec.decode_sample(*byte));
                    self.end = self.end.max(position + 1);
                }
            }
        }
    }

    pub fn receive(&mut self, now: Instant) -> Option<[i16; FRAME_SAMPLES]> {
        let at = self.next_playout?;
        if now < at {
            // Catch-up iterations must not consume the deliberately buffered
            // delay. Startup silence is not packet loss.
            return None;
        }
        self.next_playout = Some(at + Duration::from_millis(20));
        self.started = true;
        Some(self.take_frame(FRAME_SAMPLES))
    }

    fn take_frame(&mut self, count: usize) -> [i16; FRAME_SAMPLES] {
        let capacity = self.samples.len() as i64;
        let mut frame = [0; FRAME_SAMPLES];
        for sample in frame.iter_mut().take(count) {
            *sample = self.samples[self.cursor.rem_euclid(capacity) as usize]
                .take()
                .unwrap_or_else(|| {
                    self.stats.missing_samples += 1;
                    0
                });
            self.cursor += 1;
        }
        frame
    }

    // A BYE may follow the final fax acknowledgement before its playout time.
    // Deliver all buffered samples before ending T.30; cap at the ring's end.
    pub fn drain(&mut self) -> Vec<[i16; FRAME_SAMPLES]> {
        let mut frames = Vec::new();
        while self.cursor < self.end {
            frames
                .push(self.take_frame((self.end - self.cursor).min(FRAME_SAMPLES as i64) as usize));
        }
        frames
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CODEC: G711 = G711::Pcmu;
    fn packet(value: u8) -> [u8; FRAME_SAMPLES] {
        [value; FRAME_SAMPLES]
    }
    fn frame(value: u8) -> [i16; FRAME_SAMPLES] {
        [CODEC.decode_sample(value); FRAME_SAMPLES]
    }

    #[test]
    fn delay_survives_catch_up_and_reorders_packets_delayed_beyond_40_ms() {
        for delay in [40, 200, 1000] {
            let start = Instant::now();
            let mut ring = Playout::new(delay).unwrap();
            ring.insert(0, &packet(1), CODEC, start);
            ring.insert(320, &packet(3), CODEC, start + Duration::from_millis(30));
            let first = start + Duration::from_millis(u64::from(delay));
            // A late-starting worker may run several frames immediately.
            for _ in 0..20 {
                assert_eq!(ring.receive(first - Duration::from_millis(1)), None);
            }
            assert_eq!(ring.receive(first), Some(frame(1)));
            assert_eq!(ring.receive(first), None);
            ring.insert(160, &packet(2), CODEC, first + Duration::from_millis(10));
            assert_eq!(
                ring.receive(first + Duration::from_millis(20)),
                Some(frame(2))
            );
            assert_eq!(
                ring.receive(first + Duration::from_millis(40)),
                Some(frame(3))
            );
            assert_eq!(ring.stats, ReceiveStats::default());
        }
    }

    #[test]
    fn ring_wrap_and_rtp_timestamp_wrap_preserve_variable_packet_sizes() {
        let start = Instant::now();
        let mut ring = Playout::new(200).unwrap();
        let anchor = u32::MAX - 79;
        for block in 0..150u32 {
            let at = start + Duration::from_millis(u64::from(block) * 20);
            let timestamp = anchor.wrapping_add(block * 160);
            let value = (block % 64) as u8;
            // Two 10 ms packets per 20 ms frame, second arriving first.
            ring.insert(timestamp.wrapping_add(80), &[value; 80], CODEC, at);
            ring.insert(timestamp, &[value; 80], CODEC, at);
            assert_eq!(
                ring.receive(at + Duration::from_millis(200)),
                Some(frame(value))
            );
        }
        assert_eq!(ring.stats, ReceiveStats::default());
    }

    #[test]
    fn reports_late_duplicate_overflow_and_missing_samples_separately() {
        let start = Instant::now();
        let mut ring = Playout::new(200).unwrap();
        ring.insert(0, &packet(1), CODEC, start);
        ring.insert(0, &packet(2), CODEC, start);
        ring.insert(9600, &packet(3), CODEC, start);
        assert_eq!(
            ring.receive(start + Duration::from_millis(200)),
            Some(frame(1))
        );
        ring.insert(80, &packet(4), CODEC, start + Duration::from_millis(210));
        let played = ring.receive(start + Duration::from_millis(220)).unwrap();
        assert_eq!(&played[..80], &frame(4)[..80]);
        assert_eq!(&played[80..], &[0; 80]);
        assert_eq!(
            ring.stats,
            ReceiveStats {
                late_samples: 80,
                overflow_samples: 160,
                duplicate_samples: 160,
                missing_samples: 80,
            }
        );
    }

    #[test]
    fn stalled_worker_keeps_buffered_samples_without_progressive_discard() {
        let start = Instant::now();
        let mut ring = Playout::new(200).unwrap();
        for n in 0..20 {
            ring.insert(n * 160, &packet(n as u8), CODEC, start);
        }
        // A 300 ms stall permits six frames at the established receive clock.
        let resumed = start + Duration::from_millis(300);
        for n in 0..=5 {
            assert_eq!(ring.receive(resumed), Some(frame(n)));
        }
        assert_eq!(ring.receive(resumed), None);
        assert_eq!(
            ring.receive(resumed + Duration::from_millis(20)),
            Some(frame(6))
        );
        assert_eq!(ring.stats, ReceiveStats::default());
    }

    #[test]
    fn disconnect_flushes_final_samples_before_their_playout_deadline() {
        let start = Instant::now();
        let mut ring = Playout::new(200).unwrap();
        ring.insert(0, &packet(1), CODEC, start);
        ring.insert(160, &[2; 80], CODEC, start);
        let frames = ring.drain();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0], frame(1));
        assert_eq!(&frames[1][..80], &frame(2)[..80]);
        assert_eq!(ring.stats.missing_samples, 0);
        assert!(ring.drain().is_empty());
    }
}
