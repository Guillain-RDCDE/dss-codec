/// DSS SP demuxer.
///
/// Walks the file block by block. Each block header gives the offset of its
/// first frame, the byte-swap parity of that frame and the number of frames
/// that start in the block. Frames are read through the block headers they
/// straddle. A frame that the file does not hold in full ends the stream.
use crate::error::{DecodeError, Result};

const DSS_BLOCK_SIZE: usize = 512;
const DSS_BLOCK_HEADER_SIZE: usize = 6;
const DSS_SP_FRAME_SIZE: usize = 42;
const DSS_SP_SHORT_FRAME_SIZE: usize = 9;
const DSS_SP_PACKET_BUF: usize = 48;

pub(crate) fn block_frames(header: &[u8]) -> usize {
    if header[2] != 0 || header[5] == 0xff {
        header[2] as usize
    } else {
        header[5] as usize
    }
}

pub fn demux_dss(data: &[u8]) -> Result<(Vec<Vec<u8>>, usize)> {
    if data.len() < 4 || data[1..4] != *b"dss" || (data[0] != 2 && data[0] != 3) {
        return Err(DecodeError::NotDss(std::path::PathBuf::from("<bytes>")));
    }

    let header_size = data[0] as usize * DSS_BLOCK_SIZE;
    if data.len() < header_size {
        return Err(DecodeError::Truncated("DSS header".to_string()));
    }
    let codec = crate::demux::ds2::detect_ds2_format_type(data, header_size);
    crate::demux::check_sp_codec(codec)?;
    let (frames, _cut) = dss_sp_frames(data, header_size, codec == 1);
    let total_frames = frames.len();
    Ok((frames, total_frames))
}

pub(crate) fn dss_sp_frames(
    data: &[u8],
    header_size: usize,
    variable_rate: bool,
) -> (Vec<Vec<u8>>, bool) {
    let mut walker = DssSpWalker::new(header_size, variable_rate);
    let src = DssBytes {
        data,
        base: 0,
        eof: true,
    };
    let mut frames = Vec::new();
    loop {
        match walker.step(&src) {
            DssStep::Frame(frame) => frames.push(frame),
            DssStep::Dropped => {}
            DssStep::End { cut } => return (frames, cut),
            DssStep::NeedMore => return (frames, false),
        }
    }
}

struct DssBytes<'a> {
    data: &'a [u8],
    base: usize,
    eof: bool,
}

enum Short {
    NeedMore,
    End,
}

impl DssBytes<'_> {
    fn get(&self, pos: usize, len: usize) -> std::result::Result<&[u8], Short> {
        let start = pos.checked_sub(self.base).ok_or(Short::End)?;
        if start + len <= self.data.len() {
            Ok(&self.data[start..start + len])
        } else if self.eof {
            Err(Short::End)
        } else {
            Err(Short::NeedMore)
        }
    }

    fn byte_or_zero(&self, pos: usize) -> std::result::Result<u8, Short> {
        match self.get(pos, 1) {
            Ok(bytes) => Ok(bytes[0]),
            Err(Short::End) => Ok(0),
            Err(Short::NeedMore) => Err(Short::NeedMore),
        }
    }
}

enum DssStep {
    Frame(Vec<u8>),
    Dropped,
    NeedMore,
    End { cut: bool },
}

#[derive(Clone, Copy)]
struct DssSpWalker {
    header_size: usize,
    variable_rate: bool,
    block_pos: usize,
    pos: usize,
    frames_left: usize,
    swap: bool,
    swap_byte: Option<u8>,
}

impl DssSpWalker {
    fn new(header_size: usize, variable_rate: bool) -> Self {
        Self {
            header_size,
            variable_rate,
            block_pos: header_size - DSS_BLOCK_SIZE,
            pos: header_size,
            frames_left: 0,
            swap: false,
            swap_byte: None,
        }
    }

    fn step(&mut self, src: &DssBytes) -> DssStep {
        let saved = *self;
        match self.read_frame(src) {
            Ok(step) => step,
            Err(_) => {
                *self = saved;
                DssStep::NeedMore
            }
        }
    }

    fn read_frame(&mut self, src: &DssBytes) -> std::result::Result<DssStep, Short> {
        if self.frames_left == 0 {
            match self.next_block(src) {
                Err(Short::End) => return Ok(DssStep::End { cut: false }),
                other => other?,
            }
        }

        let mut size = DSS_SP_FRAME_SIZE - 1;
        let mut done = 0;
        let mut pkt = [0u8; DSS_SP_PACKET_BUF];
        if self.variable_rate {
            if self.swap {
                if self.swap_byte.is_some_and(|byte| byte & 0x80 == 0) {
                    size = DSS_SP_SHORT_FRAME_SIZE;
                }
            } else {
                match self.read_payload(src, &mut pkt[..2]) {
                    Err(Short::End) => return Ok(DssStep::End { cut: true }),
                    other => other?,
                }
                done = 2;
                if pkt[1] & 0x80 == 0 {
                    size = DSS_SP_SHORT_FRAME_SIZE;
                }
            }
        }

        let read = if self.swap {
            self.read_payload(src, &mut pkt[3..3 + size - 1])
        } else {
            self.read_payload(src, &mut pkt[done..size + 1])
        };
        match read {
            Err(Short::End) => return Ok(DssStep::End { cut: true }),
            other => other?,
        }
        self.frames_left -= 1;

        self.byte_swap(&mut pkt, size);
        if self.swap_byte.is_none() {
            return Ok(DssStep::Dropped);
        }
        Ok(DssStep::Frame(pkt[..DSS_SP_FRAME_SIZE].to_vec()))
    }

    fn next_block(&mut self, src: &DssBytes) -> std::result::Result<(), Short> {
        loop {
            self.block_pos += DSS_BLOCK_SIZE;
            let header = src.get(self.block_pos, DSS_BLOCK_HEADER_SIZE)?;

            self.swap = header[0] & 0x80 != 0;
            let mut cont =
                2 * header[1] as isize + 2 * self.swap as isize - DSS_BLOCK_HEADER_SIZE as isize;
            if cont < 0 || cont > (DSS_BLOCK_SIZE - DSS_BLOCK_HEADER_SIZE) as isize {
                cont = 0;
            }
            let cont = cont as usize;

            self.frames_left = block_frames(header);
            if self.frames_left == 0 {
                continue;
            }

            if self.swap {
                let mut at = self.block_pos + cont - 2;
                if cont >= 2 {
                    at += DSS_BLOCK_HEADER_SIZE;
                }
                if at >= self.header_size + DSS_BLOCK_HEADER_SIZE {
                    self.swap_byte = Some(src.byte_or_zero(at)?);
                }
            }

            self.pos = self.block_pos + DSS_BLOCK_HEADER_SIZE + cont;
            return Ok(());
        }
    }

    fn read_payload(&mut self, src: &DssBytes, buf: &mut [u8]) -> std::result::Result<(), Short> {
        let mut done = 0;
        while done < buf.len() {
            let off = (self.pos - self.header_size) % DSS_BLOCK_SIZE;
            if off < DSS_BLOCK_HEADER_SIZE {
                self.pos += DSS_BLOCK_HEADER_SIZE - off;
                continue;
            }
            let chunk = (buf.len() - done).min(DSS_BLOCK_SIZE - off);
            let bytes = src.get(self.pos, chunk)?;
            buf[done..done + chunk].copy_from_slice(bytes);
            self.pos += chunk;
            done += chunk;
        }
        Ok(())
    }

    fn byte_swap(&mut self, pkt: &mut [u8; DSS_SP_PACKET_BUF], size: usize) {
        if self.swap {
            for i in (0..size - 1).step_by(2) {
                pkt[i] = pkt[i + 4];
            }
            pkt[1] = self.swap_byte.unwrap_or(0xff);
        } else {
            self.swap_byte = Some(pkt[size - 1]);
        }
        pkt[size + 1..DSS_SP_FRAME_SIZE + 1].fill(0);
        pkt[size - 1] = 0;
        self.swap = !self.swap;
    }
}

pub(crate) struct DssSpStreamDemuxer {
    walker: DssSpWalker,
    buf: Vec<u8>,
    base: usize,
}

impl DssSpStreamDemuxer {
    pub(crate) fn new(version: u8, variable_rate: bool) -> Self {
        Self::with_header_size(version as usize * DSS_BLOCK_SIZE, variable_rate)
    }

    pub(crate) fn with_header_size(header_size: usize, variable_rate: bool) -> Self {
        Self {
            walker: DssSpWalker::new(header_size, variable_rate),
            buf: Vec::new(),
            base: 0,
        }
    }

    pub(crate) fn push(&mut self, data: &[u8]) -> Result<Vec<Vec<u8>>> {
        self.buf.extend_from_slice(data);
        Ok(self.walk(false).0)
    }

    pub(crate) fn finish(&mut self) -> Result<Vec<Vec<u8>>> {
        self.check_header()?;
        let (frames, cut) = self.walk(true);
        let tail = (self.base + self.buf.len()).saturating_sub(self.walker.header_size);
        if cut && !tail.is_multiple_of(DSS_BLOCK_SIZE) {
            return Err(DecodeError::Truncated("DSS SP frame".to_string()));
        }
        Ok(frames)
    }

    pub(crate) fn finish_lenient(&mut self) -> Result<Vec<Vec<u8>>> {
        self.check_header()?;
        Ok(self.walk(true).0)
    }

    fn check_header(&self) -> Result<()> {
        let received = self.base + self.buf.len();
        if received > 0 && received < self.walker.header_size {
            return Err(DecodeError::Truncated("DSS header".to_string()));
        }
        Ok(())
    }

    fn walk(&mut self, eof: bool) -> (Vec<Vec<u8>>, bool) {
        let mut frames = Vec::new();
        let cut = loop {
            let src = DssBytes {
                data: &self.buf,
                base: self.base,
                eof,
            };
            match self.walker.step(&src) {
                DssStep::Frame(frame) => frames.push(frame),
                DssStep::Dropped => {}
                DssStep::NeedMore => break false,
                DssStep::End { cut } => break cut,
            }
        };

        let unneeded = self
            .walker
            .block_pos
            .saturating_sub(self.base)
            .min(self.buf.len());
        self.buf.drain(..unneeded);
        self.base += unneeded;

        (frames, cut)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_dss_block(
        swap: u8,
        byte1: u8,
        frame_count: u8,
        payload_pattern: u8,
    ) -> [u8; DSS_BLOCK_SIZE] {
        let mut block = [0u8; DSS_BLOCK_SIZE];
        block[0] = swap << 7;
        block[1] = byte1;
        block[2] = frame_count;
        for (i, byte) in block[DSS_BLOCK_HEADER_SIZE..].iter_mut().enumerate() {
            *byte = payload_pattern.wrapping_add(i as u8);
        }
        block
    }

    fn make_dss_file(blocks: &[[u8; DSS_BLOCK_SIZE]]) -> Vec<u8> {
        let mut data = vec![0u8; 2 * DSS_BLOCK_SIZE];
        data[0] = 2;
        data[1..4].copy_from_slice(b"dss");
        for block in blocks {
            data.extend_from_slice(block);
        }
        data
    }

    fn pause_block(byte1: u8, byte5: u8, payload_pattern: u8) -> [u8; DSS_BLOCK_SIZE] {
        let mut block = make_dss_block(0, byte1, 0, payload_pattern);
        block[5] = byte5;
        block
    }

    fn collect_frames(
        demuxer: &mut DssSpStreamDemuxer,
        data: &[u8],
        chunk_size: usize,
    ) -> Vec<Vec<u8>> {
        let mut frames = Vec::new();
        for chunk in data.chunks(chunk_size) {
            frames.extend(demuxer.push(chunk).unwrap());
        }
        frames.extend(demuxer.finish().unwrap());
        frames
    }

    #[test]
    fn test_dss_stream_demux_matches_batch() {
        let data = make_dss_file(&[make_dss_block(0, 0, 3, 0x20)]);

        let (expected, _) = demux_dss(&data).unwrap();
        let mut demuxer = DssSpStreamDemuxer::new(2, false);
        let actual = collect_frames(&mut demuxer, &data, 149);

        assert_eq!(actual, expected);
    }

    #[test]
    fn test_dss_stream_demux_empty_block_reset_matches_batch() {
        let data = make_dss_file(&[
            make_dss_block(0, 0, 13, 0x10),
            make_dss_block(1, 17, 0, 0x80),
            make_dss_block(1, 0, 1, 0xC0),
        ]);

        let (expected, _) = demux_dss(&data).unwrap();
        let mut demuxer = DssSpStreamDemuxer::new(2, false);
        let actual = collect_frames(&mut demuxer, &data, 127);

        assert_eq!(actual, expected);
    }

    #[test]
    fn test_dss_demux_frame_across_block_header() {
        let data = make_dss_file(&[
            make_dss_block(0, 3, 13, 0x10),
            make_dss_block(1, 16, 1, 0x80),
        ]);

        let (frames, total) = demux_dss(&data).unwrap();
        assert_eq!(total, 14);
        assert_eq!(frames.len(), 14);
        let last_whole = &frames[12];
        assert_eq!(last_whole[0], data[2 * DSS_BLOCK_SIZE + DSS_BLOCK_HEADER_SIZE + 492]);
        assert_eq!(last_whole[13], data[2 * DSS_BLOCK_SIZE + DSS_BLOCK_HEADER_SIZE + 505]);
        assert_eq!(last_whole[14], data[3 * DSS_BLOCK_SIZE + DSS_BLOCK_HEADER_SIZE]);
        assert_eq!(frames[13][1], data[3 * DSS_BLOCK_SIZE + DSS_BLOCK_HEADER_SIZE + 26]);

        let mut demuxer = DssSpStreamDemuxer::new(2, false);
        assert_eq!(collect_frames(&mut demuxer, &data, 61), frames);
    }

    #[test]
    fn test_dss_demux_reads_swap_byte_from_file() {
        let data = make_dss_file(&[
            make_dss_block(0, 3, 1, 0x10),
            make_dss_block(1, 20, 1, 0x80),
        ]);

        let (frames, _) = demux_dss(&data).unwrap();
        assert_eq!(frames.len(), 2);
        let carried = data[2 * DSS_BLOCK_SIZE + DSS_BLOCK_HEADER_SIZE + 40];
        let in_file = data[3 * DSS_BLOCK_SIZE + DSS_BLOCK_HEADER_SIZE + 34];
        assert_ne!(carried, in_file);
        assert_eq!(frames[1][1], in_file);
        assert_eq!(frames[1][40], 0);

        let mut demuxer = DssSpStreamDemuxer::new(2, false);
        assert_eq!(collect_frames(&mut demuxer, &data, 7), frames);
    }

    #[test]
    fn test_dss_demux_drops_swapped_frame_without_swap_byte() {
        let data = make_dss_file(&[make_dss_block(1, 2, 3, 0x40)]);

        let (frames, total) = demux_dss(&data).unwrap();
        assert_eq!(total, 2);
        assert_eq!(frames.len(), 2);
        let payload = &data[2 * DSS_BLOCK_SIZE + DSS_BLOCK_HEADER_SIZE..];
        assert_eq!(frames[0][..40], payload[40..80]);
        assert_eq!(frames[1][1], payload[80]);

        let mut demuxer = DssSpStreamDemuxer::new(2, false);
        assert_eq!(collect_frames(&mut demuxer, &data, 33), frames);
    }

    #[test]
    fn test_dss_demux_partial_last_block() {
        let whole = make_dss_file(&[make_dss_block(0, 3, 3, 0x20)]);
        let end = 2 * DSS_BLOCK_SIZE + DSS_BLOCK_HEADER_SIZE + 124;

        let (frames, _) = demux_dss(&whole[..end]).unwrap();
        let (expected, _) = demux_dss(&whole).unwrap();
        assert_eq!(frames, expected);
        assert_eq!(frames.len(), 3);

        let mut demuxer = DssSpStreamDemuxer::new(2, false);
        assert_eq!(collect_frames(&mut demuxer, &whole[..end], 50), frames);
    }

    #[test]
    fn test_dss_demux_cut_block_header_ends_cleanly() {
        let mut data = make_dss_file(&[make_dss_block(0, 3, 3, 0x20)]);
        data.extend_from_slice(&[0x00, 0x03, 0x05]);

        let (frames, _) = demux_dss(&data).unwrap();
        assert_eq!(frames.len(), 3);

        let mut demuxer = DssSpStreamDemuxer::new(2, false);
        assert_eq!(collect_frames(&mut demuxer, &data, 100).len(), 3);
    }

    #[test]
    fn test_dss_demux_cut_frame_is_dropped() {
        let whole = make_dss_file(&[make_dss_block(0, 3, 3, 0x20)]);
        let end = 2 * DSS_BLOCK_SIZE + DSS_BLOCK_HEADER_SIZE + 123;

        let (frames, total) = demux_dss(&whole[..end]).unwrap();
        assert_eq!(total, 2);
        assert_eq!(frames.len(), 2);

        let mut demuxer = DssSpStreamDemuxer::new(2, false);
        let _ = demuxer.push(&whole[..end]).unwrap();
        let err = demuxer.finish().unwrap_err();
        assert!(matches!(err, DecodeError::Truncated(_)));

        let mut demuxer = DssSpStreamDemuxer::new(2, false);
        let mut lenient = demuxer.push(&whole[..end]).unwrap();
        lenient.extend(demuxer.finish_lenient().unwrap());
        assert_eq!(lenient, frames);
    }

    #[test]
    fn test_dss_demux_overdeclared_frames_stop_at_end_of_file() {
        let data = make_dss_file(&[make_dss_block(0, 3, 14, 0x20)]);

        let (frames, total) = demux_dss(&data).unwrap();
        assert_eq!(total, 12);
        assert_eq!(frames.len(), 12);
        assert!(frames.iter().all(|f| f.len() == DSS_SP_FRAME_SIZE));
    }

    #[test]
    fn test_dss_demux_pause_block_frames_from_byte5() {
        let data = make_dss_file(&[pause_block(3, 3, 0x20)]);
        let mut counted = data.clone();
        counted[2 * DSS_BLOCK_SIZE + 2] = 3;

        let (frames, total) = demux_dss(&data).unwrap();
        assert_eq!(total, 3);
        assert_eq!(frames[0][..40], data[2 * DSS_BLOCK_SIZE + DSS_BLOCK_HEADER_SIZE..][..40]);
        assert_eq!(frames, demux_dss(&counted).unwrap().0);
    }

    #[test]
    fn test_dss_stream_demux_pause_block_matches_batch() {
        let data = make_dss_file(&[
            make_dss_block(0, 3, 11, 0x10),
            pause_block(3, 2, 0x50),
            make_dss_block(0, 3, 1, 0x90),
        ]);

        let (expected, total) = demux_dss(&data).unwrap();
        assert_eq!(total, 14);
        assert_eq!(expected[11][..40], data[3 * DSS_BLOCK_SIZE + DSS_BLOCK_HEADER_SIZE..][..40]);
        let mut demuxer = DssSpStreamDemuxer::new(2, false);
        assert_eq!(collect_frames(&mut demuxer, &data, 37), expected);
    }

    #[test]
    fn test_dss_demux_byte5_ff_leaves_block_empty() {
        let data = make_dss_file(&[pause_block(3, 0xff, 0x20), make_dss_block(0, 3, 2, 0x60)]);

        let (frames, total) = demux_dss(&data).unwrap();
        assert_eq!(total, 2);
        assert_eq!(frames[0][..40], data[3 * DSS_BLOCK_SIZE + DSS_BLOCK_HEADER_SIZE..][..40]);
    }

    #[test]
    fn test_dss_demux_byte2_overrides_byte5() {
        let mut block = make_dss_block(0, 3, 1, 0x20);
        block[5] = 7;
        let data = make_dss_file(&[block]);

        let (frames, total) = demux_dss(&data).unwrap();
        assert_eq!(total, 1);
        assert_eq!(frames.len(), 1);
    }

    #[test]
    fn test_dss_demux_pause_block_count_past_end_of_file() {
        let data = make_dss_file(&[pause_block(3, 14, 0x20)]);

        let (frames, total) = demux_dss(&data).unwrap();
        assert_eq!(total, 12);

        let mut demuxer = DssSpStreamDemuxer::new(2, false);
        assert_eq!(collect_frames(&mut demuxer, &data, 37), frames);

        let end = data.len() - 100;
        let mut demuxer = DssSpStreamDemuxer::new(2, false);
        let _ = demuxer.push(&data[..end]).unwrap();
        assert!(matches!(demuxer.finish(), Err(DecodeError::Truncated(_))));

        let mut demuxer = DssSpStreamDemuxer::new(2, false);
        let mut lenient = demuxer.push(&data[..end]).unwrap();
        lenient.extend(demuxer.finish_lenient().unwrap());
        assert_eq!(lenient, demux_dss(&data[..end]).unwrap().0);
    }

    #[test]
    fn test_dss_stream_demux_truncated_frame() {
        let data = make_dss_file(&[make_dss_block(0, 0, 20, 0x30)]);

        let mut demuxer = DssSpStreamDemuxer::new(2, false);
        for chunk in data[..data.len() - 100].chunks(211) {
            let _ = demuxer.push(chunk).unwrap();
        }

        let err = demuxer.finish().unwrap_err();
        assert!(matches!(err, DecodeError::Truncated(_)));
    }

    #[test]
    fn test_dss_stream_demux_frame_past_block_boundary_is_complete() {
        let whole = make_dss_file(&[
            make_dss_block(0, 3, 13, 0x10),
            make_dss_block(1, 16, 1, 0x80),
        ]);
        let data = &whole[..3 * DSS_BLOCK_SIZE];

        let (frames, _) = demux_dss(data).unwrap();
        assert_eq!(frames.len(), 12);

        let mut demuxer = DssSpStreamDemuxer::new(2, false);
        assert_eq!(collect_frames(&mut demuxer, data, 70), frames);
    }

    #[test]
    fn test_dss_stream_demux_frame_into_cut_block_header_fails() {
        let whole = make_dss_file(&[
            make_dss_block(0, 3, 13, 0x10),
            make_dss_block(1, 16, 1, 0x80),
        ]);
        for cut in [1, 5, DSS_BLOCK_HEADER_SIZE] {
            let data = &whole[..3 * DSS_BLOCK_SIZE + cut];
            let (frames, _) = demux_dss(data).unwrap();
            assert_eq!(frames.len(), 12);

            let mut demuxer = DssSpStreamDemuxer::new(2, false);
            let _ = demuxer.push(data).unwrap();
            assert!(matches!(demuxer.finish(), Err(DecodeError::Truncated(_))));

            let mut demuxer = DssSpStreamDemuxer::new(2, false);
            let mut lenient = demuxer.push(data).unwrap();
            lenient.extend(demuxer.finish_lenient().unwrap());
            assert_eq!(lenient, frames);
        }
    }

    #[test]
    fn test_dss_stream_demux_cut_before_stated_frame_fails() {
        let whole = make_dss_file(&[make_dss_block(0, 3, 5, 0x20)]);
        let end = 2 * DSS_BLOCK_SIZE + DSS_BLOCK_HEADER_SIZE + 124;
        let (frames, _) = demux_dss(&whole[..end]).unwrap();
        assert_eq!(frames.len(), 3);

        let mut demuxer = DssSpStreamDemuxer::new(2, false);
        let _ = demuxer.push(&whole[..end]).unwrap();
        assert!(matches!(demuxer.finish(), Err(DecodeError::Truncated(_))));

        let mut demuxer = DssSpStreamDemuxer::new(2, false);
        let mut lenient = demuxer.push(&whole[..end]).unwrap();
        lenient.extend(demuxer.finish_lenient().unwrap());
        assert_eq!(lenient, frames);
    }

    #[test]
    fn test_dss_stream_demux_truncated_header() {
        let data = make_dss_file(&[]);

        let mut demuxer = DssSpStreamDemuxer::new(2, false);
        let _ = demuxer.push(&data[..100]).unwrap();
        assert!(matches!(demuxer.finish(), Err(DecodeError::Truncated(_))));

        let mut demuxer = DssSpStreamDemuxer::new(2, false);
        let _ = demuxer.push(&data[..100]).unwrap();
        assert!(matches!(demuxer.finish_lenient(), Err(DecodeError::Truncated(_))));
    }

    #[test]
    fn test_dss_demux_truncated_header() {
        let data = make_dss_file(&[]);
        for cut in [4, 100, data.len() - 1] {
            assert!(matches!(
                demux_dss(&data[..cut]),
                Err(DecodeError::Truncated(what)) if what == "DSS header"
            ));
            assert!(matches!(
                crate::decode_to_buffer(&data[..cut]),
                Err(DecodeError::Truncated(what)) if what == "DSS header"
            ));
        }
        let (frames, total) = demux_dss(&data).unwrap();
        assert!(frames.is_empty());
        assert_eq!(total, 0);
    }

    #[test]
    fn test_dss_demux_refuses_lp_and_unknown_codecs() {
        for codec in [2u8, 3, 0xff] {
            let mut block = make_dss_block(0, 3, 3, 0x20);
            block[4] = codec;
            assert!(matches!(
                demux_dss(&make_dss_file(&[block])),
                Err(DecodeError::UnsupportedCodec(c)) if c == codec
            ));
        }
        let data = make_dss_file(&[make_dss_block(0, 3, 3, 0x20)]);
        assert_eq!(demux_dss(&data).unwrap().1, 3);
    }

    #[test]
    fn test_dss_demux_ignores_header_codec_byte() {
        let data = make_dss_file(&[make_dss_block(0, 3, 3, 0x20)]);
        let (expected, _) = demux_dss(&data).unwrap();
        for byte in [1u8, 2, 0xff] {
            let mut marked = data.clone();
            marked[0x2a4] = byte;
            let (frames, total) = demux_dss(&marked).unwrap();
            assert_eq!(total, 3);
            assert_eq!(frames, expected);
        }
    }

    #[test]
    fn test_dss_demux_takes_codec_from_first_block_with_frames() {
        let mut empty = make_dss_block(0, 3, 0, 0x10);
        empty[4] = 2;
        let data = make_dss_file(&[empty, make_dss_block(0, 3, 3, 0x20)]);
        let (frames, total) = demux_dss(&data).unwrap();
        assert_eq!(total, 3);
        let (expected, _) = demux_dss(&make_dss_file(&[make_dss_block(0, 3, 3, 0x20)])).unwrap();
        assert_eq!(frames, expected);
    }

    #[test]
    fn test_dss_demux_refuses_lp_after_empty_block() {
        let mut lp = make_dss_block(0, 3, 3, 0x20);
        lp[4] = 2;
        let data = make_dss_file(&[make_dss_block(0, 3, 0, 0x10), lp]);
        assert!(matches!(demux_dss(&data), Err(DecodeError::UnsupportedCodec(2))));
    }

    #[test]
    fn test_dss_demux_refuses_lp_in_partial_first_block() {
        let mut lp = make_dss_block(0, 3, 3, 0x20);
        lp[4] = 2;
        let data = make_dss_file(&[lp]);
        for end in [2 * DSS_BLOCK_SIZE + DSS_BLOCK_HEADER_SIZE, 2 * DSS_BLOCK_SIZE + 100] {
            assert!(matches!(
                demux_dss(&data[..end]),
                Err(DecodeError::UnsupportedCodec(2))
            ));
        }
    }

    #[test]
    fn test_dss_demux_without_frames_is_empty() {
        let mut empty = make_dss_block(0, 3, 0, 0x10);
        empty[4] = 2;
        let (frames, total) = demux_dss(&make_dss_file(&[empty, empty])).unwrap();
        assert!(frames.is_empty());
        assert_eq!(total, 0);

        let mut lp = make_dss_block(0, 3, 3, 0x20);
        lp[4] = 2;
        let data = make_dss_file(&[lp]);
        let (frames, total) = demux_dss(&data[..2 * DSS_BLOCK_SIZE + 5]).unwrap();
        assert!(frames.is_empty());
        assert_eq!(total, 0);
    }

    #[test]
    fn test_dss_demux_rejects_other_magic() {
        let mut data = make_dss_file(&[make_dss_block(0, 3, 1, 0x20)]);
        data[0] = 6;
        assert!(matches!(demux_dss(&data), Err(DecodeError::NotDss(_))));
        assert!(matches!(demux_dss(b"\x02ds"), Err(DecodeError::NotDss(_))));
    }

    const PAYLOAD: usize = 2 * DSS_BLOCK_SIZE + DSS_BLOCK_HEADER_SIZE;

    fn vr_payload() -> Vec<u8> {
        let mut payload: Vec<u8> = (1..=100).collect();
        payload[19] |= 0x80;
        payload[58] |= 0x80;
        payload
    }

    fn vr_file(payload: &[u8], swap: u8, byte1: u8, frame_count: u8, codec: u8) -> Vec<u8> {
        let mut block = [0u8; DSS_BLOCK_SIZE];
        block[0] = swap << 7;
        block[1] = byte1;
        block[2] = frame_count;
        block[4] = codec;
        block[DSS_BLOCK_HEADER_SIZE..DSS_BLOCK_HEADER_SIZE + payload.len()].copy_from_slice(payload);
        make_dss_file(&[block])
    }

    #[test]
    fn test_dss_variable_rate_frame_sizes() {
        let payload = vr_payload();
        let data = vr_file(&payload, 0, 3, 4, 1);

        let (frames, total) = demux_dss(&data).unwrap();
        assert_eq!(total, 4);
        assert!(frames.iter().all(|f| f.len() == DSS_SP_FRAME_SIZE));

        assert_eq!(frames[0][..8], payload[..8]);
        assert_eq!(frames[0][8], 0);
        assert_eq!(frames[0][9], payload[9]);
        assert!(frames[0][10..].iter().all(|&b| b == 0));

        assert_eq!(frames[1][0], payload[11]);
        assert_eq!(frames[1][1], payload[8]);
        assert_eq!(frames[1][3], payload[10]);
        assert_eq!(frames[1][8], 0);
        assert!(frames[1][10..].iter().all(|&b| b == 0));

        assert_eq!(frames[2][..40], payload[18..58]);
        assert_eq!(frames[2][40], 0);
        assert_eq!(frames[2][41], payload[59]);

        assert_eq!(frames[3][0], payload[61]);
        assert_eq!(frames[3][1], payload[58]);
        assert_eq!(frames[3][3], payload[60]);

        for chunk in [1, 7, 100] {
            let mut demuxer = DssSpStreamDemuxer::new(2, true);
            assert_eq!(collect_frames(&mut demuxer, &data, chunk), frames);
        }
    }

    #[test]
    fn test_dss_codec_byte_selects_variable_rate_walk() {
        let payload = vr_payload();
        let (fixed, _) = demux_dss(&vr_file(&payload, 0, 3, 4, 0)).unwrap();
        let (variable, _) = demux_dss(&vr_file(&payload, 0, 3, 4, 1)).unwrap();
        assert_eq!(fixed.len(), 4);
        assert_eq!(fixed[0][..40], payload[..40]);
        assert_ne!(fixed, variable);
    }

    #[test]
    fn test_dss_variable_rate_cut_frame_is_dropped() {
        let data = vr_file(&vr_payload(), 0, 3, 4, 1);
        for (cut, whole) in [(1, 0), (5, 0), (15, 1), (19, 2), (50, 2), (90, 3)] {
            let end = PAYLOAD + cut;
            let (frames, total) = demux_dss(&data[..end]).unwrap();
            assert_eq!(total, whole, "cut at {cut}");

            let mut demuxer = DssSpStreamDemuxer::new(2, true);
            let _ = demuxer.push(&data[..end]).unwrap();
            assert!(matches!(demuxer.finish(), Err(DecodeError::Truncated(_))));

            let mut demuxer = DssSpStreamDemuxer::new(2, true);
            let mut lenient = demuxer.push(&data[..end]).unwrap();
            lenient.extend(demuxer.finish_lenient().unwrap());
            assert_eq!(lenient, frames, "cut at {cut}");
        }
    }

    #[test]
    fn test_dss_variable_rate_unknown_swap_byte_reads_long_frame() {
        let payload: Vec<u8> = (1..=60).collect();
        let data = vr_file(&payload, 1, 2, 2, 1);

        let (frames, total) = demux_dss(&data).unwrap();
        assert_eq!(total, 1);
        assert_eq!(frames[0][..8], payload[40..48]);
        assert_eq!(frames[0][9], payload[49]);
        assert!(frames[0][10..].iter().all(|&b| b == 0));

        let mut demuxer = DssSpStreamDemuxer::new(2, true);
        assert_eq!(collect_frames(&mut demuxer, &data, 3), frames);
    }
}
