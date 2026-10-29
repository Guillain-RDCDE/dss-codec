use crate::codec::ds2_qp::Ds2QpDecoder;
use crate::codec::dss_sp::DssSpDecoder;
use crate::codec::grundig_sp::GrundigSpDecoder;
use crate::crypto::ds2_encrypted::{EncryptedDs2BlockDecryptor, ENCRYPTED_MAGIC};
use crate::demux::ds2::{
    demux_ds2, find_ds2_format_type, qp_frame_in_progress, DemuxedDs2, Ds2QpStreamDemuxer,
};
use crate::demux::dss::DssSpStreamDemuxer;
use crate::demux::grundig::GrundigSpStreamDemuxer;
use crate::demux::{
    check_sp_codec, detect_format, ds2_header_size, dss_header_size, sp_codec, AudioFormat,
};
use crate::error::{DecodeError, Result};

pub struct StreamingDecoder {
    prebuffer: Vec<u8>,
    raw_buf: Vec<u8>,
    format: Option<AudioFormat>,
    demuxer: Option<ActiveDemuxer>,
    decoder: Option<ActiveDecoder>,
    finished: bool,
}

pub struct DecryptStreamer {
    password: Option<Vec<u8>>,
    prebuffer: Vec<u8>,
    mode: DecryptMode,
}

pub struct DecryptingDecoderStreamer {
    decryptor: DecryptStreamer,
    inner: StreamingDecoder,
    finished: bool,
}

enum DecryptMode {
    Unknown,
    PassThrough,
    Encrypted(EncryptedDs2BlockDecryptor),
}

enum ActiveDemuxer {
    Dss(DssSpStreamDemuxer),
    Ds2Qp(Ds2QpStreamDemuxer),
    Grundig(GrundigSpStreamDemuxer),
}

enum ActiveDecoder {
    Dss(DssSpDecoder),
    Ds2Qp(Ds2QpDecoder),
    Grundig(GrundigSpDecoder),
}

impl StreamingDecoder {
    pub fn new() -> Self {
        Self {
            prebuffer: Vec::new(),
            raw_buf: Vec::new(),
            format: None,
            demuxer: None,
            decoder: None,
            finished: false,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<f64>> {
        if self.finished {
            return Err(DecodeError::AlreadyFinished);
        }

        if self.format.is_none() {
            self.prebuffer.extend_from_slice(bytes);
            if !self.try_initialize(false)? {
                return Ok(Vec::new());
            }

            let buffered = std::mem::take(&mut self.prebuffer);
            return self.push_active(&buffered);
        }

        self.push_active(bytes)
    }

    pub fn finish(&mut self) -> Result<Vec<f64>> {
        if self.finished {
            return Ok(Vec::new());
        }

        if self.format.is_none() {
            if self.prebuffer.is_empty() {
                self.finished = true;
                return Ok(Vec::new());
            }

            if self.try_initialize(true)? {
                let buffered = std::mem::take(&mut self.prebuffer);
                let mut samples = self.push_active(&buffered)?;
                samples.extend(self.finish_active()?);
                self.finished = true;
                return Ok(samples);
            }

            return if ds2_header_size(&self.prebuffer).is_some() {
                Err(DecodeError::Truncated("DS2 header".to_string()))
            } else if self.prebuffer.len() >= 4 && self.prebuffer[1..4] == *b"dss" {
                Err(DecodeError::Truncated("DSS header".to_string()))
            } else {
                Err(DecodeError::UnsupportedFormat(
                    self.prebuffer.first().copied().unwrap_or(0),
                ))
            };
        }

        let samples = self.finish_active()?;
        self.finished = true;
        Ok(samples)
    }

    pub(crate) fn finish_lenient(&mut self) -> Result<Vec<f64>> {
        if self.finished {
            return Ok(Vec::new());
        }

        if self.format.is_none() {
            if self.prebuffer.is_empty() {
                self.finished = true;
                return Ok(Vec::new());
            }

            if self.try_initialize(true)? {
                let buffered = std::mem::take(&mut self.prebuffer);
                let mut samples = self.push_active(&buffered)?;
                samples.extend(self.finish_active_lenient()?);
                self.finished = true;
                return Ok(samples);
            }

            return if ds2_header_size(&self.prebuffer).is_some() {
                Err(DecodeError::Truncated("DS2 header".to_string()))
            } else if self.prebuffer.len() >= 4 && self.prebuffer[1..4] == *b"dss" {
                Err(DecodeError::Truncated("DSS header".to_string()))
            } else {
                Err(DecodeError::UnsupportedFormat(
                    self.prebuffer.first().copied().unwrap_or(0),
                ))
            };
        }

        let samples = self.finish_active_lenient()?;
        self.finished = true;
        Ok(samples)
    }

    pub fn format(&self) -> Option<AudioFormat> {
        self.format
    }

    pub fn native_rate(&self) -> Option<u32> {
        self.format.map(|fmt| fmt.native_sample_rate())
    }

    fn try_initialize(&mut self, at_end: bool) -> Result<bool> {
        if let Some(format) = detect_format(&self.prebuffer) {
            let header_size = match format {
                AudioFormat::DssSp | AudioFormat::GrundigSp => dss_header_size(&self.prebuffer),
                _ => ds2_header_size(&self.prebuffer),
            };
            let codec_pending = header_size
                .is_some_and(|size| find_ds2_format_type(&self.prebuffer, size).is_none());
            if codec_pending && !at_end {
                return Ok(false);
            }
            self.initialize_for_format(format)?;
            return Ok(true);
        }

        if self.prebuffer.len() >= 4 {
            let is_dss_prefix = self.prebuffer[1..4] == *b"dss";
            let is_ds2_prefix = ds2_header_size(&self.prebuffer).is_some();
            if !is_dss_prefix && !is_ds2_prefix {
                return Err(DecodeError::UnsupportedFormat(
                    self.prebuffer.first().copied().unwrap_or(0),
                ));
            }
        }

        Ok(false)
    }

    fn initialize_for_format(&mut self, format: AudioFormat) -> Result<()> {
        let mut codec = 0;
        if matches!(format, AudioFormat::DssSp | AudioFormat::Ds2Sp) {
            codec = sp_codec(&self.prebuffer).unwrap_or(0);
            check_sp_codec(codec)?;
        }
        let variable_rate = codec == 1;
        self.format = Some(format);
        match format {
            AudioFormat::DssSp => {
                let version = self.prebuffer[0];
                self.demuxer = Some(ActiveDemuxer::Dss(DssSpStreamDemuxer::new(
                    version,
                    variable_rate,
                )));
                self.decoder = Some(ActiveDecoder::Dss(DssSpDecoder::with_variable_rate(
                    variable_rate,
                )));
            }
            AudioFormat::Ds2Sp => {
                let header_size = ds2_header_size(&self.prebuffer).unwrap_or(3 * 512);
                self.demuxer = Some(ActiveDemuxer::Dss(DssSpStreamDemuxer::with_header_size(
                    header_size,
                    variable_rate,
                )));
                self.decoder = Some(ActiveDecoder::Dss(DssSpDecoder::with_variable_rate(
                    variable_rate,
                )));
            }
            AudioFormat::Ds2Qp | AudioFormat::Ds2Qp7 => {
                self.demuxer = Some(ActiveDemuxer::Ds2Qp(Ds2QpStreamDemuxer::new()));
                self.decoder = Some(ActiveDecoder::Ds2Qp(Ds2QpDecoder::new()));
            }
            AudioFormat::GrundigSp => {
                let header_blocks = self.prebuffer[0];
                self.demuxer =
                    Some(ActiveDemuxer::Grundig(GrundigSpStreamDemuxer::new(header_blocks)));
                self.decoder = Some(ActiveDecoder::Grundig(GrundigSpDecoder::new()));
            }
        }
        Ok(())
    }

    fn push_active(&mut self, bytes: &[u8]) -> Result<Vec<f64>> {
        if matches!(self.format, Some(AudioFormat::Ds2Qp | AudioFormat::Ds2Qp7)) {
            self.raw_buf.extend_from_slice(bytes);
            return Ok(Vec::new());
        }

        let frames = match self.demuxer.as_mut() {
            Some(ActiveDemuxer::Dss(demuxer)) => demuxer.push(bytes)?,
            Some(ActiveDemuxer::Ds2Qp(demuxer)) => demuxer.push(bytes)?,
            Some(ActiveDemuxer::Grundig(demuxer)) => demuxer.push(bytes)?,
            None => return Ok(Vec::new()),
        };

        self.decode_frames(frames)
    }

    fn finish_active(&mut self) -> Result<Vec<f64>> {
        if matches!(self.format, Some(AudioFormat::Ds2Qp | AudioFormat::Ds2Qp7)) {
            if qp_frame_in_progress(&self.raw_buf) {
                return Err(DecodeError::Truncated("DS2 QP frame".to_string()));
            }
            return self.decode_buffered_ds2_qp();
        }

        let frames = match self.demuxer.as_mut() {
            Some(ActiveDemuxer::Dss(demuxer)) => demuxer.finish()?,
            Some(ActiveDemuxer::Ds2Qp(demuxer)) => demuxer.finish()?,
            Some(ActiveDemuxer::Grundig(demuxer)) => demuxer.finish()?,
            None => Vec::new(),
        };

        self.decode_frames(frames)
    }

    fn finish_active_lenient(&mut self) -> Result<Vec<f64>> {
        if matches!(self.format, Some(AudioFormat::Ds2Qp | AudioFormat::Ds2Qp7)) {
            return self.decode_buffered_ds2_qp();
        }

        let frames = match self.demuxer.as_mut() {
            Some(ActiveDemuxer::Dss(demuxer)) => demuxer.finish_lenient()?,
            Some(ActiveDemuxer::Ds2Qp(demuxer)) => demuxer.finish_lenient()?,
            Some(ActiveDemuxer::Grundig(demuxer)) => demuxer.finish_lenient()?,
            None => Vec::new(),
        };

        self.decode_frames(frames)
    }

    fn decode_buffered_ds2_qp(&mut self) -> Result<Vec<f64>> {
        let demuxed = demux_ds2(&self.raw_buf)?;
        match (demuxed, self.decoder.as_mut()) {
            (DemuxedDs2::QpSegments { segments, .. }, Some(ActiveDecoder::Ds2Qp(decoder))) => {
                Ok(decoder.decode_qp_segments(&segments))
            }
            (DemuxedDs2::Qp7Segments { segments, .. }, Some(ActiveDecoder::Ds2Qp(decoder))) => {
                Ok(decoder.decode_qp7_segments(&segments))
            }
            _ => Ok(Vec::new()),
        }
    }

    fn decode_frames(&mut self, frames: Vec<Vec<u8>>) -> Result<Vec<f64>> {
        let mut samples = Vec::new();
        match self.decoder.as_mut() {
            Some(ActiveDecoder::Dss(decoder)) => {
                for frame in frames {
                    let frame_samples = decoder.decode_frame(&frame);
                    samples.extend(frame_samples.into_iter().map(|sample| sample as f64));
                }
            }
            Some(ActiveDecoder::Ds2Qp(decoder)) => {
                for frame in frames {
                    samples.extend_from_slice(&decoder.decode_frame(&frame));
                }
            }
            Some(ActiveDecoder::Grundig(decoder)) => {
                // The Grundig demuxer emits every frame at once (at stream
                // finish). Each frame buffers 12 kHz PCM; finish() runs the
                // polyphase resampler over the whole utterance -> 16 kHz.
                if !frames.is_empty() {
                    for frame in frames {
                        let _ = decoder.decode_frame(&frame);
                    }
                    samples.extend(decoder.finish().into_iter().map(|s| s as f64));
                }
            }
            None => {}
        }

        Ok(samples)
    }
}

impl DecryptStreamer {
    pub fn new(password: Option<&[u8]>) -> Self {
        Self {
            password: password.map(|value| value.to_vec()),
            prebuffer: Vec::new(),
            mode: DecryptMode::Unknown,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<u8>> {
        match &mut self.mode {
            DecryptMode::PassThrough => Ok(bytes.to_vec()),
            DecryptMode::Encrypted(decryptor) => decryptor.push(bytes),
            DecryptMode::Unknown => {
                self.prebuffer.extend_from_slice(bytes);
                if self.prebuffer.len() < 4 {
                    return Ok(Vec::new());
                }

                if self.prebuffer.starts_with(&ENCRYPTED_MAGIC) {
                    let password = self.password.as_deref().ok_or_else(|| {
                        DecodeError::EncryptedDs2(
                            "password required for encrypted DS2 input".to_string(),
                        )
                    })?;
                    let mut decryptor = EncryptedDs2BlockDecryptor::new(password);
                    let buffered = std::mem::take(&mut self.prebuffer);
                    let plain = decryptor.push(&buffered)?;
                    self.mode = DecryptMode::Encrypted(decryptor);
                    return Ok(plain);
                }

                if is_plain_prefix(&self.prebuffer) {
                    let buffered = std::mem::take(&mut self.prebuffer);
                    self.mode = DecryptMode::PassThrough;
                    return Ok(buffered);
                }

                Err(DecodeError::UnsupportedFormat(
                    self.prebuffer.first().copied().unwrap_or(0),
                ))
            }
        }
    }

    pub(crate) fn finish_lenient(&mut self) -> Result<Vec<u8>> {
        match &mut self.mode {
            DecryptMode::Encrypted(decryptor) => decryptor.finish_lenient(),
            _ => self.finish(),
        }
    }

    pub fn finish(&mut self) -> Result<Vec<u8>> {
        match &mut self.mode {
            DecryptMode::PassThrough => Ok(std::mem::take(&mut self.prebuffer)),
            DecryptMode::Encrypted(decryptor) => decryptor.finish(),
            DecryptMode::Unknown => {
                if self.prebuffer.is_empty() {
                    return Ok(Vec::new());
                }

                if ENCRYPTED_MAGIC.starts_with(&self.prebuffer) {
                    return Err(DecodeError::Truncated("encrypted DS2 header".to_string()));
                }

                if self.prebuffer.len() >= 4 && !is_plain_prefix(&self.prebuffer) {
                    return Err(DecodeError::UnsupportedFormat(
                        self.prebuffer.first().copied().unwrap_or(0),
                    ));
                }

                Ok(std::mem::take(&mut self.prebuffer))
            }
        }
    }
}

impl DecryptingDecoderStreamer {
    pub fn new(password: Option<&[u8]>) -> Self {
        Self {
            decryptor: DecryptStreamer::new(password),
            inner: StreamingDecoder::new(),
            finished: false,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<f64>> {
        if self.finished {
            return Err(DecodeError::AlreadyFinished);
        }

        let plain = self.decryptor.push(bytes)?;
        self.inner.push(&plain)
    }

    pub fn finish(&mut self) -> Result<Vec<f64>> {
        if self.finished {
            return Ok(Vec::new());
        }

        let plain = self.decryptor.finish_lenient()?;
        let mut samples = self.inner.push(&plain)?;
        samples.extend(self.inner.finish()?);
        self.finished = true;
        Ok(samples)
    }

    pub fn format(&self) -> Option<AudioFormat> {
        self.inner.format()
    }

    pub fn native_rate(&self) -> Option<u32> {
        self.inner.native_rate()
    }

    pub(crate) fn finish_lenient(&mut self) -> Result<Vec<f64>> {
        if self.finished {
            return Ok(Vec::new());
        }

        let plain = self.decryptor.finish_lenient()?;
        let mut samples = self.inner.push(&plain)?;
        samples.extend(self.inner.finish_lenient()?);
        self.finished = true;
        Ok(samples)
    }
}

impl Default for DecryptingDecoderStreamer {
    fn default() -> Self {
        Self::new(None)
    }
}

impl Default for StreamingDecoder {
    fn default() -> Self {
        Self::new()
    }
}

fn is_plain_prefix(bytes: &[u8]) -> bool {
    ds2_header_size(bytes).is_some() || (bytes.len() >= 4 && bytes[1..4] == *b"dss")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_ds2_header_only(mode: u8) -> Vec<u8> {
        let mut data = vec![0u8; 0x600 + 512];
        data[..4].copy_from_slice(b"\x03ds2");
        // detect_ds2_format_type only trusts a block's mode byte when its
        // frame count is non-zero, as in every real recording
        data[0x600 + 2] = 1;
        data[0x600 + 4] = mode;
        data
    }

    fn sp_block(frame_count: u8) -> Vec<u8> {
        let mut block = vec![0u8; 512];
        block[1] = 3;
        block[2] = frame_count;
        for (i, byte) in block[6..].iter_mut().enumerate() {
            *byte = (i as u8).wrapping_mul(37).wrapping_add(11);
        }
        block
    }

    fn sp_file(prefix: &[u8; 4], header_size: usize, codec: u8) -> Vec<u8> {
        let mut data = vec![0u8; header_size];
        data[..4].copy_from_slice(prefix);
        let mut block = sp_block(3);
        block[4] = codec;
        data.extend_from_slice(&block);
        data
    }

    fn stream_decode(data: &[u8], chunk: usize) -> Result<(Vec<f64>, Option<AudioFormat>)> {
        let mut decoder = StreamingDecoder::new();
        let mut samples = Vec::new();
        for part in data.chunks(chunk) {
            samples.extend(decoder.push(part)?);
        }
        samples.extend(decoder.finish()?);
        Ok((samples, decoder.format()))
    }

    #[test]
    fn test_streaming_decoder_ds2_sp_decodes_like_dss_sp() {
        let ds2 = sp_file(b"\x03ds2", 0x600, 0);
        let dss = sp_file(b"\x02dss", 0x400, 0);
        let (ds2_samples, ds2_format) = stream_decode(&ds2, 100).unwrap();
        let (dss_samples, dss_format) = stream_decode(&dss, 100).unwrap();
        assert_eq!(ds2_format, Some(AudioFormat::Ds2Sp));
        assert_eq!(dss_format, Some(AudioFormat::DssSp));
        assert_eq!(ds2_samples.len(), 3 * 264);
        assert_eq!(ds2_samples, dss_samples);
        assert_eq!(crate::decode_to_buffer(&ds2).unwrap().samples, dss_samples);
    }

    #[test]
    fn test_streaming_decoder_variable_rate_matches_batch() {
        let dss = sp_file(b"\x02dss", 0x400, 1);
        let ds2 = sp_file(b"\x03ds2", 0x600, 1);
        let (dss_samples, dss_format) = stream_decode(&dss, 100).unwrap();
        let (ds2_samples, ds2_format) = stream_decode(&ds2, 1).unwrap();
        assert_eq!(dss_format, Some(AudioFormat::DssSp));
        assert_eq!(ds2_format, Some(AudioFormat::Ds2Sp));
        assert_eq!(dss_samples.len(), 3 * 264);
        assert_eq!(ds2_samples, dss_samples);
        assert_eq!(crate::decode_to_buffer(&dss).unwrap().samples, dss_samples);
        assert_eq!(crate::decode_to_buffer(&ds2).unwrap().samples, dss_samples);

        let (fixed, _) = stream_decode(&sp_file(b"\x02dss", 0x400, 0), 100).unwrap();
        assert_ne!(fixed, dss_samples);
    }

    #[test]
    fn test_streaming_decoder_cut_qp7_keeps_last_frame() {
        let mut data = make_ds2_header_only(7);
        data.truncate(0x600 + 6 + 13);
        data[0x600 + 1] = 3;
        data[0x600 + 2] = 2;
        data[0x600 + 5] = 0xff;
        for (i, byte) in data[0x600 + 6..].iter_mut().enumerate() {
            *byte = (i as u8).wrapping_mul(37).wrapping_add(11);
        }
        data[0x600 + 7] &= 0x7f;
        let err = stream_decode(&data, 1).unwrap_err();
        assert!(matches!(err, DecodeError::Truncated(what) if what == "DS2 QP frame"));
        let mut decoder = StreamingDecoder::new();
        let mut samples = decoder.push(&data).unwrap();
        samples.extend(decoder.finish_lenient().unwrap());
        assert_eq!(decoder.format(), Some(AudioFormat::Ds2Qp7));
        assert_eq!(samples.len(), 2 * 256);
        assert_eq!(crate::decode_to_buffer(&data).unwrap().samples, samples);
    }

    #[test]
    fn test_streaming_decoder_qp_strict_finish_accepts_cut_after_declared_frames() {
        let data = make_ds2_header_only(6);
        let (expected, _) = stream_decode(&data, 64).unwrap();
        assert_eq!(expected.len(), 256);
        for end in [0x600 + 6 + 56, 0x600 + 300, data.len() - 1] {
            let (samples, format) = stream_decode(&data[..end], 64).unwrap();
            assert_eq!(format, Some(AudioFormat::Ds2Qp));
            assert_eq!(samples, expected);
        }
        let mut two = data.clone();
        two.extend_from_slice(&data[0x600..]);
        for end in [data.len() + 1, data.len() + 5] {
            let (samples, _) = stream_decode(&two[..end], 64).unwrap();
            assert_eq!(samples, expected);
        }
    }

    #[test]
    fn test_streaming_decoder_qp_strict_finish_rejects_frame_into_cut_block_header() {
        let mut data = make_ds2_header_only(6);
        data[0x600 + 1] = 3;
        data[0x600 + 2] = 10;
        data[0x600 + 5] = 0xff;
        for (i, byte) in data[0x600 + 6..].iter_mut().enumerate() {
            *byte = (i as u8).wrapping_mul(37).wrapping_add(11);
        }
        let mut next = data[0x600..].to_vec();
        next[1] = 30;
        next[2] = 1;
        data.extend_from_slice(&next);
        let whole = 0x600 + 512;
        for end in [whole + 1, whole + 5, whole + 6, whole + 30] {
            let cut = &data[..end];
            let err = stream_decode(cut, 64).unwrap_err();
            assert!(matches!(err, DecodeError::Truncated(what) if what == "DS2 QP frame"));
            let mut decoder = StreamingDecoder::new();
            let mut lenient = decoder.push(cut).unwrap();
            lenient.extend(decoder.finish_lenient().unwrap());
            assert_eq!(lenient.len(), 10 * 256);
            assert_eq!(crate::decode_to_buffer(cut).unwrap().samples, lenient);
        }
    }

    fn dss_with_header_blocks(first_byte: u8, codec: u8) -> Vec<u8> {
        let mut data = sp_file(b"\x02dss", usize::from(first_byte.max(1)) * 512, codec);
        data[0] = first_byte;
        data
    }

    #[test]
    fn test_streaming_decoder_reads_any_dss_header_block_count() {
        let (expected, _) = stream_decode(&sp_file(b"\x02dss", 0x400, 0), 100).unwrap();
        assert_eq!(expected.len(), 3 * 264);
        for first_byte in [0u8, 1, 3, 8, 19, 255] {
            let data = dss_with_header_blocks(first_byte, 0);
            for chunk in [7, data.len()] {
                let (samples, format) = stream_decode(&data, chunk).unwrap();
                assert_eq!(format, Some(AudioFormat::DssSp));
                assert_eq!(samples, expected);
            }
            assert_eq!(crate::decode_to_buffer(&data).unwrap().samples, expected);
            let mut decoder = DecryptingDecoderStreamer::new(None);
            let mut samples = decoder.push(&data).unwrap();
            samples.extend(decoder.finish().unwrap());
            assert_eq!(samples, expected);
        }
    }

    #[test]
    fn test_streaming_decoder_reads_variable_rate_sp_after_six_header_blocks() {
        let (expected, _) = stream_decode(&sp_file(b"\x02dss", 0x400, 1), 100).unwrap();
        assert_eq!(expected.len(), 3 * 264);
        let data = dss_with_header_blocks(6, 1);
        for chunk in [7, 100, data.len()] {
            let (samples, format) = stream_decode(&data, chunk).unwrap();
            assert_eq!(format, Some(AudioFormat::DssSp));
            assert_eq!(samples, expected);
        }
        let buffer = crate::decode_to_buffer(&data).unwrap();
        assert_eq!(buffer.format, AudioFormat::DssSp);
        assert_eq!(buffer.native_rate, 11000);
        assert_eq!(buffer.samples, expected);
        let mut decoder = StreamingDecoder::new();
        let _ = decoder.push(&data[..6 * 512 + 5]).unwrap();
        assert_eq!(decoder.format(), None);

        let grundig = dss_with_header_blocks(6, 0);
        assert_eq!(stream_decode(&grundig, 7).unwrap().1, Some(AudioFormat::GrundigSp));
        let buffer = crate::decode_to_buffer(&grundig).unwrap();
        assert_eq!(buffer.format, AudioFormat::GrundigSp);
        assert_eq!(buffer.native_rate, 16000);
    }

    #[test]
    fn test_streaming_decoder_dss_long_header_cut_is_truncated() {
        for first_byte in [8u8, 19] {
            let data = dss_with_header_blocks(first_byte, 0);
            let cut = &data[..usize::from(first_byte) * 512 - 1];
            let err = stream_decode(cut, 64).unwrap_err();
            assert!(matches!(err, DecodeError::Truncated(what) if what == "DSS header"));
            let mut decoder = StreamingDecoder::new();
            let _ = decoder.push(cut).unwrap();
            assert!(matches!(
                decoder.finish_lenient(),
                Err(DecodeError::Truncated(what)) if what == "DSS header"
            ));
            assert!(matches!(
                crate::decode_to_buffer(cut),
                Err(DecodeError::Truncated(what)) if what == "DSS header"
            ));
        }
    }

    #[test]
    fn test_streaming_decoder_refuses_other_codecs_after_any_dss_header() {
        for first_byte in [0u8, 6, 8, 19] {
            for codec in [2u8, 6] {
                let data = dss_with_header_blocks(first_byte, codec);
                let err = stream_decode(&data, 100).unwrap_err();
                assert!(matches!(err, DecodeError::UnsupportedCodec(c) if c == codec));
                assert!(matches!(
                    crate::decode_to_buffer(&data),
                    Err(DecodeError::UnsupportedCodec(c)) if c == codec
                ));
            }
        }
    }

    #[test]
    fn test_streaming_decoder_refuses_lp() {
        for (prefix, header_size) in [(b"\x03ds2", 0x600), (b"\x02dss", 0x400)] {
            let data = sp_file(prefix, header_size, 2);
            let err = stream_decode(&data, 100).unwrap_err();
            assert!(matches!(err, DecodeError::UnsupportedCodec(2)));
            let err = stream_decode(&data, data.len()).unwrap_err();
            assert!(matches!(err, DecodeError::UnsupportedCodec(2)));
            assert!(matches!(
                crate::decode_to_buffer(&data),
                Err(DecodeError::UnsupportedCodec(2))
            ));
        }
    }

    #[test]
    fn test_streaming_decoder_refuses_unknown_dss_codec() {
        let data = sp_file(b"\x02dss", 0x400, 5);
        let err = stream_decode(&data, 64).unwrap_err();
        assert!(matches!(err, DecodeError::UnsupportedCodec(5)));
    }

    #[test]
    fn test_streaming_decoder_waits_for_dss_block_with_frames() {
        let mut data = sp_file(b"\x02dss", 0x400, 0);
        data[0x400 + 2] = 0;
        data.extend_from_slice(&sp_block(3));
        let mut decoder = StreamingDecoder::new();
        let mut samples = decoder.push(&data[..0x400 + 512 + 5]).unwrap();
        assert_eq!(decoder.format(), None);
        samples.extend(decoder.push(&data[0x400 + 512 + 5..]).unwrap());
        assert_eq!(decoder.format(), Some(AudioFormat::DssSp));
        samples.extend(decoder.finish().unwrap());
        assert_eq!(samples.len(), 3 * 264);
    }

    #[test]
    fn test_streaming_decoder_ignores_dss_header_codec_byte() {
        let data = sp_file(b"\x02dss", 0x400, 0);
        let (expected, _) = stream_decode(&data, 100).unwrap();
        for byte in [1u8, 2, 0xff] {
            let mut marked = data.clone();
            marked[0x2a4] = byte;
            let (samples, format) = stream_decode(&marked, 100).unwrap();
            assert_eq!(format, Some(AudioFormat::DssSp));
            assert_eq!(samples, expected);
            assert_eq!(crate::decode_to_buffer(&marked).unwrap().samples, expected);
        }
    }

    #[test]
    fn test_streaming_decoder_refuses_lp_after_empty_dss_block() {
        let mut data = sp_file(b"\x02dss", 0x400, 0);
        data[0x400 + 2] = 0;
        let mut block = sp_block(2);
        block[4] = 2;
        data.extend_from_slice(&block);
        let err = stream_decode(&data, 7).unwrap_err();
        assert!(matches!(err, DecodeError::UnsupportedCodec(2)));
        assert!(matches!(
            crate::decode_to_buffer(&data),
            Err(DecodeError::UnsupportedCodec(2))
        ));
    }

    #[test]
    fn test_streaming_decoder_refuses_lp_in_partial_dss_block() {
        let data = sp_file(b"\x02dss", 0x400, 2);
        let cut = &data[..0x400 + 6];
        let err = stream_decode(cut, 7).unwrap_err();
        assert!(matches!(err, DecodeError::UnsupportedCodec(2)));
        let err = StreamingDecoder::new().push(cut).unwrap_err();
        assert!(matches!(err, DecodeError::UnsupportedCodec(2)));
        assert!(matches!(
            crate::decode_to_buffer(cut),
            Err(DecodeError::UnsupportedCodec(2))
        ));
    }

    #[test]
    fn test_streaming_decoder_dss_without_frames_ends_empty() {
        let mut data = sp_file(b"\x02dss", 0x400, 2);
        data[0x400 + 2] = 0;
        let (samples, format) = stream_decode(&data, 64).unwrap();
        assert!(samples.is_empty());
        assert_eq!(format, Some(AudioFormat::DssSp));
    }

    fn ds2_with_empty_first_block(codec: u8) -> Vec<u8> {
        let mut data = sp_file(b"\x03ds2", 0x600, 0);
        data[0x600 + 2] = 0;
        let mut block = sp_block(2);
        block[4] = codec;
        data.extend_from_slice(&block);
        data
    }

    #[test]
    fn test_streaming_decoder_waits_for_ds2_block_with_frames() {
        let data = ds2_with_empty_first_block(6);
        let mut decoder = StreamingDecoder::new();
        let _ = decoder.push(&data[..0x600 + 512]).unwrap();
        assert_eq!(decoder.format(), None);
        let _ = decoder.push(&data[0x600 + 512..0x600 + 512 + 6]).unwrap();
        assert_eq!(decoder.format(), Some(AudioFormat::Ds2Qp));
    }

    #[test]
    fn test_streaming_decoder_pause_block_sets_qp_format() {
        let mut data = make_ds2_header_only(6);
        data[0x600 + 1] = 3;
        data[0x600 + 2] = 0;
        data[0x600 + 5] = 3;
        for (i, byte) in data[0x600 + 6..].iter_mut().enumerate() {
            *byte = (i as u8).wrapping_mul(37).wrapping_add(11);
        }
        let mut counted = data.clone();
        counted[0x600 + 2] = 3;
        counted[0x600 + 5] = 0xff;

        let mut decoder = StreamingDecoder::new();
        let _ = decoder.push(&data[..0x600 + 5]).unwrap();
        assert_eq!(decoder.format(), None);
        let _ = decoder.push(&data[0x600 + 5..0x600 + 6]).unwrap();
        assert_eq!(decoder.format(), Some(AudioFormat::Ds2Qp));

        let (expected, _) = stream_decode(&counted, 1).unwrap();
        assert_eq!(expected.len(), 3 * 256);
        for chunk in [1, 7, data.len()] {
            let (samples, format) = stream_decode(&data, chunk).unwrap();
            assert_eq!(format, Some(AudioFormat::Ds2Qp));
            assert_eq!(samples, expected);
        }
        let buffer = crate::decode_to_buffer(&data).unwrap();
        assert_eq!(buffer.format, AudioFormat::Ds2Qp);
        assert_eq!(buffer.native_rate, 16000);
        assert_eq!(buffer.samples, expected);
    }

    #[test]
    fn test_streaming_decoder_refuses_lp_after_empty_ds2_block() {
        let data = ds2_with_empty_first_block(2);
        let err = stream_decode(&data, 7).unwrap_err();
        assert!(matches!(err, DecodeError::UnsupportedCodec(2)));
    }

    #[test]
    fn test_streaming_decoder_ds2_without_frames_ends_empty() {
        let mut data = sp_file(b"\x03ds2", 0x600, 2);
        data[0x600 + 2] = 0;
        let (samples, format) = stream_decode(&data, 64).unwrap();
        assert!(samples.is_empty());
        assert_eq!(format, Some(AudioFormat::Ds2Sp));
    }

    #[test]
    fn test_streaming_decoder_ds2_cut_inside_first_block_header() {
        for mode in [0u8, 6, 7] {
            let data = sp_file(b"\x03ds2", 0x600, mode);
            let cut = &data[..0x600 + 5];
            let err = stream_decode(cut, 7).unwrap_err();
            assert!(matches!(err, DecodeError::Truncated(what) if what == "DS2 header"));
            let mut decoder = StreamingDecoder::new();
            let _ = decoder.push(cut).unwrap();
            assert!(matches!(
                decoder.finish_lenient(),
                Err(DecodeError::Truncated(what)) if what == "DS2 header"
            ));
            assert!(matches!(
                crate::decode_to_buffer(cut),
                Err(DecodeError::Truncated(what)) if what == "DS2 header"
            ));
        }
    }

    #[test]
    fn test_streaming_decoder_dss_cut_inside_header() {
        let data = sp_file(b"\x02dss", 0x400, 0);
        let mut decoder = StreamingDecoder::new();
        let _ = decoder.push(&data[..0x400 - 1]).unwrap();
        assert!(matches!(
            decoder.finish(),
            Err(DecodeError::Truncated(what)) if what == "DSS header"
        ));
        let mut decoder = StreamingDecoder::new();
        let _ = decoder.push(&data[..0x400 - 1]).unwrap();
        assert!(matches!(
            decoder.finish_lenient(),
            Err(DecodeError::Truncated(what)) if what == "DSS header"
        ));
    }

    #[test]
    fn test_streaming_decoder_grundig_strict_finish_reports_cut() {
        let mut whole = vec![0u8; 6 * 512];
        whole[..4].copy_from_slice(b"\x06dss");
        let mut block = vec![0u8; 512];
        block[..6].copy_from_slice(&[0x00, 3, 2, 0xff, 0x00, 0xff]);
        for (i, byte) in block[6..].iter_mut().enumerate() {
            *byte = (i as u8).wrapping_mul(37).wrapping_add(11);
        }
        whole.extend_from_slice(&block);

        let (samples, format) = stream_decode(&whole, 37).unwrap();
        assert_eq!(format, Some(AudioFormat::GrundigSp));
        assert!(!samples.is_empty());
        assert_eq!(crate::decode_to_buffer(&whole).unwrap().samples, samples);

        let data = &whole[..6 * 512 + 6 + 70];
        let err = stream_decode(data, 37).unwrap_err();
        assert!(matches!(err, DecodeError::Truncated(_)));

        let mut decoder = StreamingDecoder::new();
        let mut lenient = Vec::new();
        for part in data.chunks(37) {
            lenient.extend(decoder.push(part).unwrap());
        }
        lenient.extend(decoder.finish_lenient().unwrap());
        assert_eq!(decoder.format(), Some(AudioFormat::GrundigSp));
        assert!(!lenient.is_empty());
        assert_eq!(crate::decode_to_buffer(data).unwrap().samples, lenient);

        let data = &whole[..1000];
        let err = stream_decode(data, 37).unwrap_err();
        assert!(matches!(err, DecodeError::Truncated(what) if what == "Grundig header"));
        assert!(matches!(
            crate::decode_to_buffer(data),
            Err(DecodeError::Truncated(what)) if what == "Grundig header"
        ));
    }

    #[test]
    fn test_streaming_decoder_detects_only_when_enough_bytes_arrive() {
        let data = make_ds2_header_only(6);
        let mut decoder = StreamingDecoder::new();

        let first = decoder.push(&data[..4]).unwrap();
        assert!(first.is_empty());
        assert_eq!(decoder.format(), None);
        assert_eq!(decoder.native_rate(), None);

        let second = decoder.push(&data[4..]).unwrap();
        assert!(second.is_empty());
        assert_eq!(decoder.format(), Some(AudioFormat::Ds2Qp));
        assert_eq!(decoder.native_rate(), Some(16000));
    }

    #[test]
    fn test_streaming_decoder_truncated_header_on_finish() {
        let mut decoder = StreamingDecoder::new();
        let _ = decoder.push(b"\x03ds2").unwrap();

        let err = decoder.finish().unwrap_err();
        assert!(matches!(err, DecodeError::Truncated(_)));
    }

    #[test]
    fn test_streaming_decoder_two_block_header_detects() {
        let mut data = vec![0u8; 1024 + 512];
        data[..4].copy_from_slice(b"\x02ds2");
        data[1024 + 2] = 1;
        data[1024 + 4] = 6;
        let mut decoder = StreamingDecoder::new();
        let _ = decoder.push(&data[..1024 + 4]).unwrap();
        assert_eq!(decoder.format(), None);
        let _ = decoder.push(&data[1024 + 4..]).unwrap();
        assert_eq!(decoder.format(), Some(AudioFormat::Ds2Qp));
        let tail = decoder.finish().unwrap();
        assert_eq!(tail.len(), 256);
    }

    #[test]
    fn test_streaming_decoder_one_block_header_for_first_byte_one() {
        let mut data = vec![0u8; 512 + 512];
        data[..4].copy_from_slice(b"\x01ds2");
        data[512 + 2] = 1;
        data[512 + 3] = 0xff;
        data[512 + 4] = 6;
        let mut decoder = StreamingDecoder::new();
        let _ = decoder.push(&data[..0x203]).unwrap();
        assert_eq!(decoder.format(), None);
        let _ = decoder.push(&data[0x203..]).unwrap();
        assert_eq!(decoder.format(), Some(AudioFormat::Ds2Qp));
        let tail = decoder.finish().unwrap();
        assert_eq!(tail.len(), 256);
    }

    #[test]
    fn test_streaming_decoder_truncated_long_header_on_finish() {
        let mut decoder = StreamingDecoder::new();
        let _ = decoder.push(&[0x08, b'd', b's', b'2', 0, 0]).unwrap();
        let err = decoder.finish().unwrap_err();
        assert!(matches!(err, DecodeError::Truncated(_)));
        let mut decoder = StreamingDecoder::new();
        let _ = decoder.push(&[0x11, b'd', b's', b'2', 0, 0]).unwrap();
        let err = decoder.finish().unwrap_err();
        assert!(matches!(err, DecodeError::Truncated(_)));
    }

    #[test]
    fn test_streaming_decoder_rejects_header_byte_out_of_range() {
        let mut decoder = StreamingDecoder::new();
        let err = decoder.push(b"\x00ds2").unwrap_err();
        assert!(matches!(err, DecodeError::UnsupportedFormat(0)));
    }

    #[test]
    fn decrypt_streamer_passes_long_header_ds2_through() {
        let mut decryptor = DecryptStreamer::new(None);
        let plain = decryptor.push(b"\x08ds2rest").unwrap();
        assert_eq!(plain, b"\x08ds2rest");
    }

    #[test]
    fn test_streaming_decoder_push_after_finish_errors() {
        let mut decoder = StreamingDecoder::new();
        let _ = decoder.finish().unwrap();

        let err = decoder.push(b"\x03ds2").unwrap_err();
        assert!(matches!(err, DecodeError::AlreadyFinished));
    }

    #[test]
    fn decrypt_streamer_passes_plain_ds2_through() {
        let mut decryptor = DecryptStreamer::new(None);
        let plain = decryptor.push(b"\x03ds2rest").unwrap();
        assert_eq!(plain, b"\x03ds2rest");
        assert!(decryptor.finish().unwrap().is_empty());
    }

    #[test]
    fn decrypt_streamer_passes_plain_dss_through() {
        let mut decryptor = DecryptStreamer::new(None);
        let plain = decryptor.push(b"\x02dssrest").unwrap();
        assert_eq!(plain, b"\x02dssrest");
        assert!(decryptor.finish().unwrap().is_empty());
    }

    #[test]
    fn decrypt_streamer_rejects_unsupported_prefix() {
        let mut decryptor = DecryptStreamer::new(None);
        let err = decryptor.push(b"nope").unwrap_err();
        assert!(matches!(err, DecodeError::UnsupportedFormat(_)));
    }

    #[test]
    fn decrypting_decoder_streamer_plain_ds2_decodes() {
        let data = make_ds2_header_only(6);
        let mut decoder = DecryptingDecoderStreamer::new(None);
        let samples = decoder.push(&data).unwrap();
        assert!(samples.is_empty());
        assert_eq!(decoder.format(), Some(AudioFormat::Ds2Qp));
        assert_eq!(decoder.native_rate(), Some(16000));
        // The fixture declares one frame in its block header, so finish()
        // decodes it (256 samples per QP frame)
        let tail = decoder.finish().unwrap();
        assert!(!tail.is_empty());
        assert_eq!(tail.len() % 256, 0);
    }

    #[test]
    fn decrypting_decoder_streamer_push_after_finish_errors() {
        let mut decoder = DecryptingDecoderStreamer::new(None);
        let _ = decoder.finish().unwrap();
        let err = decoder.push(b"\x03ds2").unwrap_err();
        assert!(matches!(err, DecodeError::AlreadyFinished));
    }
}
