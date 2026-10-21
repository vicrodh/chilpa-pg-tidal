//! Clear FLAC fragments from fragmented MP4, decoded to their original
//! integer samples.

use crate::state::{Failure, Result};
use symphonia_bundle_flac::{FlacDecoder, FlacReader};
use symphonia_core::audio::{AudioBufferRef, Signal};
use symphonia_core::codecs::{Decoder, DecoderOptions};
use symphonia_core::formats::{FormatOptions, FormatReader};
use symphonia_core::io::MediaSourceStream;

/// The stream header a fragment decodes with: the FLAC marker and STREAMINFO
/// from the init segment's dfLa box, without unrelated metadata.
pub fn metadata(init: &[u8]) -> Result<Vec<u8>> {
    let at = find(init, b"dfLa").filter(|at| *at >= 4).ok_or_else(|| Failure::new("The FLAC initialization metadata is missing."))?;
    let size = u32::from_be_bytes(init[at - 4..at].try_into().unwrap()) as usize;
    let end = (at - 4 + size).min(init.len());
    let body = init.get(at + 8..end).unwrap_or(&[]);
    // The dfLa sample-entry box carries a full-box header then FLAC metadata.
    if body.len() < 38 || body[0] & 0x7f != 0 || u32::from_be_bytes([0, body[1], body[2], body[3]]) != 34 {
        return Err(Failure::new("Invalid FLAC initialization metadata."));
    }
    let mut header = b"fLaC\x80\x00\x00\x22".to_vec();
    header.extend_from_slice(&body[4..38]);
    Ok(header)
}

/// Channels and bits per sample that a stream header declares.
pub fn stream_info(header: &[u8]) -> Option<(usize, u32, u32)> {
    let info = header.get(8..42)?;
    let rate = (u32::from(info[10]) << 12) | (u32::from(info[11]) << 4) | (u32::from(info[12]) >> 4);
    let channels = usize::from((info[12] >> 1) & 0x07) + 1;
    let bits = (((u32::from(info[12]) & 0x01) << 4) | (u32::from(info[13]) >> 4)) + 1;
    Some((channels, bits, rate))
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|window| window == needle)
}

/// The media payload of a fragment: every mdat box, in order.
pub fn payload(segment: &[u8]) -> Result<Vec<u8>> {
    let invalid = || Failure::new("Invalid audio fragment.");
    let (mut position, mut result) = (0usize, Vec::new());
    while position + 8 <= segment.len() {
        let mut size = u32::from_be_bytes(segment[position..position + 4].try_into().unwrap()) as u64;
        let kind = &segment[position + 4..position + 8];
        let mut header = 8u64;
        if size == 1 {
            if position + 16 > segment.len() {
                return Err(invalid());
            }
            size = u64::from_be_bytes(segment[position + 8..position + 16].try_into().unwrap());
            header = 16;
        } else if size == 0 {
            size = (segment.len() - position) as u64;
        }
        if size < header || position as u64 + size > segment.len() as u64 {
            return Err(invalid());
        }
        if kind == b"mdat" {
            result.extend_from_slice(&segment[position + header as usize..position + size as usize]);
        }
        position += size as usize;
    }
    if position != segment.len() || result.is_empty() {
        return Err(Failure::new("The audio fragment has no samples."));
    }
    Ok(result)
}

/// Interleaved little-endian PCM of `expected` frames, with the stream's own
/// bit depth, and its channel count.
pub fn decode(data: Vec<u8>, bits: u32, rate: u32, expected: u64) -> Result<(Vec<u8>, usize)> {
    let mismatch = || Failure::new("The FLAC fragment does not match its sample timeline.");
    let source = MediaSourceStream::new(Box::new(std::io::Cursor::new(data)), Default::default());
    let mut reader = FlacReader::try_new(source, &FormatOptions::default()).map_err(|_| mismatch())?;
    let parameters = reader.default_track().ok_or_else(mismatch)?.codec_params.clone();
    let mut decoder = FlacDecoder::try_new(&parameters, &DecoderOptions::default()).map_err(|_| mismatch())?;
    let width = (bits / 8) as usize;
    let shift = 32 - bits;
    let (mut output, mut samples, mut channels) = (Vec::with_capacity(expected as usize * 2 * width), 0u64, 0usize);
    loop {
        let packet = match reader.next_packet() {
            Ok(packet) => packet,
            Err(symphonia_core::errors::Error::IoError(error)) if error.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(_) => return Err(mismatch()),
        };
        let decoded = decoder.decode(&packet).map_err(|_| mismatch())?;
        let AudioBufferRef::S32(buffer) = decoded else { return Err(mismatch()) };
        let spec = buffer.spec();
        let count = buffer.frames();
        let current = spec.channels.count();
        if spec.rate != rate || !(1..=8).contains(&current) || (channels != 0 && channels != current) || samples + count as u64 > expected {
            return Err(mismatch());
        }
        channels = current;
        let planes: Vec<&[i32]> = (0..channels).map(|channel| buffer.chan(channel)).collect();
        for frame in 0..count {
            for plane in &planes {
                let sample = plane[frame] >> shift;
                output.extend_from_slice(&sample.to_le_bytes()[..width]);
            }
        }
        samples += count as u64;
    }
    if samples != expected || parameters.bits_per_sample != Some(bits) {
        return Err(mismatch());
    }
    Ok((output, channels))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_payload_joins_every_media_box_and_skips_the_rest() {
        let mut segment = Vec::new();
        segment.extend_from_slice(&[0, 0, 0, 12]);
        segment.extend_from_slice(b"moofabcd");
        segment.extend_from_slice(&[0, 0, 0, 11]);
        segment.extend_from_slice(b"mdatxyz");
        assert_eq!(payload(&segment).unwrap(), b"xyz");
        assert!(payload(&segment[..20]).is_err());
    }

    #[test]
    fn a_stream_header_reads_back_its_channels_bits_and_rate() {
        let mut header = b"fLaC\x80\x00\x00\x22".to_vec();
        // 4096-sample blocks, unknown frame sizes, 96 kHz, 2 channels, 24 bits.
        header.extend_from_slice(&[0x10, 0x00, 0x10, 0x00, 0, 0, 0, 0, 0, 0, 0x17, 0x70, 0x03, 0x70]);
        header.extend_from_slice(&[0; 20]);
        assert_eq!(stream_info(&header), Some((2, 24, 96000)));
    }
}
