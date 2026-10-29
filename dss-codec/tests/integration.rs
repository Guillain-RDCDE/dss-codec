use std::path::Path;

#[test]
fn test_detect_ds2_sp() {
    let path = Path::new("../test_files/sample_sp.DS2");
    if !path.exists() {
        eprintln!("Skipping: test file not found (place an SP-mode .DS2 file at test_files/sample_sp.DS2)");
        return;
    }
    let data = std::fs::read(path).unwrap();
    let fmt = dss_codec::demux::detect_format(&data).unwrap();
    assert_eq!(fmt, dss_codec::demux::AudioFormat::Ds2Sp);
}

#[test]
fn test_detect_ds2_qp() {
    let path = Path::new("../test_files/sample_qp.DS2");
    if !path.exists() {
        eprintln!("Skipping: test file not found (place a QP-mode .DS2 file at test_files/sample_qp.DS2)");
        return;
    }
    let data = std::fs::read(path).unwrap();
    let fmt = dss_codec::demux::detect_format(&data).unwrap();
    assert_eq!(fmt, dss_codec::demux::AudioFormat::Ds2Qp);
}

#[test]
fn test_decode_ds2_sp_basic() {
    let path = Path::new("../test_files/sample_sp.DS2");
    if !path.exists() {
        eprintln!("Skipping: test file not found");
        return;
    }
    let buf = dss_codec::decode_file(path).unwrap();
    assert_eq!(buf.native_rate, 11000);
    assert!(!buf.samples.is_empty());
    // SP mode: 264 samples per frame
    assert_eq!(buf.samples.len() % 264, 0);
}

#[test]
fn test_decode_ds2_qp_basic() {
    let path = Path::new("../test_files/sample_qp.DS2");
    if !path.exists() {
        eprintln!("Skipping: test file not found");
        return;
    }
    let buf = dss_codec::decode_file(path).unwrap();
    assert_eq!(buf.native_rate, 16000);
    assert!(!buf.samples.is_empty());
    // QP mode: 256 samples per frame
    assert_eq!(buf.samples.len() % 256, 0);
}

#[test]
fn test_decode_dss_sp_basic() {
    let path = Path::new("../test_files/sample.DSS");
    if !path.exists() {
        eprintln!("Skipping: test file not found (place a .DSS file at test_files/sample.DSS)");
        return;
    }
    let buf = dss_codec::decode_file(path).unwrap();
    assert_eq!(buf.native_rate, 11025);
    assert!(!buf.samples.is_empty());
}

#[test]
fn test_detect_grundig_sp() {
    let data = include_bytes!("fixtures/grundig_sample.dss");
    let fmt = dss_codec::demux::detect_format(data).unwrap();
    assert_eq!(fmt, dss_codec::demux::AudioFormat::GrundigSp);
    assert_eq!(fmt.native_sample_rate(), 12000);
}

/// dss2wav's 3:4 polyphase FIR from 12 kHz to 16 kHz: 100 taps per phase,
/// one extra tap on phase 0, and 0xce zero samples of history.
fn dss2wav_fir(inp: &[f64]) -> Vec<i16> {
    use dss_codec::tables::grundig_sp::RESAMP;
    const HIST: usize = 0xce;
    let mut x = vec![0.0f64; HIST];
    x.extend_from_slice(inp);
    let n = x.len();
    let mut out = Vec::new();
    let mut uv: i64 = 0;
    let mut pos: usize = HIST;
    while pos < n {
        let mut acc = 0.0f64;
        let mut c = uv as usize;
        for k in 0..100 {
            acc += x[pos - k] * RESAMP[c];
            c += 4;
        }
        if uv == 0 {
            acc += x[pos - 100] * RESAMP[400];
        }
        out.push(if acc <= 32767.5 {
            if acc >= -32767.5 {
                (acc + 0.5).floor() as i16
            } else {
                -32767
            }
        } else {
            32767
        });
        uv += 3;
        if uv > 3 {
            let adv = uv >> 2;
            pos += adv as usize;
            uv -= 4 * adv;
        }
    }
    out
}

fn wav_samples(wav: &[u8]) -> Vec<i16> {
    wav[44..]
        .chunks_exact(2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]))
        .collect()
}

#[test]
fn test_decode_grundig_sp_bit_exact() {
    use std::io::Write;

    // Decode the committed Grundig .dss sample to a 12 kHz WAV. dss2wav, the
    // genuine Grundig decoder, made the 16 kHz reference with a 3:4 FIR after
    // the same synthesis. Run that FIR over the decode and compare every
    // sample with the reference.
    let data = include_bytes!("fixtures/grundig_sample.dss");
    let reference = include_bytes!("fixtures/grundig_sample_16k.wav");

    let dir = std::env::temp_dir();
    let in_path = dir.join("dss_codec_grundig_in.dss");
    let out_path = dir.join("dss_codec_grundig_out.wav");
    {
        let mut f = std::fs::File::create(&in_path).unwrap();
        f.write_all(data).unwrap();
    }

    let buf = dss_codec::decode_and_write(
        &in_path,
        &out_path,
        &dss_codec::output::OutputConfig::default(),
    )
    .unwrap();

    assert_eq!(buf.format, dss_codec::demux::AudioFormat::GrundigSp);
    assert_eq!(buf.native_rate, 12000);
    // 151 frames of 288 samples at 12 kHz.
    assert_eq!(buf.samples.len(), 151 * 288);

    let produced = std::fs::read(&out_path).unwrap();
    assert_eq!(u32::from_le_bytes(produced[24..28].try_into().unwrap()), 12000);
    let pcm12 = wav_samples(&produced);
    assert_eq!(pcm12.len(), buf.samples.len());
    let pcm12: Vec<f64> = pcm12.into_iter().map(f64::from).collect();
    assert_eq!(pcm12, buf.samples);

    let reference = wav_samples(reference);
    assert_eq!(reference.len(), 151 * 384);
    assert!(
        dss2wav_fir(&pcm12) == reference,
        "decode through the dss2wav FIR differs from the reference"
    );

    let _ = std::fs::remove_file(&in_path);
    let _ = std::fs::remove_file(&out_path);
}
