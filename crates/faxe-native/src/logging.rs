use faxe_pj_sys as pj;
use spandsp::spandsp_sys as dsp;
use std::{
    borrow::Cow,
    ffi::{CStr, c_char, c_void},
};

pub(crate) struct PjLogging {
    previous: Option<unsafe extern "C" fn(i32, *const c_char, i32)>,
    level: i32,
}

impl PjLogging {
    pub fn install() -> Self {
        unsafe {
            let previous = Self {
                previous: pj::pj_log_get_log_func(),
                level: pj::pj_log_get_level(),
            };
            let level = match () {
                _ if tracing::enabled!(target: "faxe_native::pjsip", tracing::Level::TRACE) => 5,
                _ if tracing::enabled!(target: "faxe_native::pjsip", tracing::Level::DEBUG) => 4,
                _ if tracing::enabled!(target: "faxe_native::pjsip", tracing::Level::INFO) => 3,
                _ if tracing::enabled!(target: "faxe_native::pjsip", tracing::Level::WARN) => 2,
                _ => 1,
            };
            pj::pj_log_set_log_func(Some(pj_message));
            pj::pj_log_set_level(level);
            previous
        }
    }
}

impl Drop for PjLogging {
    fn drop(&mut self) {
        unsafe {
            pj::pj_log_set_log_func(self.previous);
            pj::pj_log_set_level(self.level);
        }
    }
}

unsafe extern "C" fn pj_message(level: i32, data: *const c_char, length: i32) {
    if data.is_null() || length <= 0 {
        return;
    }
    let data = unsafe { std::slice::from_raw_parts(data.cast(), length as usize) };
    let message = redact(&String::from_utf8_lossy(data));
    match level {
        0 | 1 => tracing::error!(target: "faxe_native::pjsip", %message),
        2 => tracing::warn!(target: "faxe_native::pjsip", %message),
        3 => tracing::info!(target: "faxe_native::pjsip", %message),
        4 => tracing::debug!(target: "faxe_native::pjsip", %message),
        _ => tracing::trace!(target: "faxe_native::pjsip", %message),
    }
}

pub(crate) fn sip_module() -> Box<pj::pjsip_module> {
    Box::new(pj::pjsip_module {
        name: pj::pj_str_t {
            ptr: c"faxe-wire-log".as_ptr().cast_mut(),
            slen: 13,
        },
        id: -1,
        priority: pj::pjsip_module_priority_PJSIP_MOD_PRIORITY_TRANSPORT_LAYER as i32 - 1,
        on_rx_request: Some(receive),
        on_rx_response: Some(receive),
        on_tx_request: Some(transmit),
        on_tx_response: Some(transmit),
        ..Default::default()
    })
}

unsafe extern "C" fn receive(data: *mut pj::pjsip_rx_data) -> pj::pj_bool_t {
    if tracing::enabled!(target: "faxe_native::sip::wire", tracing::Level::TRACE) {
        unsafe {
            let source = CStr::from_ptr((*data).pkt_info.src_name.as_ptr()).to_string_lossy();
            tracing::trace!(target: "faxe_native::sip::wire", peer = %source, port = (*data).pkt_info.src_port, "SIP packet source");
            log_message("RX", (*data).msg_info.msg);
        }
    }
    0
}

unsafe extern "C" fn transmit(data: *mut pj::pjsip_tx_data) -> pj::pj_status_t {
    if tracing::enabled!(target: "faxe_native::sip::wire", tracing::Level::TRACE) {
        unsafe {
            log_message("TX", (*data).msg);
        }
    }
    0
}

unsafe fn log_message(direction: &str, message: *mut pj::pjsip_msg) {
    let mut buffer = vec![0_u8; 65536];
    let length = unsafe { pj::pjsip_msg_print(message, buffer.as_mut_ptr().cast(), buffer.len()) };
    if length < 0 {
        tracing::warn!(target: "faxe_native::sip::wire", direction, "SIP message exceeds diagnostic buffer");
        return;
    }
    let message = redact(&String::from_utf8_lossy(&buffer[..length as usize]));
    tracing::trace!(target: "faxe_native::sip::wire", direction, %message);
}

fn redact(message: &str) -> String {
    let mut redacted = String::new();
    let mut sensitive = false;
    for line in message.lines() {
        if sensitive && line.starts_with([' ', '\t']) {
            continue;
        }
        let lowercase = line.to_ascii_lowercase();
        sensitive = [
            "authorization",
            "proxy-authorization",
            "www-authenticate",
            "proxy-authenticate",
            "authentication-info",
            "proxy-authentication-info",
        ]
        .iter()
        .any(|name| {
            lowercase.match_indices(name).any(|(offset, _)| {
                lowercase[offset + name.len()..]
                    .trim_start()
                    .starts_with(':')
            })
        });
        match sensitive {
            true => {
                let name = line
                    .split_once(':')
                    .map_or("Authentication", |(name, _)| name);
                redacted.push_str(name);
                redacted.push_str(": [REDACTED]");
            }
            false => redacted.push_str(line),
        }
        redacted.push('\n');
    }
    redacted.trim_end().to_owned()
}

/// Configures an embedded logging state without creating an owning wrapper.
pub(crate) unsafe fn spandsp(state: *mut dsp::logging_state_t) {
    if state.is_null() {
        return;
    }
    let level = match () {
        _ if tracing::enabled!(target: "faxe_native::spandsp", tracing::Level::TRACE) => 10,
        _ if tracing::enabled!(target: "faxe_native::spandsp", tracing::Level::DEBUG) => 7,
        _ => 4,
    };
    unsafe {
        dsp::span_log_set_message_handler(state, Some(dsp_message), std::ptr::null_mut());
        dsp::span_log_set_level(state, level | 0x0800);
    }
}

unsafe extern "C" fn dsp_message(_: *mut c_void, level: i32, text: *const c_char) {
    if text.is_null() {
        return;
    }
    let message = unsafe { CStr::from_ptr(text) }.to_string_lossy();
    let message = omit_byte_dump(message.trim_end());
    match level & 0xff {
        1 | 3 => tracing::error!(target: "faxe_native::spandsp", %message),
        2 | 4 => tracing::warn!(target: "faxe_native::spandsp", %message),
        5..=7 => tracing::debug!(target: "faxe_native::spandsp", %message),
        _ => tracing::trace!(target: "faxe_native::spandsp", %message),
    }
}

fn omit_byte_dump(message: &str) -> Cow<'_, str> {
    for marker in ["IFP ", "Tx: ", "Rx: "] {
        if let Some((prefix, suffix)) = message.split_once(marker) {
            let mut bytes = suffix.split_whitespace().peekable();
            if bytes.peek().is_some()
                && bytes.all(|byte| byte.len() == 2 && byte.bytes().all(|b| b.is_ascii_hexdigit()))
            {
                return Cow::Owned(format!("{prefix}{marker}[byte dump omitted]"));
            }
        }
    }
    Cow::Borrowed(message)
}

#[cfg(test)]
mod tests {
    #[test]
    fn fax_byte_dumps_are_omitted_without_losing_protocol_messages() {
        assert_eq!(
            super::omit_byte_dump("T.38 Tx 1: IFP 00 a1 ff"),
            "T.38 Tx 1: IFP [byte dump omitted]"
        );
        assert_eq!(
            super::omit_byte_dump("T.30 Rx:  ff 03 80"),
            "T.30 Rx: [byte dump omitted]"
        );
        assert_eq!(
            super::omit_byte_dump("T.30 Rx: DIS with final frame tag"),
            "T.30 Rx: DIS with final frame tag"
        );
    }
    #[test]
    fn authentication_headers_and_folded_values_are_redacted() {
        let input = "INVITE sip:3003@pbx SIP/2.0\r\naUtHoRiZaTiOn: Digest secret\r\n response=hidden\r\nProxy-Authorization: secret\r\n\tmore-secret\r\nWWW-Authenticate: nonce-secret\r\nAuthorization \t: hidden\r\nCall-ID: visible\r\n\r\nm=image 4000 udptl t38\r\n";
        let output = super::redact(input);
        assert!(!output.contains("secret"));
        assert!(!output.contains("hidden"));
        assert!(output.contains("Call-ID: visible"));
        assert!(output.contains("m=image 4000 udptl t38"));
    }
}
