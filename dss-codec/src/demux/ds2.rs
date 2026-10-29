/// DS2 file demuxer.
///
/// SP mode (0-1): DSS SP blocks, read by the DSS SP block walk.
/// QP mode (6): segmented continuous bitstream.
/// QP7 mode (7): segmented 12/56-byte byte-aligned records.
use crate::demux::dss::dss_sp_frames;
use crate::demux::{check_sp_codec, ds2_header_size};
use crate::error::{DecodeError, Result};

const DS2_BLOCK_SIZE: usize = 512;
pub(crate) const DS2_BLOCK_HEADER_SIZE: usize = 6;
const DS2_QP_FRAME_SIZE: usize = 56;
const DS2_QP7_PACKET_SHORT_SIZE: usize = 12;
const DS2_QP7_PACKET_LONG_SIZE: usize = 56;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ds2QpSegment {
    pub stream: Vec<u8>,
    pub total_frames: usize,
    pub reset_before: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ds2Qp7Segment {
    pub records: Vec<Vec<u8>>,
    pub total_frames: usize,
    pub reset_before: bool,
}

pub(crate) fn find_ds2_format_type(data: &[u8], header_size: usize) -> Option<u8> {
    let mut bstart = header_size;
    while bstart + DS2_BLOCK_HEADER_SIZE <= data.len() {
        if data[bstart + 2] > 0 {
            return Some(data[bstart + 4]);
        }
        bstart += DS2_BLOCK_SIZE;
    }
    None
}

pub(crate) fn detect_ds2_format_type(data: &[u8], header_size: usize) -> u8 {
    find_ds2_format_type(data, header_size).unwrap_or(0)
}

/// Demux a DS2 file.
/// Returns (frame_data, total_frames, is_qp).
/// For SP: frame_data is a Vec<Vec<u8>> of packets.
/// For QP/QP7: frame data is segmented to preserve state resets at cut points.
pub fn demux_ds2(data: &[u8]) -> Result<DemuxedDs2> {
    let header_size = ds2_header_size(data)
        .ok_or_else(|| DecodeError::NotDs2(std::path::PathBuf::from("<bytes>")))?;
    let num_blocks = data.len().saturating_sub(header_size) / DS2_BLOCK_SIZE;
    let format_type = detect_ds2_format_type(data, header_size);

    let mut total_frames: usize = 0;
    for bi in 0..num_blocks {
        total_frames += data[header_size + bi * DS2_BLOCK_SIZE + 2] as usize;
    }

    if format_type == 7 {
        let payload_size = DS2_BLOCK_SIZE - DS2_BLOCK_HEADER_SIZE;
        let mut raw = Vec::with_capacity(num_blocks * payload_size);
        for bi in 0..num_blocks {
            let bstart = header_size + bi * DS2_BLOCK_SIZE;
            raw.extend_from_slice(&data[bstart + DS2_BLOCK_HEADER_SIZE..bstart + DS2_BLOCK_SIZE]);
        }

        let read_record = |raw: &[u8], pos: usize| -> Result<(Vec<u8>, usize)> {
            if pos + 2 > raw.len() {
                return Err(DecodeError::Truncated(format!(
                    "incomplete format-7 selector at byte {}",
                    pos
                )));
            }
            let size = if (raw[pos + 1] & 0x80) == 0 {
                DS2_QP7_PACKET_SHORT_SIZE
            } else {
                DS2_QP7_PACKET_LONG_SIZE
            };
            if pos + size > raw.len() {
                return Err(DecodeError::Truncated(format!(
                    "incomplete format-7 record at byte {}",
                    pos
                )));
            }
            Ok((raw[pos..pos + size].to_vec(), pos + size))
        };

        let mut segments = Vec::new();
        let mut seg_records: Vec<Vec<u8>> = Vec::new();
        let mut raw_read_pos: Option<usize> = None;

        for bi in 0..num_blocks {
            let bstart = header_size + bi * DS2_BLOCK_SIZE;
            let block_header = &data[bstart..bstart + DS2_BLOCK_HEADER_SIZE];
            let fc = block_header[2] as usize;

            if fc == 0 {
                if !seg_records.is_empty() {
                    segments.push(Ds2Qp7Segment {
                        records: std::mem::take(&mut seg_records),
                        total_frames: segments.last().map_or(0, |_| 0),
                        reset_before: !segments.is_empty(),
                    });
                    if let Some(last) = segments.last_mut() {
                        last.total_frames = last.records.len();
                    }
                }
                let zero_end = (bi + 1) * payload_size;
                raw_read_pos = Some(raw_read_pos.unwrap_or(0).max(zero_end));
                continue;
            }

            let payload_off = (block_header[1] as usize * 2).saturating_sub(DS2_BLOCK_HEADER_SIZE);
            let frames_raw_start = bi * payload_size + payload_off;
            raw_read_pos = Some(raw_read_pos.unwrap_or(0).max(frames_raw_start));

            for _ in 0..fc {
                let (record, next_pos) = read_record(&raw, raw_read_pos.unwrap_or(0))?;
                seg_records.push(record);
                raw_read_pos = Some(next_pos);
            }
        }

        if !seg_records.is_empty() {
            segments.push(Ds2Qp7Segment {
                total_frames: seg_records.len(),
                records: seg_records,
                reset_before: !segments.is_empty(),
            });
        }

        Ok(DemuxedDs2::Qp7Segments {
            total_frames: segments.iter().map(|seg| seg.total_frames).sum(),
            segments,
        })
    } else if format_type >= 6 {
        let payload_size = DS2_BLOCK_SIZE - DS2_BLOCK_HEADER_SIZE;
        let mut raw = Vec::with_capacity(num_blocks * payload_size);
        for bi in 0..num_blocks {
            let bstart = header_size + bi * DS2_BLOCK_SIZE;
            raw.extend_from_slice(&data[bstart + DS2_BLOCK_HEADER_SIZE..bstart + DS2_BLOCK_SIZE]);
        }

        let mut segments = Vec::new();
        let mut seg_raw_start = 0usize;
        let mut seg_frames = 0usize;
        let mut raw_read_pos = 0usize;
        let mut first_seg = true;

        for bi in 0..num_blocks {
            let bstart = header_size + bi * DS2_BLOCK_SIZE;
            let b1 = data[bstart + 1] as usize;
            let fc = data[bstart + 2] as usize;
            let payload_off = (b1 * 2).saturating_sub(DS2_BLOCK_HEADER_SIZE);
            let frames_raw_start = bi * payload_size + payload_off;

            if bi == 0 {
                raw_read_pos = frames_raw_start;
                seg_raw_start = frames_raw_start;
            } else if frames_raw_start != raw_read_pos {
                let end = raw_read_pos.min(raw.len());
                if seg_frames > 0 && end > seg_raw_start {
                    segments.push(Ds2QpSegment {
                        stream: raw[seg_raw_start..end].to_vec(),
                        total_frames: seg_frames,
                        reset_before: !first_seg,
                    });
                    first_seg = false;
                }
                seg_raw_start = frames_raw_start;
                seg_frames = 0;
                raw_read_pos = frames_raw_start;
            }

            if fc > 0 {
                seg_frames += fc;
                raw_read_pos += fc * DS2_QP_FRAME_SIZE;
            }
        }

        let end = raw_read_pos.min(raw.len());
        if seg_frames > 0 && end > seg_raw_start {
            segments.push(Ds2QpSegment {
                stream: raw[seg_raw_start..end].to_vec(),
                total_frames: seg_frames,
                reset_before: !first_seg,
            });
        }

        Ok(DemuxedDs2::QpSegments {
            total_frames,
            segments,
        })
    } else {
        check_sp_codec(format_type)?;
        let (packets, _cut) = dss_sp_frames(data, header_size);
        Ok(DemuxedDs2::Sp {
            total_frames: packets.len(),
            packets,
        })
    }
}

pub enum DemuxedDs2 {
    Sp {
        packets: Vec<Vec<u8>>,
        total_frames: usize,
    },
    QpSegments {
        segments: Vec<Ds2QpSegment>,
        total_frames: usize,
    },
    Qp7Segments {
        segments: Vec<Ds2Qp7Segment>,
        total_frames: usize,
    },
}

pub(crate) struct Ds2QpStreamDemuxer {
    data: Vec<u8>,
}

impl Ds2QpStreamDemuxer {
    pub(crate) fn new() -> Self {
        Self { data: Vec::new() }
    }

    pub(crate) fn push(&mut self, data: &[u8]) -> Result<Vec<Vec<u8>>> {
        self.data.extend_from_slice(data);
        Ok(Vec::new())
    }

    pub(crate) fn finish(&mut self) -> Result<Vec<Vec<u8>>> {
        self.finish_lenient()
    }

    pub(crate) fn finish_lenient(&mut self) -> Result<Vec<Vec<u8>>> {
        match demux_ds2(&self.data)? {
            DemuxedDs2::QpSegments {
                segments,
                total_frames: _,
            } => Ok(segments.into_iter().flat_map(|seg| {
                seg.stream
                    .chunks(DS2_QP_FRAME_SIZE)
                    .take(seg.total_frames)
                    .map(|chunk| chunk.to_vec())
                    .collect::<Vec<_>>()
            }).collect()),
            DemuxedDs2::Qp7Segments {
                segments,
                total_frames: _,
            } => Ok(segments.into_iter().flat_map(|seg| seg.records).collect()),
            _ => Ok(Vec::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_ds2_file(mode: u8, frame_count: u8, payload_pattern: u8) -> Vec<u8> {
        make_ds2_file_with_header(3, mode, frame_count, payload_pattern)
    }

    fn make_ds2_file_with_header(
        first_byte: u8,
        mode: u8,
        frame_count: u8,
        payload_pattern: u8,
    ) -> Vec<u8> {
        let header_blocks = if first_byte == 1 { 3 } else { first_byte as usize };
        let mut data = vec![0u8; header_blocks * DS2_BLOCK_SIZE];
        data[0] = first_byte;
        data[1..4].copy_from_slice(b"ds2");

        let mut block = [0u8; DS2_BLOCK_SIZE];
        block[2] = frame_count;
        block[4] = mode;
        for (i, byte) in block[DS2_BLOCK_HEADER_SIZE..].iter_mut().enumerate() {
            *byte = payload_pattern.wrapping_add(i as u8);
        }

        data.extend_from_slice(&block);
        data
    }

    fn sp_packets(data: &[u8]) -> Vec<Vec<u8>> {
        match demux_ds2(data).unwrap() {
            DemuxedDs2::Sp {
                packets,
                total_frames,
            } => {
                assert_eq!(total_frames, packets.len());
                packets
            }
            _ => panic!("expected DS2 SP packets"),
        }
    }

    #[test]
    fn test_ds2_sp_demux_reads_dss_blocks() {
        let data = make_ds2_file(0, 4, 0x10);
        let mut dss = vec![0u8; 2 * DS2_BLOCK_SIZE];
        dss[..4].copy_from_slice(b"\x02dss");
        dss.extend_from_slice(&data[3 * DS2_BLOCK_SIZE..]);

        let (expected, _) = crate::demux::dss::demux_dss(&dss).unwrap();
        assert_eq!(expected.len(), 4);
        assert_eq!(sp_packets(&data), expected);
    }

    #[test]
    fn test_ds2_sp_demux_overdeclared_frames_stop_at_end_of_file() {
        let data = make_ds2_file(0, 14, 0x20);
        let packets = sp_packets(&data);
        assert_eq!(packets.len(), 12);
        assert!(packets.iter().all(|p| p.len() == 42));
    }

    #[test]
    fn test_ds2_sp_demux_two_block_header() {
        let data = make_ds2_file_with_header(2, 0, 4, 0x10);
        let short = make_ds2_file(0, 4, 0x10);
        assert_eq!(sp_packets(&data).len(), 4);
        assert_eq!(sp_packets(&data), sp_packets(&short));
    }

    #[test]
    fn test_find_ds2_format_type_reads_first_block_with_frames() {
        let mut data = make_ds2_file(6, 0, 0x10);
        let second = make_ds2_file(7, 2, 0x10);
        data.extend_from_slice(&second[3 * DS2_BLOCK_SIZE..]);
        assert_eq!(find_ds2_format_type(&data, 3 * DS2_BLOCK_SIZE), Some(7));
        assert_eq!(detect_ds2_format_type(&data, 3 * DS2_BLOCK_SIZE), 7);
    }

    #[test]
    fn test_find_ds2_format_type_reads_partial_block_header() {
        let data = make_ds2_file(6, 2, 0x10);
        let cut = 3 * DS2_BLOCK_SIZE + DS2_BLOCK_HEADER_SIZE;
        assert_eq!(find_ds2_format_type(&data[..cut], 3 * DS2_BLOCK_SIZE), Some(6));
        assert_eq!(detect_ds2_format_type(&data[..cut], 3 * DS2_BLOCK_SIZE), 6);
    }

    #[test]
    fn test_find_ds2_format_type_none_without_block_header() {
        let data = make_ds2_file(6, 2, 0x10);
        let cut = 3 * DS2_BLOCK_SIZE + DS2_BLOCK_HEADER_SIZE - 1;
        assert_eq!(find_ds2_format_type(&data[..cut], 3 * DS2_BLOCK_SIZE), None);
        assert_eq!(detect_ds2_format_type(&data[..cut], 3 * DS2_BLOCK_SIZE), 0);
        assert_eq!(find_ds2_format_type(&data[..100], 3 * DS2_BLOCK_SIZE), None);
    }

    #[test]
    fn test_find_ds2_format_type_none_when_no_block_has_frames() {
        let mut data = make_ds2_file(6, 0, 0x10);
        let second = make_ds2_file(7, 0, 0x10);
        data.extend_from_slice(&second[3 * DS2_BLOCK_SIZE..]);
        assert_eq!(find_ds2_format_type(&data, 3 * DS2_BLOCK_SIZE), None);
        assert_eq!(detect_ds2_format_type(&data, 3 * DS2_BLOCK_SIZE), 0);
    }

    #[test]
    fn test_ds2_demux_refuses_lp_and_unknown_codecs() {
        for mode in [2u8, 3, 4, 5] {
            let data = make_ds2_file(mode, 4, 0x10);
            assert!(matches!(
                demux_ds2(&data),
                Err(DecodeError::UnsupportedCodec(m)) if m == mode
            ));
        }
    }

    #[test]
    fn test_ds2_qp_stream_demux_matches_batch() {
        let data = make_ds2_file(6, 3, 0x40);
        let expected = match demux_ds2(&data).unwrap() {
            DemuxedDs2::QpSegments {
                segments,
                total_frames,
            } => segments
                .into_iter()
                .flat_map(|seg| seg.stream.into_iter())
                .collect::<Vec<_>>()[..total_frames * DS2_QP_FRAME_SIZE]
                .chunks(DS2_QP_FRAME_SIZE)
                .map(|chunk| chunk.to_vec())
                .collect::<Vec<_>>(),
            _ => panic!("expected DS2 QP stream"),
        };

        let mut demuxer = Ds2QpStreamDemuxer::new();
        let mut actual = Vec::new();
        for chunk in data.chunks(113) {
            actual.extend(demuxer.push(chunk).unwrap());
        }
        actual.extend(demuxer.finish().unwrap());

        assert_eq!(actual, expected);
    }

    #[test]
    fn test_ds2_qp_demux_two_block_header() {
        let data = make_ds2_file_with_header(2, 6, 3, 0x40);
        let short = make_ds2_file(6, 3, 0x40);
        let frames = |demuxed: DemuxedDs2| match demuxed {
            DemuxedDs2::QpSegments {
                segments,
                total_frames,
            } => (segments, total_frames),
            _ => panic!("expected DS2 QP stream"),
        };
        assert_eq!(frames(demux_ds2(&data).unwrap()), frames(demux_ds2(&short).unwrap()));
    }

    #[test]
    fn test_ds2_demux_rejects_bad_header_byte() {
        let mut data = make_ds2_file(6, 3, 0x40);
        data[0] = 0;
        assert!(matches!(demux_ds2(&data), Err(DecodeError::NotDs2(_))));
    }

    #[test]
    fn test_ds2_qp_demux_one_block_header() {
        let mut long = make_ds2_file(6, 3, 0x40);
        long[3 * DS2_BLOCK_SIZE + 3] = 0xff;
        let mut data = long[..DS2_BLOCK_SIZE].to_vec();
        data[0] = 1;
        data.extend_from_slice(&long[3 * DS2_BLOCK_SIZE..]);
        let frames = |demuxed: DemuxedDs2| match demuxed {
            DemuxedDs2::QpSegments {
                segments,
                total_frames,
            } => (segments, total_frames),
            _ => panic!("expected DS2 QP stream"),
        };
        assert_eq!(frames(demux_ds2(&data).unwrap()), frames(demux_ds2(&long).unwrap()));
    }

    #[test]
    fn test_ds2_qp_stream_demux_lenient_batching() {
        let data = make_ds2_file(6, 3, 0x55);
        let mut demuxer = Ds2QpStreamDemuxer::new();
        for chunk in data.chunks(97) {
            let _ = demuxer.push(chunk).unwrap();
        }

        assert_eq!(demuxer.finish().unwrap().len(), 3);
    }
}
