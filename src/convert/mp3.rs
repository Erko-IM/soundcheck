//! MP3, written by LAME: the ID3v2 tag first, with the markers as its
//! chapters, then the frames, the first of which LAME fills in at the end
//! with the length and the encoder's delay and padding, which players need
//! to give back exactly what went in, with no silence added.

use std::fs::File;
use std::io::{self, BufWriter, Seek, SeekFrom, Write};

use mp3lame_encoder::{
    Bitrate, Builder, Encoder, FlushGap, InterleavedPcm, MonoPcm, Quality, VbrMode,
};

use super::carry::Carried;
use super::{Encode, Frames, Mp3, Spec};

fn failed(e: io::Error) -> String {
    format!("cannot write the new file: {e}")
}

pub struct Writer {
    out: BufWriter<File>,
    lame: Encoder,
    channels: usize,
    /// Where the first frame starts, which LAME's own header frame takes.
    first: u64,
    buffer: Vec<u8>,
}

impl Writer {
    pub fn new(file: File, spec: Spec, quality: Mp3, carried: &Carried) -> Result<Self, String> {
        let bad = |e: mp3lame_encoder::BuildError| format!("LAME cannot be set up: {e}");
        let mut builder = Builder::new().ok_or("LAME cannot be set up")?;
        builder.set_num_channels(spec.channels as u8).map_err(bad)?;
        builder.set_sample_rate(spec.rate).map_err(bad)?;
        builder.set_quality(Quality::Best).map_err(bad)?;
        match quality {
            Mp3::V0 | Mp3::V2 => {
                builder.set_vbr_mode(VbrMode::Mtrh).map_err(bad)?;
                let q = if quality == Mp3::V0 {
                    Quality::Best
                } else {
                    Quality::NearBest
                };
                builder.set_vbr_quality(q).map_err(bad)?;
            }
            Mp3::Cbr320 | Mp3::Cbr256 | Mp3::Cbr192 | Mp3::Cbr128 => {
                builder.set_vbr_mode(VbrMode::Off).map_err(bad)?;
                let rate = match quality {
                    Mp3::Cbr320 => Bitrate::Kbps320,
                    Mp3::Cbr256 => Bitrate::Kbps256,
                    Mp3::Cbr192 => Bitrate::Kbps192,
                    _ => Bitrate::Kbps128,
                };
                builder.set_brate(rate).map_err(bad)?;
            }
        }
        builder.set_to_write_vbr_tag(true).map_err(bad)?;
        let lame = builder.build().map_err(bad)?;
        let mut out = BufWriter::with_capacity(1 << 20, file);
        let tag = carried.id3v2(true, |_| false);
        let mut first = 0;
        if !tag.is_empty() {
            let bytes = carried.id3v2_bytes(&tag)?;
            out.write_all(&bytes).map_err(failed)?;
            first = bytes.len() as u64;
        }
        Ok(Self {
            out,
            lame,
            channels: spec.channels,
            first,
            buffer: Vec::new(),
        })
    }
}

impl Encode for Writer {
    fn push(&mut self, frames: Frames<'_>) -> Result<(), String> {
        let Frames::Float(samples) = frames else {
            return Err("MP3 is encoded from floats".into());
        };
        let n = samples.len() / self.channels;
        self.buffer.clear();
        self.buffer
            .reserve(mp3lame_encoder::max_required_buffer_size(n));
        if self.channels == 1 {
            self.lame.encode_to_vec(MonoPcm(samples), &mut self.buffer)
        } else {
            self.lame
                .encode_to_vec(InterleavedPcm(samples), &mut self.buffer)
        }
        .map_err(|e| format!("LAME failed: {e}"))?;
        self.out.write_all(&self.buffer).map_err(failed)
    }

    fn finish(mut self: Box<Self>) -> Result<(), String> {
        self.buffer.clear();
        self.buffer.reserve(7200);
        self.lame
            .flush_to_vec::<FlushGap>(&mut self.buffer)
            .map_err(|e| format!("LAME failed: {e}"))?;
        self.out.write_all(&self.buffer).map_err(failed)?;
        let mut header = Vec::with_capacity(self.lame.lame_tag_size().max(1));
        self.lame.lame_tag_encode_to_vec(&mut header);
        let first = self.first;
        (|| -> io::Result<()> {
            self.out.flush()?;
            let file = self.out.get_mut();
            if !header.is_empty() {
                file.seek(SeekFrom::Start(first))?;
                file.write_all(&header)?;
            }
            file.sync_all()
        })()
        .map_err(failed)
    }
}
