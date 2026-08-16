use std::error::Error;

use ogg::{PacketWriteEndInfo, PacketWriter};
use opus_rs::{Application, OpusEncoder};
use rusty_mp3::{Error as Mp3Error, Mp3Encoder, Mp3EncoderConfig};

#[derive(Clone, Copy)]
pub(crate) enum FixtureFormat {
    Wav,
    Mp3,
    Flac,
    OggOpus,
}

impl FixtureFormat {
    pub(crate) const fn media_type(self) -> &'static str {
        match self {
            Self::Wav => "audio/wav",
            Self::Mp3 => "audio/mpeg",
            Self::Flac => "audio/flac",
            Self::OggOpus => "audio/ogg",
        }
    }
}

pub(crate) fn valid(format: FixtureFormat) -> Result<Vec<u8>, Box<dyn Error>> {
    match format {
        FixtureFormat::Wav => wav(8_000, 800),
        FixtureFormat::Mp3 => mp3(8_000, 800),
        FixtureFormat::Flac => flac(8_000, 800),
        FixtureFormat::OggOpus => ogg_opus(5, 960),
    }
}

pub(crate) fn expected_metadata(format: FixtureFormat) -> serde_json::Value {
    let sample_rate_hz = match format {
        FixtureFormat::OggOpus => 48_000,
        FixtureFormat::Wav | FixtureFormat::Mp3 | FixtureFormat::Flac => 8_000,
    };
    serde_json::json!({"channels": 1, "sample_rate_hz": sample_rate_hz})
}

pub(crate) fn truncated(format: FixtureFormat) -> Result<Vec<u8>, Box<dyn Error>> {
    let mut bytes = valid(format)?;
    bytes.truncate(bytes.len() / 2);
    Ok(bytes)
}

pub(crate) fn malformed(format: FixtureFormat) -> Vec<u8> {
    match format {
        FixtureFormat::Wav => b"RIFF\x04\x00\x00\x00WAVE".to_vec(),
        FixtureFormat::Mp3 => vec![0xff, 0xfb, 0x00, 0x00],
        FixtureFormat::Flac => b"fLaCmalformed".to_vec(),
        FixtureFormat::OggOpus => b"OggSmalformed-OpusHead".to_vec(),
    }
}

pub(crate) fn oversized(format: FixtureFormat) -> Result<Vec<u8>, Box<dyn Error>> {
    let mut bytes = valid(format)?;
    bytes.resize(
        signalbox_file_media_adapters_audio::MAX_AUDIO_SOURCE_BYTES as usize + 1,
        0,
    );
    Ok(bytes)
}

pub(crate) fn duration_bomb(format: FixtureFormat) -> Result<Vec<u8>, Box<dyn Error>> {
    match format {
        FixtureFormat::Wav => wav(1_000, 61_000),
        FixtureFormat::Mp3 => mp3(8_000, 488_000),
        FixtureFormat::Flac => flac(8_000, 488_000),
        FixtureFormat::OggOpus => ogg_opus(3_001, 960),
    }
}

fn wav(sample_rate_hz: u32, frames: usize) -> Result<Vec<u8>, Box<dyn Error>> {
    let data_size = u32::try_from(frames)?;
    let riff_size = 36_u32.checked_add(data_size).ok_or("WAV size overflow")?;
    let mut bytes = Vec::with_capacity(44 + frames);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&riff_size.to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16_u32.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&sample_rate_hz.to_le_bytes());
    bytes.extend_from_slice(&sample_rate_hz.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&8_u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_size.to_le_bytes());
    bytes.resize(44 + frames, 128);
    Ok(bytes)
}

fn mp3(sample_rate_hz: u32, frames: usize) -> Result<Vec<u8>, Box<dyn Error>> {
    let samples = vec![0_i16; frames];
    let mut encoder = Mp3Encoder::new(Mp3EncoderConfig {
        bitrate_kbps: 8,
        vbr_quality: None,
    });
    encoder.push_pcm_s16(&samples, 1, sample_rate_hz)?;
    encoder.finish();
    let mut bytes = Vec::new();
    loop {
        match encoder.next_packet() {
            Ok(packet) => bytes.extend_from_slice(&packet),
            Err(Mp3Error::Eof) => break,
            Err(Mp3Error::Again) => {
                return Err("MP3 encoder requested more input after finish".into());
            }
            Err(error) => return Err(error.into()),
        }
    }
    let mut tagged = b"ID3\x04\x00\x00\x00\x00\x00\x00".to_vec();
    tagged.extend_from_slice(&bytes);
    Ok(tagged)
}

fn flac(sample_rate_hz: u32, frames: usize) -> Result<Vec<u8>, Box<dyn Error>> {
    const BLOCK_SIZE: usize = 256;
    let total_samples = u64::try_from(frames)?;
    let stream_info =
        (u64::from(sample_rate_hz) << 44) | (7_u64 << 36) | (total_samples & 0x0f_ff_ff_ff_ff);
    let mut bytes = b"fLaC".to_vec();
    bytes.extend_from_slice(&[0x80, 0, 0, 34]);
    bytes.extend_from_slice(&u16::try_from(BLOCK_SIZE)?.to_be_bytes());
    bytes.extend_from_slice(&u16::try_from(BLOCK_SIZE)?.to_be_bytes());
    bytes.extend_from_slice(&[0; 6]);
    bytes.extend_from_slice(&stream_info.to_be_bytes());
    bytes.extend_from_slice(&[0; 16]);

    for (frame_number, start) in (0..frames).step_by(BLOCK_SIZE).enumerate() {
        let block_size = (frames - start).min(BLOCK_SIZE);
        append_constant_flac_frame(
            &mut bytes,
            u32::try_from(frame_number)?,
            u16::try_from(block_size)?,
            sample_rate_hz,
        )?;
    }
    Ok(bytes)
}

fn append_constant_flac_frame(
    bytes: &mut Vec<u8>,
    frame_number: u32,
    block_size: u16,
    sample_rate_hz: u32,
) -> Result<(), Box<dyn Error>> {
    let frame_start = bytes.len();
    bytes.extend_from_slice(&[0xff, 0xf8, 0x6c, 0x02]);
    let frame_character = char::from_u32(frame_number).ok_or("FLAC frame number overflow")?;
    let mut encoded_frame_number = [0_u8; 4];
    bytes.extend_from_slice(
        frame_character
            .encode_utf8(&mut encoded_frame_number)
            .as_bytes(),
    );
    bytes.push(u8::try_from(block_size - 1)?);
    bytes.push(u8::try_from(sample_rate_hz / 1_000)?);
    bytes.push(flac_crc8(&bytes[frame_start..]));
    bytes.extend_from_slice(&[0, 0]);
    let checksum = flac_crc16(&bytes[frame_start..]);
    bytes.extend_from_slice(&checksum.to_be_bytes());
    Ok(())
}

fn flac_crc8(bytes: &[u8]) -> u8 {
    let mut crc = 0_u8;
    for byte in bytes {
        crc ^= byte;
        for _ in 0..8 {
            crc = if crc & 0x80 != 0 {
                (crc << 1) ^ 0x07
            } else {
                crc << 1
            };
        }
    }
    crc
}

fn flac_crc16(bytes: &[u8]) -> u16 {
    let mut crc = 0_u16;
    for byte in bytes {
        crc ^= u16::from(*byte) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x8005
            } else {
                crc << 1
            };
        }
    }
    crc
}

fn ogg_opus(packet_count: usize, frame_size: usize) -> Result<Vec<u8>, Box<dyn Error>> {
    const SERIAL: u32 = 0x51_67_6e_6c;
    let mut encoder = OpusEncoder::new(48_000, 1, Application::Audio)?;
    encoder.bitrate_bps = 6_000;
    let samples = vec![0.0_f32; frame_size];
    let mut encoded = vec![0_u8; 1_276];
    let packet_bytes = encoder.encode(&samples, frame_size, &mut encoded)?;
    encoded.truncate(packet_bytes);

    let mut writer = PacketWriter::new(Vec::new());
    writer.write_packet(opus_head(), SERIAL, PacketWriteEndInfo::EndPage, 0)?;
    writer.write_packet(opus_tags(), SERIAL, PacketWriteEndInfo::NormalPacket, 0)?;
    for packet_index in 0..packet_count {
        let end = if packet_index + 1 == packet_count {
            PacketWriteEndInfo::EndStream
        } else {
            PacketWriteEndInfo::NormalPacket
        };
        let granule = u64::try_from(packet_index + 1)? * u64::try_from(frame_size)?;
        writer.write_packet(encoded.clone(), SERIAL, end, granule)?;
    }
    Ok(writer.into_inner())
}

fn opus_head() -> Vec<u8> {
    let mut head = b"OpusHead".to_vec();
    head.push(1);
    head.push(1);
    head.extend_from_slice(&0_u16.to_le_bytes());
    head.extend_from_slice(&48_000_u32.to_le_bytes());
    head.extend_from_slice(&0_i16.to_le_bytes());
    head.push(0);
    head
}

fn opus_tags() -> Vec<u8> {
    let mut tags = b"OpusTags".to_vec();
    tags.extend_from_slice(&0_u32.to_le_bytes());
    tags.extend_from_slice(&0_u32.to_le_bytes());
    tags
}
