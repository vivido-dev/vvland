//! One-shot screenshot conversion and safe delivery.

use std::io;
use std::path::{Path, PathBuf};

use ffmpeg_next as ffmpeg;

use crate::cli::{ScreenshotFormat, ScreenshotScale};
use crate::linux::launcher::write_private_file;
use crate::linux::video::{RawFrame, ffmpeg_error};

pub struct EncodedScreenshot {
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub format: ScreenshotFormat,
    pub pixel_format: Option<&'static str>,
}

pub fn encode(
    raw: &RawFrame,
    format: ScreenshotFormat,
    scale: ScreenshotScale,
    quality: u8,
) -> io::Result<EncodedScreenshot> {
    validate_raw(raw)?;
    let denominator = scale.denominator();
    let width = raw.width / denominator;
    let height = raw.height / denominator;
    if width == 0 || height == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "screenshot scale produces an empty image",
        ));
    }
    let (target, pixel_format) = match format {
        ScreenshotFormat::Png => (ffmpeg::format::Pixel::RGB24, None),
        ScreenshotFormat::Jpeg => (ffmpeg::format::Pixel::YUVJ420P, None),
        ScreenshotFormat::Raw => (ffmpeg::format::Pixel::RGBA, Some("rgba")),
    };
    let converted = convert(raw, target, width, height)?;
    let bytes = match format {
        ScreenshotFormat::Png => encode_image(converted, "png", quality)?,
        ScreenshotFormat::Jpeg => encode_image(converted, "mjpeg", quality)?,
        ScreenshotFormat::Raw => packed_plane(&converted, 4)?,
    };
    Ok(EncodedScreenshot {
        bytes,
        width,
        height,
        format,
        pixel_format,
    })
}

pub fn write_output(path: &Path, bytes: &[u8]) -> io::Result<PathBuf> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "screenshot output path must be absolute",
        ));
    }
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "screenshot output path has no parent",
        )
    })?;
    if !parent.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "screenshot output parent does not exist or is not a directory",
        ));
    }
    write_private_file(path, bytes, 0o600)?;
    Ok(path.to_owned())
}

pub fn check_encoders() -> io::Result<()> {
    ffmpeg::init().map_err(ffmpeg_error)?;
    for name in ["png", "mjpeg"] {
        ffmpeg::encoder::find_by_name(name).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("FFmpeg {name} encoder is unavailable"),
            )
        })?;
    }
    Ok(())
}

fn validate_raw(raw: &RawFrame) -> io::Result<()> {
    let expected = usize::try_from(raw.width)
        .ok()
        .and_then(|width| width.checked_mul(4))
        .and_then(|stride| {
            usize::try_from(raw.height)
                .ok()
                .and_then(|height| stride.checked_mul(height))
        })
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "raw frame size overflow"))?;
    if raw.data.len() != expected {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "captured frame length does not match its dimensions",
        ));
    }
    Ok(())
}

fn convert(
    raw: &RawFrame,
    target: ffmpeg::format::Pixel,
    width: u32,
    height: u32,
) -> io::Result<ffmpeg::frame::Video> {
    ffmpeg::init().map_err(ffmpeg_error)?;
    let mut source = ffmpeg::frame::Video::new(raw.format.ffmpeg(), raw.width, raw.height);
    let row_bytes = usize::try_from(raw.width).unwrap_or(0).saturating_mul(4);
    let source_stride = source.stride(0);
    for row in 0..usize::try_from(raw.height).unwrap_or(0) {
        let input_start = row * row_bytes;
        let output_start = row * source_stride;
        source.data_mut(0)[output_start..output_start + row_bytes]
            .copy_from_slice(&raw.data[input_start..input_start + row_bytes]);
    }
    let mut output = ffmpeg::frame::Video::new(target, width, height);
    ffmpeg::software::scaling::Context::get(
        raw.format.ffmpeg(),
        raw.width,
        raw.height,
        target,
        width,
        height,
        ffmpeg::software::scaling::Flags::BILINEAR,
    )
    .map_err(ffmpeg_error)?
    .run(&source, &mut output)
    .map_err(ffmpeg_error)?;
    output.set_pts(Some(0));
    Ok(output)
}

fn encode_image(
    mut frame: ffmpeg::frame::Video,
    encoder_name: &str,
    quality: u8,
) -> io::Result<Vec<u8>> {
    let codec = ffmpeg::encoder::find_by_name(encoder_name).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("FFmpeg {encoder_name} encoder is unavailable"),
        )
    })?;
    let mut encoder = ffmpeg::codec::context::Context::new_with_codec(codec)
        .encoder()
        .video()
        .map_err(ffmpeg_error)?;
    encoder.set_width(frame.width());
    encoder.set_height(frame.height());
    encoder.set_format(frame.format());
    encoder.set_time_base(ffmpeg::Rational(1, 1));
    if encoder_name == "mjpeg" {
        // FFmpeg's MJPEG quality uses MPEG quantizers (2 best, 31 worst) scaled by
        // FF_QP2LAMBDA. Map the public 1..=100 scale monotonically onto that range.
        let quantizer = 2 + usize::from(100_u8.saturating_sub(quality)) * 29 / 99;
        encoder.set_flags(ffmpeg::codec::Flags::QSCALE);
        encoder.set_quality(quantizer * 118);
        frame.set_color_range(ffmpeg::color::Range::JPEG);
    }
    let mut encoder = encoder.open().map_err(ffmpeg_error)?;
    encoder.send_frame(&frame).map_err(ffmpeg_error)?;
    encoder.send_eof().map_err(ffmpeg_error)?;
    let mut bytes = Vec::new();
    loop {
        let mut packet = ffmpeg::Packet::empty();
        if encoder.receive_packet(&mut packet).is_err() {
            break;
        }
        bytes.extend_from_slice(packet.data().unwrap_or_default());
    }
    if bytes.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("FFmpeg {encoder_name} emitted an empty screenshot"),
        ));
    }
    Ok(bytes)
}

fn packed_plane(frame: &ffmpeg::frame::Video, bytes_per_pixel: usize) -> io::Result<Vec<u8>> {
    let row_bytes = usize::try_from(frame.width())
        .ok()
        .and_then(|width| width.checked_mul(bytes_per_pixel))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "raw row size overflow"))?;
    let height = usize::try_from(frame.height())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "raw height overflow"))?;
    let mut bytes = Vec::with_capacity(row_bytes.checked_mul(height).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "raw screenshot size overflow")
    })?);
    for row in 0..height {
        let start = row * frame.stride(0);
        bytes.extend_from_slice(&frame.data(0)[start..start + row_bytes]);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::MetadataExt;
    use std::sync::Arc;

    fn source(format: crate::linux::video::RawPixelFormat) -> RawFrame {
        let mut data = vec![0_u8; 64 * 64 * 4];
        for (index, pixel) in data.chunks_exact_mut(4).enumerate() {
            let x = u8::try_from(index % 64).unwrap();
            let y = u8::try_from(index / 64).unwrap();
            match format {
                crate::linux::video::RawPixelFormat::Bgrx
                | crate::linux::video::RawPixelFormat::Bgra => {
                    pixel.copy_from_slice(&[x, y, x.wrapping_add(y), 255]);
                }
                crate::linux::video::RawPixelFormat::Rgbx
                | crate::linux::video::RawPixelFormat::Rgba => {
                    pixel.copy_from_slice(&[x.wrapping_add(y), y, x, 255]);
                }
            }
        }
        RawFrame {
            format,
            width: 64,
            height: 64,
            pts_us: 0,
            data: Arc::from(data),
        }
    }

    fn decode_dimensions(bytes: &[u8], decoder_name: &str) -> (u32, u32) {
        let codec = ffmpeg::decoder::find_by_name(decoder_name).unwrap();
        let mut decoder = ffmpeg::codec::context::Context::new_with_codec(codec)
            .decoder()
            .video()
            .unwrap();
        decoder.send_packet(&ffmpeg::Packet::copy(bytes)).unwrap();
        decoder.send_eof().unwrap();
        let mut frame = ffmpeg::frame::Video::empty();
        decoder.receive_frame(&mut frame).unwrap();
        (frame.width(), frame.height())
    }

    #[test]
    fn every_pixel_layout_encodes_and_decodes_png_and_jpeg() {
        for layout in [
            crate::linux::video::RawPixelFormat::Bgrx,
            crate::linux::video::RawPixelFormat::Rgbx,
            crate::linux::video::RawPixelFormat::Bgra,
            crate::linux::video::RawPixelFormat::Rgba,
        ] {
            let source = source(layout);
            let png = encode(&source, ScreenshotFormat::Png, ScreenshotScale::Half, 85).unwrap();
            assert!(png.bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
            assert_eq!(decode_dimensions(&png.bytes, "png"), (32, 32));

            let jpeg = encode(&source, ScreenshotFormat::Jpeg, ScreenshotScale::Half, 85).unwrap();
            assert!(jpeg.bytes.starts_with(&[0xff, 0xd8]));
            assert_eq!(decode_dimensions(&jpeg.bytes, "mjpeg"), (32, 32));
        }
    }

    #[test]
    fn raw_is_tightly_packed_rgba_at_the_requested_scale() {
        let raw = encode(
            &source(crate::linux::video::RawPixelFormat::Bgrx),
            ScreenshotFormat::Raw,
            ScreenshotScale::Quarter,
            85,
        )
        .unwrap();
        assert_eq!((raw.width, raw.height), (16, 16));
        assert_eq!(raw.pixel_format, Some("rgba"));
        assert_eq!(raw.bytes.len(), 16 * 16 * 4);
    }

    #[test]
    fn output_is_private_create_new_and_refuses_symlinks() {
        let mut random = [0_u8; 8];
        getrandom::fill(&mut random).unwrap();
        let root = std::env::temp_dir().join(format!(
            "vvland-screenshot-test-{}-{:016x}",
            std::process::id(),
            u64::from_be_bytes(random)
        ));
        fs::create_dir(&root).unwrap();
        let output = root.join("shot.png");
        write_output(&output, b"png").unwrap();
        assert_eq!(fs::read(&output).unwrap(), b"png");
        assert_eq!(fs::metadata(&output).unwrap().mode() & 0o777, 0o600);

        let target = root.join("target");
        fs::write(&target, b"unchanged").unwrap();
        let link = root.join("link.png");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(write_output(&link, b"replacement").is_err());
        assert_eq!(fs::read(&target).unwrap(), b"unchanged");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn encoder_probe_finds_both_required_still_codecs() {
        check_encoders().unwrap();
    }
}
