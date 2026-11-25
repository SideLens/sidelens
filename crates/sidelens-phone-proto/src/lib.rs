//! Version 1 message framing for the phone camera link.
//!
//! Each message is `u8 type | u32 payload_len (little endian) | payload`.
//! Payload fields follow the rng-lens v1 wire specification. The stream is
//! full-duplex: phone messages travel toward the PC while CONTROL/PING travel
//! in the reverse direction.

use std::io::{self, Read, Write};

pub const VERSION: u8 = 1;
pub const MAX_MESSAGE_SIZE: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MessageType {
    Hello = 0x01,
    Config = 0x02,
    Frame = 0x03,
    State = 0x04,
    AudioConfig = 0x05,
    Audio = 0x06,
    Control = 0x10,
    Ping = 0x11,
}

impl TryFrom<u8> for MessageType {
    type Error = io::Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0x01 => Ok(Self::Hello),
            0x02 => Ok(Self::Config),
            0x03 => Ok(Self::Frame),
            0x04 => Ok(Self::State),
            0x05 => Ok(Self::AudioConfig),
            0x06 => Ok(Self::Audio),
            0x10 => Ok(Self::Control),
            0x11 => Ok(Self::Ping),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown phone message type 0x{value:02x}"),
            )),
        }
    }
}

#[derive(Debug)]
pub struct Message {
    pub kind: MessageType,
    pub payload: Vec<u8>,
}

pub fn write_message<W: Write>(
    writer: &mut W,
    kind: MessageType,
    payload: &[u8],
) -> io::Result<()> {
    if payload.len() > MAX_MESSAGE_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("phone message exceeds {MAX_MESSAGE_SIZE} bytes"),
        ));
    }
    writer.write_all(&[kind as u8])?;
    writer.write_all(&(payload.len() as u32).to_le_bytes())?;
    writer.write_all(payload)
}

/// Return `Ok(None)` only for a clean EOF before the next type byte.
pub fn read_message<R: Read>(reader: &mut R) -> io::Result<Option<Message>> {
    let mut kind = [0u8; 1];
    match reader.read(&mut kind)? {
        0 => return Ok(None),
        1 => {}
        _ => unreachable!("one-byte read cannot return more than one byte"),
    }
    let mut len = [0u8; 4];
    reader.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_MESSAGE_SIZE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("phone message length {len} exceeds limit"),
        ));
    }
    let mut payload = vec![0; len];
    reader.read_exact(&mut payload)?;
    Ok(Some(Message {
        kind: MessageType::try_from(kind[0])?,
        payload,
    }))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoCodec {
    H264 = 0,
    Hevc = 1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoConfig {
    pub codec: VideoCodec,
    pub width: u16,
    pub height: u16,
}

impl VideoConfig {
    /// CONFIG payload: codec, width, height, followed by Annex-B codec data.
    pub fn parse(payload: &[u8]) -> io::Result<(Self, &[u8])> {
        if payload.len() < 5 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "short CONFIG payload",
            ));
        }
        let codec = match payload[0] {
            0 => VideoCodec::H264,
            1 => VideoCodec::Hevc,
            value => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unsupported video codec {value}"),
                ));
            }
        };
        let config = Self {
            codec,
            width: u16::from_le_bytes([payload[1], payload[2]]),
            height: u16::from_le_bytes([payload[3], payload[4]]),
        };
        if config.width == 0 || config.height == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "zero CONFIG dimensions",
            ));
        }
        Ok((config, &payload[5..]))
    }
}

#[derive(Debug, Clone, Copy)]
pub struct VideoFrame<'a> {
    pub pts_us: u64,
    pub flags: u8,
    pub data: &'a [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioCodec {
    Opus = 0,
    AacLc = 1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioConfig {
    pub codec: AudioCodec,
    pub sample_rate: u32,
    pub channels: u8,
}

impl AudioConfig {
    /// AUDIO_CONFIG payload: codec, sample rate (LE), channels, optional codec data.
    pub fn parse(payload: &[u8]) -> io::Result<(Self, &[u8])> {
        if payload.len() < 6 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "short AUDIO_CONFIG payload",
            ));
        }
        let codec = match payload[0] {
            0 => AudioCodec::Opus,
            1 => AudioCodec::AacLc,
            value => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unsupported audio codec {value}"),
                ));
            }
        };
        let config = Self {
            codec,
            sample_rate: u32::from_le_bytes(payload[1..5].try_into().expect("checked length")),
            channels: payload[5],
        };
        if config.sample_rate == 0 || config.channels == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "zero audio config values",
            ));
        }
        Ok((config, &payload[6..]))
    }
}

#[derive(Debug, Clone, Copy)]
pub struct AudioFrame<'a> {
    pub pts_us: u64,
    pub data: &'a [u8],
}

impl<'a> AudioFrame<'a> {
    /// AUDIO payload: presentation timestamp followed by one encoded packet.
    pub fn parse(payload: &'a [u8]) -> io::Result<Self> {
        if payload.len() < 8 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "short AUDIO payload",
            ));
        }
        Ok(Self {
            pts_us: u64::from_le_bytes(payload[..8].try_into().expect("checked length")),
            data: &payload[8..],
        })
    }
}

impl<'a> VideoFrame<'a> {
    /// FRAME payload: presentation timestamp, flags, Annex-B access unit.
    pub fn parse(payload: &'a [u8]) -> io::Result<Self> {
        if payload.len() < 9 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "short FRAME payload",
            ));
        }
        Ok(Self {
            pts_us: u64::from_le_bytes(payload[..8].try_into().expect("checked length")),
            flags: payload[8],
            data: &payload[9..],
        })
    }

    pub fn is_keyframe(self) -> bool {
        self.flags & 1 != 0
    }
}
