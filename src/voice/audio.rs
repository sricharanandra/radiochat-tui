use anyhow::{anyhow, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use audiopus::{coder::Encoder, Application, Channels, SampleRate};
use tokio::sync::mpsc;

// Wrapper to make cpal::Stream Send (required for tokio::spawn)
// ALSA streams are raw pointers, but handles are usually thread-safe to drop.
struct SendStream(cpal::Stream);
unsafe impl Send for SendStream {}

pub struct AudioEngine {
    input_stream: Option<SendStream>,
}

impl AudioEngine {
    pub fn new() -> Self {
        Self { input_stream: None }
    }

    pub fn start_capture(&mut self, encoded_tx: mpsc::UnboundedSender<Vec<u8>>) -> Result<()> {
        let host = cpal::default_host();
        let device = host.default_input_device().ok_or(anyhow!("No input device"))?;
        
        // Try to find a config that supports 48kHz, then 24kHz, then 16kHz
        let mut supported_configs = device.supported_input_configs()?;
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
        
        // If no preferred rate found, fallback to default (likely 44.1k which will crash Opus without resampling)
        let config = if let Some(c) = selected_config {
            c
        } else {
            // Warn user or try to handle 44.1k later
            eprintln!("Warning: No standard Opus sample rate found. Fallback to default.");
            device.default_input_config()?
        };

        let stream_config: cpal::StreamConfig = config.clone().into();
        
        let sample_rate = stream_config.sample_rate.0;
        // println!("Microphone Configured Rate: {}", sample_rate);

        // Frame size for 20ms
        let frame_size = (sample_rate as usize * 20) / 1000;

        // Create a channel to send raw audio from CPAL thread to Encoder task
        let (raw_tx, mut raw_rx) = mpsc::unbounded_channel::<Vec<f32>>();

        // Spawn Encoding Task
        tokio::spawn(async move {
            // Audiopus SampleRate enum is restrictive. 
            let opus_rate = match sample_rate {
                48000 => SampleRate::Hz48000,
                24000 => SampleRate::Hz24000,
                16000 => SampleRate::Hz16000,
                12000 => SampleRate::Hz12000,
                8000 => SampleRate::Hz8000,
                _ => {
                    eprintln!("Error: Sample rate {} not supported by Opus. Resampling needed.", sample_rate);
                    return; 
                }
            };

            let encoder = match Encoder::new(opus_rate, Channels::Mono, Application::Voip) {
                Ok(e) => e,
                Err(_e) => {
                    // eprintln!("Failed to create Opus encoder: {:?}", e);
                    return;
                }
            };

            let mut buffer: Vec<f32> = Vec::with_capacity(frame_size * 2);

            while let Some(samples) = raw_rx.recv().await {
                buffer.extend_from_slice(&samples);

                while buffer.len() >= frame_size {
                    let frame: Vec<f32> = buffer.drain(0..frame_size).collect();
                    let mut output = [0u8; 1024];
                    
                    match encoder.encode_float(&frame, &mut output) {
                        Ok(len) => {
                            let packet = output[..len].to_vec();
                            if encoded_tx.send(packet).is_err() {
                                return; // Channel closed
                            }
                        },
                        Err(_e) => {
                            // eprintln!("Opus encode error: {:?}", e)
                        },
                    }
                }
            }
        });

        // Setup CPAL Stream
        let err_fn = |_err| {
            // eprintln!("Audio capture error: {}", err)
        };
        
        let stream = device.build_input_stream(
            &stream_config,
            move |data: &[f32], _: &_| {
                // We only want Mono. If input is Stereo, we take first channel or mix?
                // data is interleaved.
                // If channels > 1, we need to decimate.
                let channels = config.channels() as usize;
                if channels == 1 {
                    let _ = raw_tx.send(data.to_vec());
                } else {
                    // Take every Nth sample
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
