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

pub(crate) struct ValidFixture {
    bytes: Vec<u8>,
    media_type: &'static str,
    expected_metadata: serde_json::Value,
}

impl ValidFixture {
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub(crate) const fn media_type(&self) -> &'static str {
        self.media_type
    }

    pub(crate) const fn expected_metadata(&self) -> &serde_json::Value {
        &self.expected_metadata
    }
}

struct OggOpusFixture {
    packet_count: usize,
    frame_size: usize,
    pre_skip: u16,
    final_granule: u64,
    ending: OggEnding,
    head_ending: HeadEnding,
    first_audio_page_granule: Option<u64>,
}

enum HeadEnding {
    IsolatedPage,
    SharedPage,
}

enum OggEnding {
    EndOfStream,
    EndOfPage,
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
        FixtureFormat::OggOpus => ogg_opus(OggOpusFixture {
            packet_count: 5,
            frame_size: 960,
            pre_skip: 0,
            final_granule: 4_800,
            ending: OggEnding::EndOfStream,
            head_ending: HeadEnding::IsolatedPage,
            first_audio_page_granule: None,
        }),
    }
}

pub(crate) fn valid_fixture(format: FixtureFormat) -> Result<ValidFixture, Box<dyn Error>> {
    let sample_rate_hz = if matches!(format, FixtureFormat::OggOpus) {
        48_000
    } else {
        8_000
    };
    Ok(ValidFixture {
        bytes: valid(format)?,
        media_type: format.media_type(),
        expected_metadata: serde_json::json!({
            "channels": 1,
            "sample_rate_hz": sample_rate_hz
        }),
    })
}

pub(crate) fn truncated(format: FixtureFormat) -> Result<Vec<u8>, Box<dyn Error>> {
    let mut bytes = valid(format)?;
    bytes.truncate(bytes.len() / 2);
    Ok(bytes)
}

pub(crate) fn wav_larger_than_one_broker_range() -> Result<Vec<u8>, Box<dyn Error>> {
    wav(192_000, 600_000)
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
        FixtureFormat::OggOpus => ogg_opus(OggOpusFixture {
            packet_count: 3_001,
            frame_size: 960,
            pre_skip: 0,
            final_granule: 2_880_960,
            ending: OggEnding::EndOfStream,
            head_ending: HeadEnding::IsolatedPage,
            first_audio_page_granule: None,
        }),
    }
}

pub(crate) fn ogg_opus_without_end_of_stream() -> Result<Vec<u8>, Box<dyn Error>> {
    ogg_opus(OggOpusFixture {
        packet_count: 5,
        frame_size: 960,
        pre_skip: 0,
        final_granule: 4_800,
        ending: OggEnding::EndOfPage,
        head_ending: HeadEnding::IsolatedPage,
        first_audio_page_granule: None,
    })
}

pub(crate) fn ogg_opus_trimmed_to_duration_limit() -> Result<Vec<u8>, Box<dyn Error>> {
    ogg_opus(OggOpusFixture {
        packet_count: 3_001,
        frame_size: 960,
        pre_skip: 312,
        final_granule: 2_880_312,
        ending: OggEnding::EndOfStream,
        head_ending: HeadEnding::IsolatedPage,
        first_audio_page_granule: None,
    })
}

pub(crate) fn ogg_opus_with_shared_identification_page() -> Result<Vec<u8>, Box<dyn Error>> {
    ogg_opus(OggOpusFixture {
        packet_count: 1,
        frame_size: 960,
        pre_skip: 0,
        final_granule: 960,
        ending: OggEnding::EndOfStream,
        head_ending: HeadEnding::SharedPage,
        first_audio_page_granule: None,
    })
}

pub(crate) fn ogg_opus_with_regressing_granule() -> Result<Vec<u8>, Box<dyn Error>> {
    ogg_opus(OggOpusFixture {
        packet_count: 2,
        frame_size: 960,
        pre_skip: 0,
        final_granule: 480,
        ending: OggEnding::EndOfStream,
        head_ending: HeadEnding::IsolatedPage,
        first_audio_page_granule: Some(960),
    })
}

pub(crate) fn mp3_with_long_id3_tag() -> Result<Vec<u8>, Box<dyn Error>> {
    let encoded = mp3(8_000, 800)?;
    let audio = encoded.get(10..).ok_or("missing MP3 audio")?;
    let tag_length = 128_usize;
    let mut tagged = b"ID3\x04\x00\x00\x00\x00\x01\x00".to_vec();
    tagged.resize(10 + tag_length, 0);
    tagged.extend_from_slice(audio);
    Ok(tagged)
}

pub(crate) fn mp3_with_id3_header(major: u8, flags: u8) -> Result<Vec<u8>, Box<dyn Error>> {
    let mut bytes = mp3(8_000, 800)?;
    bytes[3] = major;
    bytes[5] = flags;
    Ok(bytes)
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

fn ogg_opus(fixture: OggOpusFixture) -> Result<Vec<u8>, Box<dyn Error>> {
    const SERIAL: u32 = 0x51_67_6e_6c;
    let mut encoder = OpusEncoder::new(48_000, 1, Application::Audio)?;
    encoder.bitrate_bps = 6_000;
    let samples = vec![0.0_f32; fixture.frame_size];
    let mut encoded = vec![0_u8; 1_276];
    let packet_bytes = encoder.encode(&samples, fixture.frame_size, &mut encoded)?;
    encoded.truncate(packet_bytes);

    let mut writer = PacketWriter::new(Vec::new());
    let head_end = match fixture.head_ending {
        HeadEnding::IsolatedPage => PacketWriteEndInfo::EndPage,
        HeadEnding::SharedPage => PacketWriteEndInfo::NormalPacket,
    };
    writer.write_packet(opus_head(fixture.pre_skip), SERIAL, head_end, 0)?;
    writer.write_packet(opus_tags(), SERIAL, PacketWriteEndInfo::NormalPacket, 0)?;
    for packet_index in 0..fixture.packet_count {
        let final_packet = packet_index + 1 == fixture.packet_count;
        let end = if packet_index == 0 && fixture.first_audio_page_granule.is_some() {
            PacketWriteEndInfo::EndPage
        } else if final_packet {
            match fixture.ending {
                OggEnding::EndOfStream => PacketWriteEndInfo::EndStream,
                OggEnding::EndOfPage => PacketWriteEndInfo::EndPage,
            }
        } else {
            PacketWriteEndInfo::NormalPacket
        };
        let granule = if packet_index == 0 && fixture.first_audio_page_granule.is_some() {
            fixture.first_audio_page_granule.unwrap_or(0)
        } else if final_packet {
            fixture.final_granule
        } else {
            u64::try_from(packet_index + 1)? * u64::try_from(fixture.frame_size)?
        };
        writer.write_packet(encoded.clone(), SERIAL, end, granule)?;
    }
    Ok(writer.into_inner())
}

fn opus_head(pre_skip: u16) -> Vec<u8> {
    let mut head = b"OpusHead".to_vec();
    head.push(1);
    head.push(1);
    head.extend_from_slice(&pre_skip.to_le_bytes());
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
