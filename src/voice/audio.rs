use anyhow::{anyhow, Result};
use audiopus::{coder::Decoder, coder::Encoder, Application, Channels, SampleRate};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

// Wrapper to make cpal::Stream Send (required for tokio::spawn)
struct SendStream(cpal::Stream);
unsafe impl Send for SendStream {}

pub struct AudioEngine {
    input_stream: Option<SendStream>,
    output_streams: Vec<SendStream>,
}

struct StatefulResampler {
    from_rate: u32,
    to_rate: u32,
    last_sample: f32,
    fraction: f32,
    samples_to_drop: usize, // How many samples to skip at start of next chunk
}

impl StatefulResampler {
    fn new(from: u32, to: u32) -> Self {
        Self { from_rate: from, to_rate: to, last_sample: 0.0, fraction: 0.0, samples_to_drop: 0 }
    }

    fn process(&mut self, input: &[f32]) -> Vec<f32> {
        if self.from_rate == self.to_rate {
            return input.to_vec();
        }

        let ratio = self.from_rate as f32 / self.to_rate as f32;
        let mut output = Vec::with_capacity(input.len());
        
        // Start index relative to this buffer
        // -1 refers to self.last_sample
        // 0 refers to input[0]
        let mut idx: i32 = -1 + (self.samples_to_drop as i32);
        
        loop {
            // Check bounds for interpolation interval [idx, idx+1]
            if idx + 1 >= input.len() as i32 {
                break;
            }
            
            let s0 = if idx == -1 { self.last_sample } else { input[idx as usize] };
            let s1 = input[(idx + 1) as usize];
            
            let val = s0 + (s1 - s0) * self.fraction;
            output.push(val);
            
            self.fraction += ratio;
            while self.fraction >= 1.0 {
                self.fraction -= 1.0;
                idx += 1;
            }
        }
        
        // Calculate carry-over
        // `idx` is now the index of the "base" sample for the *next* output sample (which we couldn't produce).
        // It is >= input.len() - 1.
        // We want to map this to the next buffer.
        // Next buffer starts at index 0 (which was index `len` in this frame).
        // So new_idx = idx - len.
        // If new_idx < 0 (e.g. -1), it means we start with `last_sample`.
        // If new_idx >= 0, we skip `new_idx + 1` samples? No.
        
        // Example: len=10. Loop breaks at idx=9.
        // new_idx = 9 - 10 = -1.
        // Next call: starts at -1. Correct.
        
        // Example: len=10. Loop breaks at idx=10 (overshot by 1).
        // new_idx = 10 - 10 = 0.
        // Next call starts at 0.
        // Means `s0` is `input[0]`. `s1` is `input[1]`.
        // We effectively skipped using `last_sample` (prev[9]) as `s0`. Correct.
        
        let new_idx = idx - (input.len() as i32);
        
        // We need to store `last_sample` regardless.
        if let Some(last) = input.last() {
            self.last_sample = *last;
        }
        
        // samples_to_drop = new_idx - (-1) = new_idx + 1
        self.samples_to_drop = (new_idx + 1) as usize;
        
        output
    }
}

impl AudioEngine {
    pub fn new() -> Self {
        Self { 
            input_stream: None,
            output_streams: Vec::new(),
        }
    }

    /// Reset all audio streams - must be called before rejoining voice
    pub fn reset(&mut self) {
        // Drop input stream (stops capture)
        if self.input_stream.take().is_some() {
            eprintln!("[AUDIO] Stopped input stream");
        }
        
        // Drop all output streams (stops playback)
        let count = self.output_streams.len();
        self.output_streams.clear();
        if count > 0 {
            eprintln!("[AUDIO] Stopped {} output stream(s)", count);
        }
    }

    pub fn start_playback(&mut self, mut packet_rx: mpsc::UnboundedReceiver<Vec<u8>>) -> Result<()> {
        eprintln!("[AUDIO] start_playback() called");
        
        let host = cpal::default_host();
        let device = host.default_output_device().ok_or(anyhow!("No output device"))?;
        
        eprintln!("[AUDIO] Output device: {:?}", device.name());
        
        // Try to find a config that supports 48kHz (Opus native)
        let preferred_rates = [48000, 24000, 16000, 12000, 8000];
        let mut selected_config = None;

        for &rate in &preferred_rates {
            let configs = device.supported_output_configs()?;
            if let Some(c) = configs.into_iter().find(|c| {
                c.min_sample_rate().0 <= rate && c.max_sample_rate().0 >= rate
            }) {
                selected_config = Some(c.with_sample_rate(cpal::SampleRate(rate)));
                break;
            }
        }

        // If no preferred rate, fallback to default (often 44.1k)
        let config = if let Some(c) = selected_config {
            c
        } else {
            device.default_output_config()?
        };

        let stream_config: cpal::StreamConfig = config.clone().into();
        let device_sample_rate = stream_config.sample_rate.0;
        
        eprintln!("[AUDIO] Output sample rate: {}", device_sample_rate);

        // Opus only supports specific rates. We decode to 48k and resample if needed.
        let opus_rate = SampleRate::Hz48000;
        
        // Resampler: 48k -> device_rate
        let mut resampler = StatefulResampler::new(48000, device_sample_rate);
        
        let max_buffer_samples = device_sample_rate as usize * 2; // 2 seconds buffer
        let shared_buffer = Arc::new(Mutex::new(VecDeque::with_capacity(max_buffer_samples)));

        // Spawn Decoding Task
        let buffer_for_decode = shared_buffer.clone();
        tokio::spawn(async move {
            let mut decoder = match Decoder::new(opus_rate, Channels::Mono) {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("[AUDIO] Failed to create Opus decoder: {:?}", e);
                    return;
                }
            };
            
            let mut packet_count = 0u64;

            while let Some(packet) = packet_rx.recv().await {
                packet_count += 1;
                if packet_count == 1 {
                    eprintln!("[AUDIO] Received first audio packet ({} bytes)", packet.len());
                } else if packet_count % 500 == 0 {
                    eprintln!("[AUDIO] Received {} packets", packet_count);
                }
                
                let mut output = [0.0f32; 1920]; // 40ms at 48k
                match decoder.decode_float(Some(&packet), &mut output[..], false) {
                    Ok(len) => {
                        let decoded_frames = &output[..len];
                        // Resample if needed
                        let resampled = resampler.process(decoded_frames);
                        
                        if let Ok(mut buffer) = buffer_for_decode.lock() {
                            buffer.extend(resampled);
                            // Prevent bufferbloat / drift
                            if buffer.len() > max_buffer_samples {
                                let drain_count = buffer.len() - max_buffer_samples;
                                buffer.drain(0..drain_count);
                            }
                        }
                    },
                    Err(e) => {
                        if packet_count <= 5 {
                            eprintln!("[AUDIO] Decode error: {:?}", e);
                        }
                    },
                }
            }
            eprintln!("[AUDIO] Decode loop ended after {} packets", packet_count);
        });

        // Setup CPAL Output Stream
        let err_fn = |_err| {};
        
        let stream = device.build_output_stream(
            &stream_config,
            move |data: &mut [f32], _: &_| {
                if let Ok(mut buffer) = shared_buffer.lock() {
                    let mut written = 0;
                    while written < data.len() {
                        if let Some(sample) = buffer.pop_front() {
                            data[written] = sample;
                            written += 1;
                        } else {
                            break;
                        }
                    }
                    // Fill remainder with silence
                    for i in written..data.len() {
                        data[i] = 0.0;
                    }
                } else {
                    for sample in data.iter_mut() {
                        *sample = 0.0;
                    }
                }
            },
            err_fn,
            None
        )?;

        stream.play()?;
        self.output_streams.push(SendStream(stream));
        eprintln!("[AUDIO] Playback stream started successfully (total: {})", self.output_streams.len());
        Ok(())
    }

    pub fn start_capture(&mut self, encoded_tx: mpsc::UnboundedSender<Vec<u8>>) -> Result<()> {
        let host = cpal::default_host();
        let device = host.default_input_device().ok_or(anyhow!("No input device"))?;
        
        // Try to find a config that supports 48kHz
        let preferred_rates = [48000, 24000, 16000, 12000, 8000];
        let mut selected_config = None;
        
        for &rate in &preferred_rates {
            let configs = device.supported_input_configs()?;
            if let Some(c) = configs.into_iter().find(|c| {
                c.min_sample_rate().0 <= rate && c.max_sample_rate().0 >= rate
            }) {
                selected_config = Some(c.with_sample_rate(cpal::SampleRate(rate)));
                break;
            }
        }
        
        // Fallback to default
        let config = if let Some(c) = selected_config {
            c
        } else {
            device.default_input_config()?
        };

        let stream_config: cpal::StreamConfig = config.clone().into();
        let device_sample_rate = stream_config.sample_rate.0;

        // We encode at 48k. Resample input -> 48k.
        let opus_rate = SampleRate::Hz48000;
        let frame_size_48k = (48000 * 20) / 1000; // 960 samples for 20ms

        // Resampler: device_rate -> 48k
        let mut resampler = StatefulResampler::new(device_sample_rate, 48000);

        // Channel from CPAL -> Encoder
        let (raw_tx, mut raw_rx) = mpsc::unbounded_channel::<Vec<f32>>();

        // Spawn Encoding Task
        tokio::spawn(async move {
            let encoder = match Encoder::new(opus_rate, Channels::Mono, Application::Voip) {
                Ok(e) => e,
                Err(_) => return,
            };
            
            // We need to buffer incoming resampled samples until we have a full Opus frame (960 samples)
            let mut buffer: Vec<f32> = Vec::with_capacity(frame_size_48k * 2);

            while let Some(samples) = raw_rx.recv().await {
                // Resample incoming chunk
                let resampled = resampler.process(&samples);
                buffer.extend(resampled);

                while buffer.len() >= frame_size_48k {
                    let frame: Vec<f32> = buffer.drain(0..frame_size_48k).collect();
                    let mut output = [0u8; 1024];
                    
                    match encoder.encode_float(&frame, &mut output) {
                        Ok(len) => {
                            let packet = output[..len].to_vec();
                            if encoded_tx.send(packet).is_err() {
                                return; // Channel closed
                            }
                        },
                        Err(_) => {},
                    }
                }
            }
        });

        // Setup CPAL Input Stream
        let err_fn = |_err| {};
        
        let stream = device.build_input_stream(
            &stream_config,
            move |data: &[f32], _: &_| {
                // Downmix to Mono if needed
                let channels = config.channels() as usize;
                if channels == 1 {
                    let _ = raw_tx.send(data.to_vec());
                } else {
                    // Take every Nth sample (decimation)
                    let mono: Vec<f32> = data.iter().step_by(channels).cloned().collect();
                    let _ = raw_tx.send(mono);
                }
            },
            err_fn,
            None
        )?;

        stream.play()?;
        self.input_stream = Some(SendStream(stream));
        
        Ok(())
    }
}
