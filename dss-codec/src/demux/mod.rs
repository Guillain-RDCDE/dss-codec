pub mod dss;
pub mod ds2;
pub mod grundig;

use crate::demux::ds2::{detect_ds2_audio_start, detect_ds2_format_type};

/// Detected audio format
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioFormat {
    /// Pure DSS file (.dss), SP codec: decoded at 12000 Hz, then decimated
    /// 11:12 to 11000 Hz output
    DssSp,
    /// DS2 file (.ds2), SP mode (mode byte 0-1), 12000 Hz
    Ds2Sp,
    /// DS2 file (.ds2), QP mode (mode byte 6-7), 16000 Hz
    Ds2Qp,
    /// DS2 file (.ds2), QP7 mode (mode byte 7), 16000 Hz
    Ds2Qp7,
    /// Grundig DSS file (first byte 6, magic "dss"), SP codec at 16000 Hz output
    GrundigSp,
    /// DSS file whose blocks announce a frame mode other than 0. Those frames
    /// are G.723.1, not the SP codec: mode 2 is the 24-byte 6.3 kbit/s frame,
    /// modes 3 and 5 add the 4-byte SID frame. Written by older recorders such
    /// as the DS4000, and known as DSS LP.
    DssLp,
}

impl AudioFormat {
    pub fn native_sample_rate(&self) -> u32 {
        match self {
            AudioFormat::DssSp => 11000,
            AudioFormat::Ds2Sp => 12000,
            AudioFormat::Ds2Qp | AudioFormat::Ds2Qp7 => 16000,
            AudioFormat::GrundigSp => 16000,
            AudioFormat::DssLp => 8000,
        }
    }

    pub fn extension(&self) -> &'static str {
        match self {
            AudioFormat::DssSp => "dss",
            AudioFormat::Ds2Sp | AudioFormat::Ds2Qp | AudioFormat::Ds2Qp7 => "ds2",
            AudioFormat::GrundigSp => "dss",
            AudioFormat::DssLp => "dss",
        }
    }
}

/// Result of demuxing a file
pub struct DemuxResult {
    pub format: AudioFormat,
    pub frame_data: FrameData,
    pub total_frames: usize,
}

/// Frame data varies by format
pub enum FrameData {
    /// List of fixed-size packets (DSS SP, DS2 SP)
    Packets(Vec<Vec<u8>>),
    /// Continuous bitstream (DS2 QP)
    Stream(Vec<u8>),
}

/// Detect format from file header bytes
pub fn detect_format(data: &[u8]) -> Option<AudioFormat> {
    if data.len() < 4 {
        return None;
    }
    if data[1..4] == *b"dss" && data[0] == 6 {
        return Some(AudioFormat::GrundigSp);
    }
    // Byte 0 is the header size in 512-byte blocks. Two and three are what the
    // common recorders write, but some write a larger header; those files were
    // falling through to the DS2 branch and failing there with a misleading
    // message. Six is the Grundig variant, handled just above.
    if data[1..4] == *b"dss" && data[0] > 0 && data[0] <= 32 {
        // Byte 4 of the first block header selects the frame size. Mode 0 is the
        // 41-byte SP frame; anything else is G.723.1, which the SP decoder must
        // not be handed.
        let entete = data[0] as usize * 512;
        if data.len() > entete + 4 && data[entete + 4] != 0 {
            return Some(AudioFormat::DssLp);
        }
        return Some(AudioFormat::DssSp);
    }
    // Fichier DSS ampute de son en-tete : les blocs audio sont intacts et leur
    // chainage le prouve.
    if crate::demux::dss::looks_like_headerless_dss(data) {
        return Some(AudioFormat::DssSp);
    }
    if data[..4] == *b"\x03enc" && data.len() > 0x604 {
        let format_type = data[0x600 + 4];
        return Some(match format_type {
            7 => AudioFormat::Ds2Qp7,
            6 => AudioFormat::Ds2Qp,
            _ => AudioFormat::Ds2Sp,
        });
    }
    if matches!(&data[..4], b"\x03ds2" | b"\x01ds2" | b"\x07ds2") && data.len() > 0x604 {
        let header_size = detect_ds2_audio_start(data);
        if data.len() <= header_size + 4 {
            return None;
        }
        let format_type = detect_ds2_format_type(data, header_size);
        return Some(match format_type {
            7 => AudioFormat::Ds2Qp7,
            6 => AudioFormat::Ds2Qp,
            _ => AudioFormat::Ds2Sp,
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_ds2_like_file(magic: [u8; 4], mode: u8) -> Vec<u8> {
        let mut data = vec![0u8; 0x600 + 0x200];
        data[..4].copy_from_slice(&magic);
        data[0x600 + 4] = mode;
        data
    }

    #[test]
    fn detect_format_recognizes_encrypted_ds2_qp() {
        let data = make_ds2_like_file(*b"\x03enc", 6);
        assert_eq!(detect_format(&data), Some(AudioFormat::Ds2Qp));
    }

    #[test]
    fn detect_format_recognizes_encrypted_ds2_qp7() {
        let data = make_ds2_like_file(*b"\x03enc", 7);
        assert_eq!(detect_format(&data), Some(AudioFormat::Ds2Qp7));
    }

    #[test]
    fn detect_format_recognizes_encrypted_ds2_sp() {
        let data = make_ds2_like_file(*b"\x03enc", 0);
        assert_eq!(detect_format(&data), Some(AudioFormat::Ds2Sp));
    }
}
