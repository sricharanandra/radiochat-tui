use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::{mpsc, Mutex};
use anyhow::Result;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::{MediaEngine, MIME_TYPE_OPUS};
use webrtc::api::APIBuilder;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
use webrtc::track::track_local::TrackLocal;
use webrtc::media::Sample;

use crate::voice::audio::{AudioEngine, AudioDeviceError};

#[derive(Debug, Clone, PartialEq)]
pub enum VoiceConnectionStatus {
    Disconnected,
    Connecting,
    Connected,
    Reconnecting,
}

impl Default for VoiceConnectionStatus {
    fn default() -> Self {
        VoiceConnectionStatus::Disconnected
    }
}

#[derive(Debug)]
pub enum VoiceEvent {
    Signal { target_id: Option<String>, signal_type: String, data: String },
    Connecting,
    Connected,
    Disconnected,
    ConnectionFailed(String),
    MuteStateChanged(bool),
    TxActivity(bool),
    AudioError(String),
}

pub enum VoiceCommand {
    Join(String),
    Leave,
    Mute(bool),
    Signal { sender_id: String, signal_type: String, data: String },
}

pub struct VoiceManager {
    room_id: Option<String>,
    event_tx: mpsc::UnboundedSender<VoiceEvent>,
    audio_engine: Arc<Mutex<AudioEngine>>,
    audio_error_rx: Option<mpsc::UnboundedReceiver<AudioDeviceError>>,
    server_peer: Option<Arc<RTCPeerConnection>>,
    local_track: Option<Arc<TrackLocalStaticSample>>,
    is_muted: Arc<AtomicBool>,
    is_joined: Arc<AtomicBool>,
}

impl VoiceManager {
    pub fn new(event_tx: mpsc::UnboundedSender<VoiceEvent>) -> Self {
        let (audio_error_tx, audio_error_rx) = mpsc::unbounded_channel::<AudioDeviceError>();

        let mut audio_engine = AudioEngine::new();
        audio_engine.set_error_channel(audio_error_tx);

        Self {
            room_id: None,
            event_tx,
            audio_engine: Arc::new(Mutex::new(audio_engine)),
            audio_error_rx: Some(audio_error_rx),
            server_peer: None,
            local_track: None,
            is_muted: Arc::new(AtomicBool::new(false)),
            is_joined: Arc::new(AtomicBool::new(false)),
        }
    }

    pub async fn run(&mut self, mut command_rx: mpsc::UnboundedReceiver<VoiceCommand>) {
        let mut audio_error_rx = self.audio_error_rx.take();

        loop {
            tokio::select! {
                Some(cmd) = command_rx.recv() => {
                    match cmd {
                        VoiceCommand::Join(room_id) => {
                            let _ = self.join_voice(room_id).await;
                        }
                        VoiceCommand::Leave => {
                            let _ = self.leave_voice().await;
                        }
                        VoiceCommand::Mute(muted) => {
                            self.is_muted.store(muted, Ordering::Relaxed);
                            let _ = self.event_tx.send(VoiceEvent::MuteStateChanged(muted));
                        }
                        VoiceCommand::Signal { sender_id, signal_type, data } => {
                            let _ = self.handle_signal(&sender_id, &signal_type, &data).await;
                        }
                    }
                }
                Some(err) = async {
                    if let Some(ref mut rx) = audio_error_rx {
                        rx.recv().await
                    } else {
                        None
                    }
                } => {
                    let err_msg = match err {
                        AudioDeviceError::OutputDeviceError(e) => format!("Output device error: {}", e),
                        AudioDeviceError::InputDeviceError(e) => format!("Input device error: {}", e),
                    };
                    let _ = self.event_tx.send(VoiceEvent::AudioError(err_msg));
                }
                else => break,
            }
        }
    }

    async fn join_voice(&mut self, room_id: String) -> Result<()> {
        let _ = self.event_tx.send(VoiceEvent::Connecting);

        {
            let mut audio = self.audio_engine.lock().await;
            audio.reset();
        }
        self.local_track = None;

        if let Some(pc) = self.server_peer.take() {
            let _ = pc.close().await;
        }

        self.room_id = Some(room_id.clone());
        self.is_joined.store(true, Ordering::Relaxed);

        let (encoded_tx, mut encoded_rx) = mpsc::unbounded_channel();
        {
            let mut audio = self.audio_engine.lock().await;
            if let Err(e) = audio.start_capture(encoded_tx) {
                let err_msg = format!("Failed to start microphone: {}", e);
                self.is_joined.store(false, Ordering::Relaxed);
                self.room_id = None;
                let _ = self.event_tx.send(VoiceEvent::ConnectionFailed(err_msg));
                return Err(anyhow::anyhow!("Microphone capture failed"));
            }
        }

        let track = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_OPUS.to_owned(),
                ..Default::default()
            },
            "audio".to_owned(),
            "webrtc-rs".to_owned(),
        ));
        self.local_track = Some(track.clone());

        let is_muted = self.is_muted.clone();
        let event_tx = self.event_tx.clone();
        tokio::spawn(async move {
            while let Some(packet) = encoded_rx.recv().await {
                if is_muted.load(Ordering::Relaxed) {
                    continue;
                }

                let _ = event_tx.send(VoiceEvent::TxActivity(true));

                let sample = Sample {
                    data: packet.into(),
                    duration: std::time::Duration::from_millis(20),
                    ..Default::default()
                };
                if let Err(_) = track.write_sample(&sample).await {
                    break;
                }
            }
        });

        self.event_tx.send(VoiceEvent::Signal {
            target_id: Some("server".to_string()),
            signal_type: "join_voice".to_string(),
            data: "".to_string(),
        })?;

        Ok(())
    }

    async fn create_server_connection(&mut self) -> Result<()> {
        let mut m = MediaEngine::default();
        m.register_default_codecs()?;

        let registry = register_default_interceptors(webrtc::interceptor::registry::Registry::new(), &mut m)?;
        let api = APIBuilder::new()
            .with_media_engine(m)
            .with_interceptor_registry(registry)
            .build();

        let config = RTCConfiguration {
            ice_servers: vec![RTCIceServer {
                urls: vec!["stun:stun.l.google.com:19302".to_owned()],
                ..Default::default()
            }],
            ..Default::default()
        };

        let pc = Arc::new(api.new_peer_connection(config).await?);

        if let Some(track) = &self.local_track {
            pc.add_track(Arc::clone(track) as Arc<dyn TrackLocal + Send + Sync>).await?;
        }

        let event_tx_clone = self.event_tx.clone();
        pc.on_ice_candidate(Box::new(move |c| {
            let tx = event_tx_clone.clone();
            Box::pin(async move {
                if let Some(c) = c {
                    if let Ok(json) = serde_json::to_string(&c.to_json().unwrap()) {
                        let _ = tx.send(VoiceEvent::Signal {
                            target_id: Some("server".to_string()),
                            signal_type: "candidate".to_string(),
                            data: json,
                        });
                    }
                }
            })
        }));

        let event_tx_clone = self.event_tx.clone();
        let pc_clone = pc.clone();
        pc.on_peer_connection_state_change(Box::new(move |state| {
            let tx = event_tx_clone.clone();
            let pc = pc_clone.clone();
            Box::pin(async move {
                match state {
                    RTCPeerConnectionState::Connected => {
                    }
                    RTCPeerConnectionState::Disconnected | RTCPeerConnectionState::Closed => {
                        let _ = pc.close().await;
                    }
                    RTCPeerConnectionState::Failed => {
                        let _ = tx.send(VoiceEvent::ConnectionFailed("Server connection failed".to_string()));
                    }
                    _ => {}
                }
            })
        }));

        let audio_engine_clone = self.audio_engine.clone();
        let is_joined = self.is_joined.clone();
        let event_tx_clone = self.event_tx.clone();
        pc.on_track(Box::new(move |track, _, _| {
            let audio_engine = audio_engine_clone.clone();
            let joined_state = is_joined.clone();
            let event_tx = event_tx_clone.clone();
            Box::pin(async move {
                if !joined_state.load(Ordering::Relaxed) {
                    return;
                }

                let (packet_tx, packet_rx) = mpsc::unbounded_channel();

                {
                    let mut engine = audio_engine.lock().await;
                    if let Err(e) = engine.start_playback_for_peer("server", packet_rx) {
                        let _ = event_tx.send(VoiceEvent::AudioError(format!("Audio playback failed: {}", e)));
                        return;
                    }
                }

                while let Ok((rtp, _attr)) = track.read_rtp().await {
                    let _ = packet_tx.send(rtp.payload.to_vec());
                }
            })
        }));

        self.server_peer = Some(pc.clone());
        Ok(())
    }

    async fn leave_voice(&mut self) -> Result<()> {
        self.is_joined.store(false, Ordering::Relaxed);

        if self.room_id.is_some() {
            let _ = self.event_tx.send(VoiceEvent::Signal {
                target_id: Some("server".to_string()),
                signal_type: "leave_voice".to_string(),
                data: "".to_string(),
            });
        }

        if let Some(pc) = self.server_peer.take() {
            let _ = pc.close().await;
        }

        {
            let mut audio = self.audio_engine.lock().await;
            audio.reset();
        }

        self.local_track = None;
        self.is_muted.store(false, Ordering::Relaxed);
        self.room_id = None;

        let _ = self.event_tx.send(VoiceEvent::TxActivity(false));
        let _ = self.event_tx.send(VoiceEvent::MuteStateChanged(false));
        let _ = self.event_tx.send(VoiceEvent::Disconnected);

        Ok(())
    }

    pub async fn handle_signal(&mut self, _sender_id: &str, signal_type: &str, data: &str) -> Result<()> {
        match signal_type {
            "offer" => {
                if !self.is_joined.load(Ordering::Relaxed) {
                    return Ok(());
                }

                if self.server_peer.is_none() {
                    self.create_server_connection().await?;
                }

                let pc = self.server_peer.as_ref().ok_or_else(|| anyhow::anyhow!("No server peer"))?;

                let desc: RTCSessionDescription = serde_json::from_str(data)?;
                pc.set_remote_description(desc).await?;

                let answer = pc.create_answer(None).await?;
                pc.set_local_description(answer.clone()).await?;

                if let Ok(json) = serde_json::to_string(&answer) {
                    self.event_tx.send(VoiceEvent::Signal {
                        target_id: Some("server".to_string()),
                        signal_type: "answer".to_string(),
                        data: json,
                    })?;
                }

                let _ = self.event_tx.send(VoiceEvent::Connected);
            }
            _ => {}
        }
        Ok(())
    }
}
