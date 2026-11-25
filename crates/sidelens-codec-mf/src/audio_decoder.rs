//! Raw AAC-LC audio decoder backed by the synchronous Media Foundation AAC MFT.

use std::ffi::c_void;
use std::mem::ManuallyDrop;

use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFSample, IMFTransform, MFAudioFormat_AAC, MFAudioFormat_Float,
    MFAudioFormat_PCM, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Audio,
    MFTEnumEx, MFT_CATEGORY_AUDIO_DECODER, MFT_ENUM_FLAG_LOCALMFT, MFT_ENUM_FLAG_SYNCMFT,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_END_OF_STREAM, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFT_REGISTER_TYPE_INFO, MF_E_NOTACCEPTING,
    MF_E_NO_MORE_TYPES, MF_E_TRANSFORM_NEED_MORE_INPUT, MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION,
    MF_MT_AAC_PAYLOAD_TYPE, MF_MT_AUDIO_AVG_BYTES_PER_SECOND, MF_MT_AUDIO_BITS_PER_SAMPLE,
    MF_MT_AUDIO_NUM_CHANNELS, MF_MT_AUDIO_SAMPLES_PER_SECOND, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE,
    MF_MT_USER_DATA,
};
use windows::Win32::System::Com::CoTaskMemFree;

use crate::com::MediaFoundation;
use crate::error::{EncoderError, Result};

const MAX_INPUT_RETRIES: usize = 4;
const MAX_OUTPUTS_PER_INPUT: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PcmKind {
    Signed16,
    Float32,
}

/// Decodes one raw AAC-LC access unit at a time to interleaved PCM.
#[derive(Debug)]
pub struct AacDecoder {
    transform: IMFTransform,
    _mf: MediaFoundation,
    sample_rate: u32,
    channels: u16,
    kind: PcmKind,
    output_provides_samples: bool,
    output_sample_size: u32,
}

impl AacDecoder {
    /// `codec_data` is the Android MediaCodec AudioSpecificConfig (normally 2 bytes for AAC-LC).
    pub fn new(sample_rate: u32, channels: u16, codec_data: &[u8]) -> Result<Self> {
        if sample_rate == 0 || channels == 0 || channels > 2 {
            return Err(EncoderError::new(
                "AAC config",
                format!("unsupported stream format {sample_rate}Hz/{channels}ch"),
            ));
        }
        if codec_data.len() < 2 {
            return Err(EncoderError::new(
                "AAC config",
                "AudioSpecificConfig is missing or too short",
            ));
        }
        if codec_data[0] >> 3 != 2 {
            return Err(EncoderError::new(
                "AAC config",
                "only AAC-LC AudioSpecificConfig (audioObjectType 2) is supported",
            ));
        }

        let mf = MediaFoundation::init().map_err(init_err)?;
        let transform = activate()?;
        set_input_type(&transform, sample_rate, channels, codec_data)?;
        let (kind, output_rate, output_channels) = set_output_type(&transform)?;
        if output_rate != sample_rate || output_channels != channels as u32 {
            return Err(EncoderError::new(
                "AAC output type",
                format!("decoder changed format to {output_rate}Hz/{output_channels}ch"),
            ));
        }
        let info = unsafe { transform.GetOutputStreamInfo(0) }
            .map_err(|e| EncoderError::from_win("GetOutputStreamInfo(AAC)", e))?;
        let _ = unsafe { transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0) };

        Ok(Self {
            transform,
            _mf: mf,
            sample_rate,
            channels,
            kind,
            output_provides_samples: info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32
                != 0,
            output_sample_size: info.cbSize.max(4096),
        })
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn channels(&self) -> u16 {
        self.channels
    }

    /// Decode one raw_data_block AAC frame and return any PCM made available by the MFT.
    pub fn decode(&mut self, packet: &[u8], pts_us: u64) -> Result<Vec<i16>> {
        if packet.is_empty() {
            return Ok(Vec::new());
        }
        let sample = make_input_sample(packet, pts_us, self.sample_rate)?;
        let mut accepted = false;
        for _ in 0..MAX_INPUT_RETRIES {
            match unsafe { self.transform.ProcessInput(0, &sample, 0) } {
                Ok(()) => {
                    accepted = true;
                    break;
                }
                Err(e) if e.code() == MF_E_NOTACCEPTING => {
                    let _ = self.drain_outputs()?;
                }
                Err(e) => return Err(EncoderError::from_win("ProcessInput(AAC)", e)),
            }
        }
        if !accepted {
            return Err(EncoderError::new(
                "ProcessInput(AAC)",
                "AAC decoder remained full after draining output",
            ));
        }
        self.drain_outputs()
    }

    fn drain_outputs(&mut self) -> Result<Vec<i16>> {
        let mut pcm = Vec::new();
        for _ in 0..MAX_OUTPUTS_PER_INPUT {
            let own_sample = if self.output_provides_samples {
                None
            } else {
                Some(make_output_sample(self.output_sample_size)?)
            };
            let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(own_sample),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0u32;
            let result = unsafe { self.transform.ProcessOutput(0, &mut buffers, &mut status) };
            let output = ManuallyDrop::into_inner(std::mem::replace(
                &mut buffers[0].pSample,
                ManuallyDrop::new(None),
            ));
            let events = ManuallyDrop::into_inner(std::mem::replace(
                &mut buffers[0].pEvents,
                ManuallyDrop::new(None),
            ));
            drop(events);

            match result {
                Ok(()) => {
                    if let Some(sample) = output {
                        pcm.extend(convert_pcm(&read_sample(&sample)?, self.kind));
                    }
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(pcm),
                Err(e) => return Err(EncoderError::from_win("ProcessOutput(AAC)", e)),
            }
        }
        Ok(pcm)
    }
}

impl Drop for AacDecoder {
    fn drop(&mut self) {
        let _ = unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0)
        };
    }
}

fn init_err(e: crate::com::InitError) -> EncoderError {
    EncoderError::with_hresult(
        e.stage(),
        e.hresult(),
        "Media Foundation initialization failed",
    )
}

fn activate() -> Result<IMFTransform> {
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Audio,
        guidSubtype: MFAudioFormat_AAC,
    };
    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_AUDIO_DECODER,
            MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_LOCALMFT,
            Some(&input),
            None,
            &mut activates,
            &mut count,
        )
    }
    .map_err(|e| EncoderError::from_win("MFTEnumEx(AAC decoder)", e))?;

    let mut result = None;
    if !activates.is_null() {
        let entries = unsafe { std::slice::from_raw_parts(activates, count as usize) };
        for activate in entries.iter().flatten() {
            if let Ok(transform) = unsafe { activate.ActivateObject::<IMFTransform>() } {
                result = Some(transform);
                break;
            }
        }
        unsafe { CoTaskMemFree(Some(activates as *const c_void)) };
    }
    result.ok_or_else(|| {
        EncoderError::new(
            "MFTEnumEx(AAC decoder)",
            "no synchronous AAC decoder MFT found",
        )
    })
}

fn set_input_type(
    transform: &IMFTransform,
    sample_rate: u32,
    channels: u16,
    codec_data: &[u8],
) -> Result<()> {
    let media_type = unsafe { MFCreateMediaType() }
        .map_err(|e| EncoderError::from_win("MFCreateMediaType(AAC input)", e))?;
    unsafe {
        media_type
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)
            .map_err(|e| EncoderError::from_win("SetGUID(AAC major type)", e))?;
        media_type
            .SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)
            .map_err(|e| EncoderError::from_win("SetGUID(AAC subtype)", e))?;
        media_type
            .SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, sample_rate)
            .map_err(|e| EncoderError::from_win("SetUINT32(AAC sample rate)", e))?;
        media_type
            .SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, channels as u32)
            .map_err(|e| EncoderError::from_win("SetUINT32(AAC channels)", e))?;
        media_type
            .SetUINT32(&MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, 0x29)
            .map_err(|e| EncoderError::from_win("SetUINT32(AAC profile)", e))?;
        media_type
            .SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0)
            .map_err(|e| EncoderError::from_win("SetUINT32(AAC payload type)", e))?;
        media_type
            .SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, 8_000)
            .map_err(|e| EncoderError::from_win("SetUINT32(AAC bitrate)", e))?;
        // HEAACWAVEINFO suffix: raw payload type, AAC-LC profile, struct type,
        // reserved fields, then the AudioSpecificConfig from MediaCodec.
        let mut user_data = vec![0, 0, 0x29, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        user_data.extend_from_slice(codec_data);
        media_type
            .SetBlob(&MF_MT_USER_DATA, &user_data)
            .map_err(|e| EncoderError::from_win("SetBlob(AAC AudioSpecificConfig)", e))?;
        transform
            .SetInputType(0, &media_type, 0)
            .map_err(|e| EncoderError::from_win("SetInputType(AAC)", e))?;
    }
    Ok(())
}

fn set_output_type(transform: &IMFTransform) -> Result<(PcmKind, u32, u32)> {
    for index in 0..16 {
        let media_type = match unsafe { transform.GetOutputAvailableType(0, index) } {
            Ok(media_type) => media_type,
            Err(e) if e.code() == MF_E_NO_MORE_TYPES => break,
            Err(e) => return Err(EncoderError::from_win("GetOutputAvailableType(AAC)", e)),
        };
        let subtype = unsafe { media_type.GetGUID(&MF_MT_SUBTYPE) };
        let kind = match subtype {
            Ok(guid) if guid == MFAudioFormat_PCM => PcmKind::Signed16,
            Ok(guid) if guid == MFAudioFormat_Float => PcmKind::Float32,
            _ => continue,
        };
        if unsafe { transform.SetOutputType(0, &media_type, 0) }.is_err() {
            continue;
        }
        let rate = unsafe { media_type.GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND) }
            .map_err(|e| EncoderError::from_win("GetUINT32(AAC output rate)", e))?;
        let channels = unsafe { media_type.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS) }
            .map_err(|e| EncoderError::from_win("GetUINT32(AAC output channels)", e))?;
        let bits = unsafe { media_type.GetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE) }
            .map_err(|e| EncoderError::from_win("GetUINT32(AAC output bits)", e))?;
        if (kind == PcmKind::Signed16 && bits != 16) || (kind == PcmKind::Float32 && bits != 32) {
            continue;
        }
        return Ok((kind, rate, channels));
    }
    Err(EncoderError::new(
        "SetOutputType(AAC)",
        "decoder exposed no 16-bit PCM or 32-bit float output",
    ))
}

fn make_input_sample(data: &[u8], pts_us: u64, sample_rate: u32) -> Result<IMFSample> {
    let size = u32::try_from(data.len())
        .map_err(|_| EncoderError::new("AAC input sample", "AAC frame is too large"))?;
    let buffer = unsafe { MFCreateMemoryBuffer(size) }
        .map_err(|e| EncoderError::from_win("MFCreateMemoryBuffer(AAC input)", e))?;
    unsafe {
        let mut ptr = std::ptr::null_mut();
        buffer
            .Lock(&mut ptr, None, None)
            .map_err(|e| EncoderError::from_win("Lock(AAC input)", e))?;
        if ptr.is_null() {
            let _ = buffer.Unlock();
            return Err(EncoderError::new(
                "Lock(AAC input)",
                "buffer pointer is null",
            ));
        }
        std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
        buffer
            .SetCurrentLength(size)
            .map_err(|e| EncoderError::from_win("SetCurrentLength(AAC input)", e))?;
        buffer
            .Unlock()
            .map_err(|e| EncoderError::from_win("Unlock(AAC input)", e))?;
        let sample =
            MFCreateSample().map_err(|e| EncoderError::from_win("MFCreateSample(AAC input)", e))?;
        sample
            .AddBuffer(&buffer)
            .map_err(|e| EncoderError::from_win("AddBuffer(AAC input)", e))?;
        sample
            .SetSampleTime((pts_us.min(i64::MAX as u64) as i64).saturating_mul(10))
            .map_err(|e| EncoderError::from_win("SetSampleTime(AAC)", e))?;
        sample
            .SetSampleDuration(1024i64 * 10_000_000 / sample_rate as i64)
            .map_err(|e| EncoderError::from_win("SetSampleDuration(AAC)", e))?;
        Ok(sample)
    }
}

fn make_output_sample(size: u32) -> Result<IMFSample> {
    let buffer = unsafe { MFCreateMemoryBuffer(size.max(1)) }
        .map_err(|e| EncoderError::from_win("MFCreateMemoryBuffer(AAC output)", e))?;
    unsafe { buffer.SetCurrentLength(0) }
        .map_err(|e| EncoderError::from_win("SetCurrentLength(AAC output)", e))?;
    let sample = unsafe { MFCreateSample() }
        .map_err(|e| EncoderError::from_win("MFCreateSample(AAC output)", e))?;
    unsafe { sample.AddBuffer(&buffer) }
        .map_err(|e| EncoderError::from_win("AddBuffer(AAC output)", e))?;
    Ok(sample)
}

fn read_sample(sample: &IMFSample) -> Result<Vec<u8>> {
    let buffer = unsafe { sample.ConvertToContiguousBuffer() }
        .map_err(|e| EncoderError::from_win("ConvertToContiguousBuffer(AAC)", e))?;
    unsafe {
        let mut ptr = std::ptr::null_mut();
        let mut len = 0u32;
        buffer
            .Lock(&mut ptr, None, Some(&mut len))
            .map_err(|e| EncoderError::from_win("Lock(AAC output)", e))?;
        let bytes = if ptr.is_null() || len == 0 {
            Vec::new()
        } else {
            std::slice::from_raw_parts(ptr, len as usize).to_vec()
        };
        buffer
            .Unlock()
            .map_err(|e| EncoderError::from_win("Unlock(AAC output)", e))?;
        Ok(bytes)
    }
}

fn convert_pcm(bytes: &[u8], kind: PcmKind) -> Vec<i16> {
    match kind {
        PcmKind::Signed16 => bytes
            .chunks_exact(2)
            .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
            .collect(),
        PcmKind::Float32 => bytes
            .chunks_exact(4)
            .map(|sample| {
                let value = f32::from_le_bytes([sample[0], sample[1], sample[2], sample[3]]);
                (value.clamp(-1.0, 1.0) * i16::MAX as f32) as i16
            })
            .collect(),
    }
}
