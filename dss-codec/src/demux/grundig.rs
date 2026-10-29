//! Grundig DSS demuxer.
//!
//! Grundig `\x06dss` files store the CELP bitstream in 512-byte blocks. Each
//! block begins with a 6-byte header `[b0][b1][b2] ff 00 ff`; the remaining 506
//! bytes are payload, stored as little-endian 16-bit words whose bits are read
//! MSB-first. `b2` is the number of frames that *start* within the block. The
//! first frame starts `b1*16 + (b0>>4)` bits from the start of the block. The
//! demuxer takes off the 48 header bits to get the offset into the payload,
//! and an offset that lands inside the header counts as 0. Frames are a
//! continuous 328-bit (41-byte) stream; a frame may spill into the next block,
//! but each block's frames always start at that block's own first-frame offset.
//!
//! The whole audio region is buffered and demuxed at `finish`, because frame
//! extraction needs each block's `frame_count` and the cross-block bit reads.

use crate::error::{DecodeError, Result};

const BLOCK_SIZE: usize = 512;
const HEADER_SIZE: usize = 6;
const FRAME_BITS: usize = 328;

/// Read a single bit (MSB-first) from a payload stored as little-endian
/// 16-bit words. Out-of-range reads return 0, matching the reference.
#[inline]
fn read_bit(payload: &[u8], bit_index: usize) -> u8 {
    let wi = bit_index / 16;
    let bit = bit_index % 16;
    if wi * 2 + 1 >= payload.len() {
        0
    } else {
        let w = ((payload[wi * 2 + 1] as u16) << 8) | (payload[wi * 2] as u16);
        ((w >> (15 - bit)) & 1) as u8
    }
}

/// Demux a Grundig audio region (after the header blocks) into 41-byte frames.
pub fn demux_grundig(audio: &[u8]) -> (Vec<Vec<u8>>, usize) {
    let (frames, _cut) = grundig_frames(audio);
    let total = frames.len();
    (frames, total)
}

fn grundig_frames(audio: &[u8]) -> (Vec<Vec<u8>>, bool) {
    let mut payload = Vec::with_capacity(audio.len());
    let mut blocks = Vec::new();
    for blk in audio.chunks(BLOCK_SIZE) {
        if blk.len() < HEADER_SIZE {
            break;
        }
        let first = (blk[1] as usize * 16 + (blk[0] >> 4) as usize).saturating_sub(HEADER_SIZE * 8);
        blocks.push((payload.len() * 8 + first, blk[2] as usize));
        payload.extend_from_slice(&blk[HEADER_SIZE..]);
    }
    let payload_bits = payload.len() / 2 * 16;

    let mut frames = Vec::new();
    for (first, count) in blocks {
        for pos in (first..).step_by(FRAME_BITS).take(count) {
            if pos + FRAME_BITS > payload_bits {
                return (frames, true);
            }
            let bits: Vec<u8> = (pos..pos + FRAME_BITS)
                .map(|i| read_bit(&payload, i))
                .collect();
            frames.push(pack_frame(&bits));
        }
    }
    (frames, false)
}

/// Pack 328 MSB-first bits into a 41-byte frame.
fn pack_frame(bits: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; 41];
    for (i, &bit) in bits.iter().enumerate().take(FRAME_BITS) {
        if bit != 0 {
            out[i / 8] |= 1 << (7 - (i % 8));
        }
    }
    out
}

/// Streaming wrapper: buffers the whole file and demuxes at `finish`.
pub(crate) struct GrundigSpStreamDemuxer {
    header_size: usize,
    buf: Vec<u8>,
}

impl GrundigSpStreamDemuxer {
    /// `header_blocks` = first byte of the file (number of 512-byte header blocks).
    pub(crate) fn new(header_blocks: u8) -> Self {
        Self {
            header_size: header_blocks as usize * BLOCK_SIZE,
            buf: Vec::new(),
        }
    }

    pub(crate) fn push(&mut self, data: &[u8]) -> Result<Vec<Vec<u8>>> {
        self.buf.extend_from_slice(data);
        Ok(Vec::new())
    }

    pub(crate) fn finish(&mut self) -> Result<Vec<Vec<u8>>> {
        self.check_header()?;
        let (frames, cut) = self.demux_all();
        let tail = self.buf.len().saturating_sub(self.header_size);
        if cut && !tail.is_multiple_of(BLOCK_SIZE) {
            return Err(DecodeError::Truncated("Grundig SP frame".to_string()));
        }
        Ok(frames)
    }

    pub(crate) fn finish_lenient(&mut self) -> Result<Vec<Vec<u8>>> {
        self.check_header()?;
        Ok(self.demux_all().0)
    }

    fn check_header(&self) -> Result<()> {
        if !self.buf.is_empty() && self.buf.len() < self.header_size {
            return Err(DecodeError::Truncated("Grundig header".to_string()));
        }
        Ok(())
    }

    fn demux_all(&mut self) -> (Vec<Vec<u8>>, bool) {
        if self.buf.len() <= self.header_size {
            return (Vec::new(), false);
        }
        grundig_frames(&self.buf[self.header_size..])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(b0: u8, b1: u8, count: u8, seed: u8) -> Vec<u8> {
        let mut blk = vec![0u8; BLOCK_SIZE];
        blk[..HEADER_SIZE].copy_from_slice(&[b0, b1, count, 0xff, 0x00, 0xff]);
        for (i, byte) in blk[HEADER_SIZE..].iter_mut().enumerate() {
            *byte = (i as u8).wrapping_mul(37).wrapping_add(seed);
        }
        blk
    }

    fn payload_bits(audio: &[u8]) -> Vec<u8> {
        let mut bits = Vec::new();
        for blk in audio.chunks(BLOCK_SIZE) {
            for word in blk[HEADER_SIZE..].chunks_exact(2) {
                let word = u16::from_le_bytes([word[0], word[1]]);
                bits.extend((0..16).rev().map(|k| ((word >> k) & 1) as u8));
            }
        }
        bits
    }

    fn frame_at(bits: &[u8], start: usize) -> Vec<u8> {
        let mut frame = vec![0u8; 41];
        for (i, &bit) in bits[start..start + FRAME_BITS].iter().enumerate() {
            frame[i / 8] |= bit << (7 - i % 8);
        }
        frame
    }

    fn frames_from(bits: &[u8], count: usize) -> Vec<Vec<u8>> {
        (0..count).map(|k| frame_at(bits, k * FRAME_BITS)).collect()
    }

    fn grundig_file(audio: &[u8]) -> Vec<u8> {
        let mut file = vec![0u8; 6 * BLOCK_SIZE];
        file[..4].copy_from_slice(b"\x06dss");
        file.extend_from_slice(audio);
        file
    }

    fn stream_finish(file: &[u8], strict: bool) -> Result<Vec<Vec<u8>>> {
        let mut demuxer = GrundigSpStreamDemuxer::new(6);
        for piece in file.chunks(37) {
            assert!(demuxer.push(piece).unwrap().is_empty());
        }
        if strict {
            demuxer.finish()
        } else {
            demuxer.finish_lenient()
        }
    }

    #[test]
    fn test_grundig_frames_continue_into_next_block() {
        let data = [block(0x00, 255, 2, 1), block(0x00, 53, 1, 2)].concat();
        let bits = payload_bits(&data);

        let (frames, total) = demux_grundig(&data);
        assert_eq!(total, 3);
        assert_eq!(
            frames,
            vec![
                frame_at(&bits, 4032),
                frame_at(&bits, 4360),
                frame_at(&bits, 4048 + 800)
            ]
        );
    }

    #[test]
    fn test_grundig_block_frames_start_at_own_offset() {
        let data = [block(0x00, 3, 1, 1), block(0x40, 13, 2, 2)].concat();
        let bits = payload_bits(&data);

        let (frames, total) = demux_grundig(&data);
        assert_eq!(total, 3);
        assert_eq!(
            frames,
            vec![
                frame_at(&bits, 0),
                frame_at(&bits, 4048 + 164),
                frame_at(&bits, 4048 + 492)
            ]
        );
    }

    #[test]
    fn test_grundig_partial_last_block_gives_its_frames() {
        let whole = [block(0x00, 3, 13, 1), block(0x80, 16, 1, 2)].concat();
        let bits = payload_bits(&whole);

        let (frames, total) = demux_grundig(&whole[..BLOCK_SIZE + HEADER_SIZE + 70]);
        assert_eq!(total, 14);
        let mut expected = frames_from(&bits, 13);
        expected.push(frame_at(&bits, 4048 + 216));
        assert_eq!(frames, expected);
    }

    #[test]
    fn test_grundig_stream_demux_matches_batch() {
        let audio = [
            block(0x0f, 3, 12, 1),
            block(0x80, 16, 12, 2),
            block(0x80, 9, 5, 3),
        ]
        .concat();
        let audio = &audio[..2 * BLOCK_SIZE + 300];
        let mut file = vec![0u8; 6 * BLOCK_SIZE];
        file[..4].copy_from_slice(b"\x06dss");
        file.extend_from_slice(audio);

        let (expected, total) = demux_grundig(audio);
        assert_eq!(total, 29);
        let mut strict = GrundigSpStreamDemuxer::new(6);
        let mut lenient = GrundigSpStreamDemuxer::new(6);
        for piece in file.chunks(37) {
            assert!(strict.push(piece).unwrap().is_empty());
            assert!(lenient.push(piece).unwrap().is_empty());
        }
        assert_eq!(strict.finish().unwrap(), expected);
        assert_eq!(lenient.finish_lenient().unwrap(), expected);
    }

    #[test]
    fn test_grundig_strict_finish_reports_cut_frame() {
        let long = [block(0x00, 3, 13, 1), block(0x80, 16, 1, 2)].concat();
        let short = [block(0x00, 3, 2, 1), block(0x00, 3, 2, 2)].concat();
        for audio in [
            &long[..BLOCK_SIZE + HEADER_SIZE + 20],
            &short[..BLOCK_SIZE + HEADER_SIZE],
        ] {
            let file = grundig_file(audio);
            assert!(matches!(
                stream_finish(&file, true),
                Err(DecodeError::Truncated(what)) if what == "Grundig SP frame"
            ));
            let lenient = stream_finish(&file, false).unwrap();
            assert!(!lenient.is_empty());
            assert_eq!(lenient, demux_grundig(audio).0);
        }
    }

    #[test]
    fn test_grundig_strict_finish_reports_frame_into_cut_block_header() {
        let long = [block(0x00, 3, 13, 1), block(0x80, 16, 1, 2)].concat();
        let bits = payload_bits(&long);
        for cut in [1, 3, HEADER_SIZE] {
            let audio = &long[..BLOCK_SIZE + cut];
            let file = grundig_file(audio);
            assert!(matches!(
                stream_finish(&file, true),
                Err(DecodeError::Truncated(what)) if what == "Grundig SP frame"
            ));
            assert_eq!(stream_finish(&file, false).unwrap(), frames_from(&bits, 12));
            assert_eq!(demux_grundig(audio).0, frames_from(&bits, 12));
        }
    }

    #[test]
    fn test_grundig_strict_finish_accepts_block_boundary() {
        let long = [block(0x00, 3, 13, 1), block(0x80, 16, 1, 2)].concat();
        let overstated = block(0x00, 3, 255, 1);
        for audio in [&long[..BLOCK_SIZE], &overstated[..]] {
            let file = grundig_file(audio);
            let strict = stream_finish(&file, true).unwrap();
            assert_eq!(strict.len(), 12);
            assert_eq!(strict, stream_finish(&file, false).unwrap());
            assert_eq!(strict, demux_grundig(audio).0);
        }
    }

    #[test]
    fn test_grundig_finish_reports_cut_header() {
        let file = grundig_file(&block(0x00, 3, 2, 1));
        for strict in [true, false] {
            for cut in [1, 1000, 6 * BLOCK_SIZE - 1] {
                assert!(matches!(
                    stream_finish(&file[..cut], strict),
                    Err(DecodeError::Truncated(what)) if what == "Grundig header"
                ));
            }
            assert!(stream_finish(&file[..6 * BLOCK_SIZE], strict).unwrap().is_empty());
        }
    }

    #[test]
    fn test_grundig_strict_finish_accepts_cut_block_header() {
        let whole = [block(0x00, 3, 2, 1), block(0x00, 3, 2, 2)].concat();
        let bits = payload_bits(&whole);

        for cut in [BLOCK_SIZE + 1, BLOCK_SIZE + HEADER_SIZE - 1] {
            let file = grundig_file(&whole[..cut]);
            assert_eq!(stream_finish(&file, true).unwrap(), frames_from(&bits, 2));
            assert_eq!(stream_finish(&file, false).unwrap(), frames_from(&bits, 2));
        }
    }

    #[test]
    fn test_grundig_offset_into_header_starts_at_payload() {
        for (b0, b1) in [(0xf0, 2), (0x00, 0), (0xf0, 1)] {
            let data = block(b0, b1, 2, 1);
            let bits = payload_bits(&data);

            let (frames, total) = demux_grundig(&data);
            assert_eq!(total, 2);
            assert_eq!(frames, frames_from(&bits, 2));
        }
    }

    #[test]
    fn test_grundig_drops_frame_cut_by_end_of_file() {
        let whole = [block(0x00, 3, 13, 1), block(0x80, 16, 1, 2)].concat();
        let bits = payload_bits(&whole);

        let (frames, total) = demux_grundig(&whole[..BLOCK_SIZE + HEADER_SIZE + 20]);
        assert_eq!(total, 12);
        assert_eq!(frames, frames_from(&bits, 12));
    }

    #[test]
    fn test_grundig_overstated_count_stops_at_end_of_file() {
        let data = block(0x00, 3, 255, 1);
        let bits = payload_bits(&data);

        let (frames, total) = demux_grundig(&data);
        assert_eq!(total, 12);
        assert_eq!(frames, frames_from(&bits, 12));
    }

    #[test]
    fn test_grundig_overstated_count_reads_into_next_block() {
        let data = [block(0x00, 3, 14, 1), block(0x80, 16, 1, 2)].concat();
        let bits = payload_bits(&data);

        let (frames, total) = demux_grundig(&data);
        assert_eq!(total, 15);
        let mut expected = frames_from(&bits, 14);
        expected.push(frame_at(&bits, 4048 + 216));
        assert_eq!(frames, expected);
    }

    #[test]
    fn test_grundig_file_cut_inside_block_header() {
        let whole = [block(0x00, 3, 2, 1), block(0x00, 3, 2, 2)].concat();
        let bits = payload_bits(&whole);

        for cut in [1, HEADER_SIZE - 1, HEADER_SIZE, HEADER_SIZE + 1] {
            let (frames, total) = demux_grundig(&whole[..BLOCK_SIZE + cut]);
            assert_eq!(total, 2);
            assert_eq!(frames, frames_from(&bits, 2));
        }
    }

    #[test]
    fn test_grundig_frame_needs_whole_last_word() {
        let data = block(0x00, 3, 1, 1);
        let bits = payload_bits(&data);

        let (frames, total) = demux_grundig(&data[..HEADER_SIZE + 41]);
        assert_eq!(total, 0);
        assert!(frames.is_empty());

        let (frames, total) = demux_grundig(&data[..HEADER_SIZE + 42]);
        assert_eq!(total, 1);
        assert_eq!(frames, frames_from(&bits, 1));
    }

    #[test]
    fn test_grundig_blocks_without_frames_give_nothing() {
        let data = [block(0x00, 3, 0, 1), block(0x00, 3, 0, 2)].concat();
        assert_eq!(demux_grundig(&data), (Vec::<Vec<u8>>::new(), 0));
        assert_eq!(demux_grundig(&[]), (Vec::<Vec<u8>>::new(), 0));

        let mut demuxer = GrundigSpStreamDemuxer::new(6);
        assert!(demuxer.push(&[0u8; 6 * BLOCK_SIZE]).unwrap().is_empty());
        assert!(demuxer.finish().unwrap().is_empty());
    }
}
