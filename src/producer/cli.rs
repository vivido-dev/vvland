//! Shared CLI bounds and configuration validation for the Linux producers.
//!
//! The two producers' `Config` structs differ (one carries DRM backend selection), but their
//! bound validation is identical: stream dimensions, frame rate, bitrate, GOP, access-unit
//! ceiling, the Pulse server string, and the single-line XKB/DRM value rule.

use std::io;

/// Configuration values that feed a generated compositor config must be bounded and
/// single-line, preventing injection into the generated compositor configuration.
pub fn safe_config_value(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && !value.bytes().any(|byte| matches!(byte, 0 | b'\n' | b'\r'))
}

/// Validate the stream bounds shared by both producers.
///
/// Returns the same errors both producers printed before the extraction; each producer calls
/// this from its own `Config::validate` and keeps its endpoint, doctor, and backend checks.
#[allow(clippy::too_many_arguments)]
pub fn validate_common_bounds(
    width: Option<u32>,
    height: Option<u32>,
    fps: u32,
    bitrate: u64,
    gop_seconds: u32,
    max_access_unit_bytes: u32,
    audio_capture_server: Option<&str>,
    xkb_layout: &str,
    xkb_model: Option<&str>,
    xkb_variant: Option<&str>,
    xkb_options: Option<&str>,
) -> io::Result<()> {
    if width.is_some() != height.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--width and --height must be supplied together",
        ));
    }
    if width.is_some_and(|width| !(64..=8192).contains(&width))
        || height.is_some_and(|height| !(64..=8192).contains(&height))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "desktop dimensions must be between 64 and 8192 pixels",
        ));
    }
    if !(1..=240).contains(&fps) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--fps must be between 1 and 240",
        ));
    }
    if !(64_000..=200_000_000).contains(&bitrate) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--bitrate must be between 64000 and 200000000",
        ));
    }
    if !(1..=30).contains(&gop_seconds) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--gop-seconds must be between 1 and 30",
        ));
    }
    if !(1024..=16_000_000).contains(&max_access_unit_bytes) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--max-access-unit-bytes must be between 1024 and 16000000",
        ));
    }
    if audio_capture_server
        .is_some_and(|server| server.is_empty() || server.len() > 4096 || server.contains('\0'))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "--audio-capture-server must contain 1 to 4096 bytes without NUL",
        ));
    }
    for value in [Some(xkb_layout), xkb_model, xkb_variant, xkb_options]
        .into_iter()
        .flatten()
    {
        if !safe_config_value(value) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "XKB values must contain 1 to 128 single-line bytes",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    type Bounds<'a> = (
        Option<u32>,
        Option<u32>,
        u32,
        u64,
        u32,
        u32,
        Option<&'a str>,
        &'a str,
        Option<&'a str>,
        Option<&'a str>,
        Option<&'a str>,
    );

    fn valid<'a>() -> Bounds<'a> {
        (
            None, None, 30, 8_000_000, 2, 4_194_304, None, "us", None, None, None,
        )
    }

    #[test]
    fn accepts_default_bounds() {
        let (w, h, fps, b, g, a, s, l, m, v, o) = valid();
        assert!(validate_common_bounds(w, h, fps, b, g, a, s, l, m, v, o).is_ok());
    }

    #[test]
    fn rejects_unpaired_dimensions() {
        let (_, _, fps, b, g, a, s, l, m, v, o) = valid();
        assert!(validate_common_bounds(Some(1280), None, fps, b, g, a, s, l, m, v, o).is_err());
    }

    #[test]
    fn rejects_injected_config_values() {
        let (w, h, fps, b, g, a, s, _, m, v, _) = valid();
        assert!(
            validate_common_bounds(
                w,
                h,
                fps,
                b,
                g,
                a,
                s,
                "us\noutput HEADLESS-1 disable",
                m,
                v,
                None
            )
            .is_err()
        );
        assert!(
            validate_common_bounds(w, h, fps, b, g, a, s, "us", m, v, Some("compose:ralt\r"))
                .is_err()
        );
        assert!(!safe_config_value("bad\nvalue"));
        assert!(safe_config_value("ok-value"));
    }

    #[test]
    fn rejects_out_of_range_stream_settings() {
        let (w, h, _, b, g, a, s, l, m, v, o) = valid();
        assert!(validate_common_bounds(w, h, 0, b, g, a, s, l, m, v, o).is_err());
        assert!(validate_common_bounds(w, h, 30, 0, g, a, s, l, m, v, o).is_err());
        assert!(validate_common_bounds(w, h, 30, b, 0, a, s, l, m, v, o).is_err());
        assert!(validate_common_bounds(w, h, 30, b, g, 0, s, l, m, v, o).is_err());
        assert!(validate_common_bounds(w, h, 30, b, g, a, Some(""), l, m, v, o).is_err());
    }
}
