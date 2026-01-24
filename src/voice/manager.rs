use std::collections::HashMap;
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
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
use webrtc::track::track_local::TrackLocal;
use webrtc::media::Sample;

use crate::voice::audio::AudioEngine;

#[derive(Debug)]
pub enum VoiceEvent {
    Signal { target_id: Option<String>, signal_type: String, data: String },
    StatusUpdate(String),
    Error(String),
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
    peers: Arc<Mutex<HashMap<String, Arc<RTCPeerConnection>>>>,
    local_track: Option<Arc<TrackLocalStaticSample>>,
    is_muted: Arc<AtomicBool>,
}

impl VoiceManager {
    pub fn new(event_tx: mpsc::UnboundedSender<VoiceEvent>) -> Self {
        Self {
            room_id: None,
            event_tx,
            audio_engine: Arc::new(Mutex::new(AudioEngine::new())),
            peers: Arc::new(Mutex::new(HashMap::new())),
            local_track: None,
            is_muted: Arc::new(AtomicBool::new(false)),
        }
    }

    pub async fn run(&mut self, mut command_rx: mpsc::UnboundedReceiver<VoiceCommand>) {
        while let Some(cmd) = command_rx.recv().await {
            match cmd {
                VoiceCommand::Join(room_id) => {
                    let _ = self.join_voice(room_id).await;
                }
                VoiceCommand::Leave => {
                    let _ = self.leave_voice().await;
                }
                VoiceCommand::Mute(muted) => {
                    self.is_muted.store(muted, Ordering::Relaxed);
                    let status = if muted { "Muted" } else { "Unmuted" };
                    let _ = self.event_tx.send(VoiceEvent::StatusUpdate(status.to_string()));
                }
                VoiceCommand::Signal { sender_id, signal_type, data } => {
                    let _ = self.handle_signal(&sender_id, &signal_type, &data).await;
                }
            }
        }
    }

    async fn join_voice(&mut self, room_id: String) -> Result<()> {
        self.room_id = Some(room_id.clone());
        
        // 1. Setup Audio Engine
        let (encoded_tx, mut encoded_rx) = mpsc::unbounded_channel();
        {
            let mut audio = self.audio_engine.lock().await;
            audio.start_capture(encoded_tx)?;
        }

        // 2. Create Local Track
        let track = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_OPUS.to_owned(),
                ..Default::default()
            },
            "audio".to_owned(),
            "webrtc-rs".to_owned(),
        ));
        self.local_track = Some(track.clone());

        // 3. Spawn Task to feed audio to track
        let is_muted = self.is_muted.clone();
        tokio::spawn(async move {
            while let Some(packet) = encoded_rx.recv().await {
                // Check mute state
                if is_muted.load(Ordering::Relaxed) {
                    continue; // Skip sending packets
                }
                
                // Send sample to WebRTC track
                let sample = Sample {
                    data: packet.into(),
                    duration: std::time::Duration::from_millis(20), // 20ms frame
                    ..Default::default()
                };
                if let Err(_) = track.write_sample(&sample).await {
                    break;
                }
            }
        });

        // 4. Notify server
        self.event_tx.send(VoiceEvent::Signal {
            target_id: None, // Broadcast
            signal_type: "join_voice".to_string(),
            data: "".to_string(),
        })?;
        
        self.event_tx.send(VoiceEvent::StatusUpdate(format!("Joined voice in room {}", room_id)))?;
        
        Ok(())
    }

    async fn create_peer_connection(&self, remote_user_id: String, initiate_offer: bool) -> Result<Arc<RTCPeerConnection>> {
        // Setup Media Engine
        let mut m = MediaEngine::default();
        m.register_default_codecs()?;
        
        // Setup API
        let mut registry = register_default_interceptors(webrtc::interceptor::registry::Registry::new(), &mut m)?;
        let api = APIBuilder::new()
            .with_media_engine(m)
            .with_interceptor_registry(registry)
            .build();

        // Config with Google STUN
        let config = RTCConfiguration {
            ice_servers: vec![RTCIceServer {
                urls: vec!["stun:stun.l.google.com:19302".to_owned()],
                ..Default::default()
            }],
            ..Default::default()
        };

        let pc = Arc::new(api.new_peer_connection(config).await?);

        // Add local track
        if let Some(track) = &self.local_track {
            pc.add_track(Arc::clone(track) as Arc<dyn TrackLocal + Send + Sync>).await?;
        }

        // Handle ICE Candidates
        let event_tx_clone = self.event_tx.clone();
        let remote_id_clone = remote_user_id.clone();
        pc.on_ice_candidate(Box::new(move |c| {
            let tx = event_tx_clone.clone();
            let rid = remote_id_clone.clone();
            Box::pin(async move {
                if let Some(c) = c {
                    if let Ok(json) = serde_json::to_string(&c.to_json().unwrap()) {
                        let _ = tx.send(VoiceEvent::Signal {
                            target_id: Some(rid),
                            signal_type: "candidate".to_string(),
                            data: json,
                        });
                    }
                }
            })
        }));

        // Handle Incoming Tracks (Remote Audio)
        let audio_engine_clone = self.audio_engine.clone();
        pc.on_track(Box::new(move |track, _, _| {
            let audio_engine = audio_engine_clone.clone();
            Box::pin(async move {
                let (packet_tx, packet_rx) = mpsc::unbounded_channel();
                
                // Start playback thread for this track
                {
                    let mut engine = audio_engine.lock().await;
                    // Ignore errors for now (e.g. no output device)
                    let _ = engine.start_playback(packet_rx);
                }

                // Loop reading RTP packets
                while let Ok((rtp, _attr)) = track.read_rtp().await {
                    let _ = packet_tx.send(rtp.payload.to_vec());
                }
            })
        }));

        let peers = self.peers.clone();
        peers.lock().await.insert(remote_user_id.clone(), pc.clone());

        if initiate_offer {
            let offer = pc.create_offer(None).await?;
            pc.set_local_description(offer.clone()).await?;
            
            // Send Offer
            if let Ok(json) = serde_json::to_string(&offer) {
                self.event_tx.send(VoiceEvent::Signal {
                    target_id: Some(remote_user_id),
                    signal_type: "offer".to_string(),
                    data: json,
                })?;
            }
        }

        Ok(pc)
    }

    async fn leave_voice(&mut self) -> Result<()> {
        if let Some(_room_id) = &self.room_id {
            // Notify server
            self.event_tx.send(VoiceEvent::Signal {
                target_id: None,
                signal_type: "leave_voice".to_string(),
                data: "".to_string(),
            })?;
            self.event_tx.send(VoiceEvent::StatusUpdate("Left voice".to_string()))?;
        }
        self.room_id = None;
        // Close all peers
        let mut peers = self.peers.lock().await;
        for (_, pc) in peers.iter() {
            pc.close().await?;
        }
        peers.clear();
        Ok(())
    }

    pub async fn handle_signal(&mut self, sender_id: &str, signal_type: &str, data: &str) -> Result<()> {
        match signal_type {
            "join_voice" => {
                // Remote user joined, we connect to them (Host/Client logic needed to avoid double connection)
                // Simplest: Use ID comparison. If my_id < their_id, I initiate.
                // But I don't have my_id easily here (it's in App).
                // Alternative: "join_voice" just means "I am here". 
                // Let's assume the EXISTING users initiate connections to the NEW user.
                self.create_peer_connection(sender_id.to_string(), true).await?;
            }
            "offer" => {
                let pc = self.create_peer_connection(sender_id.to_string(), false).await?;
                let desc: RTCSessionDescription = serde_json::from_str(data)?;
                pc.set_remote_description(desc).await?;
                
                let answer = pc.create_answer(None).await?;
                pc.set_local_description(answer.clone()).await?;
                
                if let Ok(json) = serde_json::to_string(&answer) {
                    self.event_tx.send(VoiceEvent::Signal {
                        target_id: Some(sender_id.to_string()),
                        signal_type: "answer".to_string(),
                        data: json,
                    })?;
                }
            }
            "answer" => {
                let peers = self.peers.lock().await;
                if let Some(pc) = peers.get(sender_id) {
                    let desc: RTCSessionDescription = serde_json::from_str(data)?;
                    pc.set_remote_description(desc).await?;
                }
            }
            "candidate" => {
                let peers = self.peers.lock().await;
                if let Some(pc) = peers.get(sender_id) {
                    let candidate: RTCIceCandidateInit = serde_json::from_str(data)?;
                    pc.add_ice_candidate(candidate).await?;
                }
            }
            "leave_voice" => {
                let mut peers = self.peers.lock().await;
                if let Some(pc) = peers.remove(sender_id) {
                    pc.close().await?;
                }
                self.event_tx.send(VoiceEvent::StatusUpdate(format!("{} left voice", sender_id)))?;
            }
            _ => {}
        }
        Ok(())
    }
}
