use super::{PageProgress, sys};
use std::{ffi::c_void, ptr::NonNull};

/// Decode only the unique image frames that have finished transmission. This
/// measures page content rather than TIFF headers, UDPTL overhead, or retries.
pub(super) struct Progress {
    decoder: Option<Decoder>,
    pending: Option<Vec<u8>>,
    seen: [bool; 256],
    awaiting_ack: bool,
    reported: Option<(u32, u32)>,
    image_ready: bool,
}

impl Default for Progress {
    fn default() -> Self {
        Self {
            decoder: None,
            pending: None,
            seen: [false; 256],
            awaiting_ack: false,
            reported: None,
            image_ready: false,
        }
    }
}

struct Decoder {
    state: NonNull<sys::t4_rx_state_t>,
    page: u32,
    total_rows: u32,
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe { sys::t4_rx_free(self.state.as_ptr()) };
    }
}

unsafe extern "C" fn discard_row(_: *mut c_void, _: *const u8, _: usize) -> i32 {
    0
}

impl Progress {
    pub(super) fn frame(&mut self, stats: sys::t30_stats_t, incoming: bool, frame: &[u8]) {
        if frame.len() < 3 {
            return;
        }
        let kind = frame[2] & 0xfe;
        if stats.error_correcting_mode == 0 {
            if incoming && kind == 0x84 {
                self.image_ready = true;
                self.decoder = None;
            } else if incoming && kind == 0x44 {
                self.image_ready = false;
            }
            return;
        }
        if incoming {
            // MCF acknowledges a partial page block or a complete page. PPR
            // requests repeats from the same block and must not reset `seen`.
            if kind == 0x8c && self.awaiting_ack {
                self.seen.fill(false);
                self.awaiting_ack = false;
            }
            return;
        }
        // SpanDSP requests the next HDLC frame after the previous one has
        // finished. Do not credit an entire frame when it is merely queued.
        if let Some(previous) = self.pending.take()
            && let Some(decoder) = &self.decoder
        {
            unsafe {
                sys::t4_rx_put(decoder.state.as_ptr(), previous.as_ptr(), previous.len());
            }
        }
        if kind == 0xbe {
            self.awaiting_ack = true;
        }
        if kind != 0x06 || frame.len() < 5 {
            return;
        }
        let page = stats.pages_tx.max(0) as u32;
        if self
            .decoder
            .as_ref()
            .is_none_or(|decoder| decoder.page != page)
        {
            self.decoder = Decoder::new(&stats);
            self.seen.fill(false);
        }
        let sequence = frame[3] as usize;
        if !self.seen[sequence] && self.decoder.is_some() {
            self.seen[sequence] = true;
            self.pending = Some(frame[4..].to_vec());
        }
    }

    pub(super) fn non_ecm_data(&mut self, stats: sys::t30_stats_t, bytes: &[u8]) {
        if stats.error_correcting_mode != 0 || !self.image_ready {
            return;
        }
        let page = stats.pages_tx.max(0) as u32;
        if self
            .decoder
            .as_ref()
            .is_none_or(|decoder| decoder.page != page)
        {
            self.decoder = Decoder::new(&stats);
        }
        if let Some(decoder) = &self.decoder {
            unsafe { sys::t4_rx_put(decoder.state.as_ptr(), bytes.as_ptr(), bytes.len()) };
        }
    }

    pub(super) fn update(&mut self) -> Option<PageProgress> {
        let decoder = self.decoder.as_ref()?;
        let mut stats = sys::t4_stats_t::default();
        unsafe { sys::t4_rx_get_transfer_statistics(decoder.state.as_ptr(), &mut stats) };
        let rows = (stats.length.max(0) as u32).min(decoder.total_rows);
        let percent = (rows as u64 * 100 / decoder.total_rows as u64) as u32;
        let key = (decoder.page, percent);
        if self.reported == Some(key) {
            return None;
        }
        self.reported = Some(key);
        Some(PageProgress {
            page: decoder.page,
            rows,
            total_rows: decoder.total_rows,
        })
    }
}

impl Decoder {
    fn new(stats: &sys::t30_stats_t) -> Option<Self> {
        if stats.image_length <= 0 || stats.image_y_resolution <= 0 || stats.y_resolution <= 0 {
            return None;
        }
        let total_rows = (stats.image_length as u64 * stats.y_resolution as u64
            / stats.image_y_resolution as u64)
            .max(1)
            .min(u32::MAX as u64) as u32;
        // No file or retained pixels: only the codec's decoded-row counter.
        let state = NonNull::new(unsafe {
            sys::t4_rx_init(
                std::ptr::null_mut(),
                std::ptr::null(),
                sys::t4_image_compression_t_T4_COMPRESSION_T6 as i32,
            )
        })?;
        let decoder = Self {
            state,
            page: stats.pages_tx.max(0) as u32,
            total_rows,
        };
        unsafe {
            sys::t4_rx_set_image_width(state.as_ptr(), stats.width);
            sys::t4_rx_set_x_resolution(state.as_ptr(), stats.x_resolution);
            sys::t4_rx_set_y_resolution(state.as_ptr(), stats.y_resolution);
            if sys::t4_rx_set_rx_encoding(state.as_ptr(), stats.compression) != 0
                || sys::t4_rx_set_row_write_handler(
                    state.as_ptr(),
                    Some(discard_row),
                    std::ptr::null_mut(),
                ) != 0
                || sys::t4_rx_start_page(state.as_ptr()) != 0
            {
                return None;
            }
        }
        Some(decoder)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    unsafe extern "C" fn noise_row(data: *mut c_void, bytes: *mut u8, len: usize) -> i32 {
        let state = unsafe { &mut *data.cast::<(u32, u32)>() };
        if state.0 == 400 {
            return 0;
        }
        state.0 += 1;
        for byte in unsafe { std::slice::from_raw_parts_mut(bytes, len) } {
            state.1 ^= state.1 << 13;
            state.1 ^= state.1 >> 17;
            state.1 ^= state.1 << 5;
            *byte = state.1 as u8;
        }
        len as i32
    }

    #[test]
    fn image_progress_excludes_retries_and_resets_for_each_page() {
        let compression = sys::t4_image_compression_t_T4_COMPRESSION_T6 as i32;
        let mut noise = (0_u32, 1_u32);
        let encoder = unsafe {
            sys::t4_t6_encode_init(
                std::ptr::null_mut(),
                compression,
                1728,
                400,
                Some(noise_row),
                std::ptr::from_mut(&mut noise).cast(),
            )
        };
        assert!(!encoder.is_null());
        let mut encoded = Vec::new();
        loop {
            let mut chunk = [0; 256];
            let len = unsafe { sys::t4_t6_encode_get(encoder, chunk.as_mut_ptr(), 256) };
            if len <= 0 {
                break;
            }
            encoded.extend_from_slice(&chunk[..len as usize]);
        }
        unsafe { sys::t4_t6_encode_free(encoder) };
        assert!(encoded.len() > 65536, "exercise multiple ECM blocks");
        let mut progress = Progress::default();
        for page in 0..2 {
            let stats = sys::t30_stats_t {
                pages_tx: page,
                error_correcting_mode: 1,
                image_length: 400,
                image_y_resolution: 7700,
                width: 1728,
                x_resolution: 8031,
                y_resolution: 7700,
                compression,
                ..Default::default()
            };
            let mut last_rows = 0;
            let mut updates = 0;
            for (i, bytes) in encoded.chunks(256).enumerate() {
                if i > 0 && i % 256 == 0 {
                    progress.frame(stats, false, &[0xff, 0x13, 0xbe]);
                    progress.frame(stats, true, &[0xff, 0x13, 0x8c]);
                }
                let mut frame = vec![0xff, 0x03, 0x06, i as u8];
                frame.extend_from_slice(bytes);
                progress.frame(stats, false, &frame);
                progress.frame(stats, false, &[0xff, 0x03, 0x86]);
                if let Some(update) = progress.update() {
                    assert_eq!(update.page, page as u32);
                    assert_eq!(update.total_rows, 400);
                    assert!(update.rows >= last_rows && update.rows <= 400);
                    last_rows = update.rows;
                    updates += 1;
                }
                // Retransmitting this frame must not advance the decoder.
                progress.frame(stats, false, &frame);
                progress.frame(stats, false, &[0xff, 0x03, 0x86]);
                assert!(progress.update().is_none());
            }
            assert_eq!(last_rows, 400);
            assert!(updates > 20, "progress must advance within the page");
        }
    }
}
