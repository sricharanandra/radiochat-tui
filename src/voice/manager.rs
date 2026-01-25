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
    TxActivity(bool),
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
    is_joined: Arc<AtomicBool>,
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
            is_joined: Arc::new(AtomicBool::new(false)),
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
        eprintln!("[VOICE] join_voice called for room: {}", room_id);
        
        // Reset any existing state first (in case of rejoin)
        {
            let mut audio = self.audio_engine.lock().await;
            audio.reset();
        }
        self.local_track = None;
        
        self.room_id = Some(room_id.clone());
        self.is_joined.store(true, Ordering::Relaxed);
        let _ = self.event_tx.send(VoiceEvent::StatusUpdate("Connected".to_string()));
        
        // 1. Setup Audio Engine
        let (encoded_tx, mut encoded_rx) = mpsc::unbounded_channel();
        {
            let mut audio = self.audio_engine.lock().await;
            if let Err(e) = audio.start_capture(encoded_tx) {
                let err_msg = format!("Failed to start microphone: {}", e);
                eprintln!("[VOICE] {}", err_msg);
                self.event_tx.send(VoiceEvent::Error(err_msg.clone()))?;
                return Err(anyhow::anyhow!(err_msg));
            }
            eprintln!("[VOICE] Microphone capture started");
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
        let event_tx = self.event_tx.clone();
        tokio::spawn(async move {
            while let Some(packet) = encoded_rx.recv().await {
                // Check mute state
                if is_muted.load(Ordering::Relaxed) {
                    continue; // Skip sending packets
                }

                let _ = event_tx.send(VoiceEvent::TxActivity(true));
                
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
        let is_joined = self.is_joined.clone();
        let event_tx_clone = self.event_tx.clone();
        let remote_user_for_track = remote_user_id.clone();
        pc.on_track(Box::new(move |track, _, _| {
            let audio_engine = audio_engine_clone.clone();
            let joined_state = is_joined.clone();
            let event_tx = event_tx_clone.clone();
            let peer_id = remote_user_for_track.clone();
            Box::pin(async move {
                eprintln!("[VOICE] on_track fired for peer: {}", peer_id);
                
                if !joined_state.load(Ordering::Relaxed) {
                    eprintln!("[VOICE] Ignoring track - not joined");
                    return;
                }
                let (packet_tx, packet_rx) = mpsc::unbounded_channel();
                
                // Start playback thread for this track
                {
                    let mut engine = audio_engine.lock().await;
                    eprintln!("[VOICE] Starting playback for peer: {}", peer_id);
                    if let Err(e) = engine.start_playback(packet_rx) {
                        eprintln!("[VOICE] Audio playback failed for {}: {}", peer_id, e);
                        let _ = event_tx.send(VoiceEvent::Error(format!("Audio playback failed: {}", e)));
                        return;
                    }
                    eprintln!("[VOICE] Playback started successfully for peer: {}", peer_id);
                }

                // Loop reading RTP packets
                let mut rtp_count = 0u64;
                while let Ok((rtp, _attr)) = track.read_rtp().await {
                    rtp_count += 1;
                    if rtp_count == 1 {
                        eprintln!("[VOICE] First RTP packet from {} ({} bytes payload)", peer_id, rtp.payload.len());
                    }
                    let _ = packet_tx.send(rtp.payload.to_vec());
                }
                eprintln!("[VOICE] RTP loop ended for {} after {} packets", peer_id, rtp_count);
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
        // Set joined flag to false FIRST to stop on_track callbacks
        self.is_joined.store(false, Ordering::Relaxed);
        
        if let Some(_room_id) = &self.room_id {
            // Notify server
            self.event_tx.send(VoiceEvent::Signal {
                target_id: None,
                signal_type: "leave_voice".to_string(),
                data: "".to_string(),
            })?;
        }
        
        // Reset audio engine (stops all streams)
        {
            let mut audio = self.audio_engine.lock().await;
            audio.reset();
        }
        
        // Clear local track
        self.local_track = None;
        
        // Close all peers non-blocking (fire and forget)
        let mut peers = self.peers.lock().await;
        for (peer_id, pc) in peers.drain() {
            tokio::spawn(async move {
                eprintln!("[VOICE] Closing peer connection to {}", peer_id);
                let _ = pc.close().await;
            });
        }
        
        self.room_id = None;
        let _ = self.event_tx.send(VoiceEvent::StatusUpdate("Disconnected".to_string()));
        let _ = self.event_tx.send(VoiceEvent::TxActivity(false));
        
        Ok(())
    }

    pub async fn handle_signal(&mut self, sender_id: &str, signal_type: &str, data: &str) -> Result<()> {
        eprintln!("[VOICE] handle_signal: type={}, sender={}, is_joined={}", 
            signal_type, sender_id, self.is_joined.load(Ordering::Relaxed));
        
        match signal_type {
            "join_voice" => {
                eprintln!("[VOICE] Peer {} joined voice, creating peer connection with offer", sender_id);
                // Remote user joined, we connect to them (Host/Client logic needed to avoid double connection)
                // Simplest: Use ID comparison. If my_id < their_id, I initiate.
                // But I don't have my_id easily here (it's in App).
                // Alternative: "join_voice" just means "I am here". 
                // Let's assume the EXISTING users initiate connections to the NEW user.
                match self.create_peer_connection(sender_id.to_string(), true).await {
                    Ok(_) => eprintln!("[VOICE] Peer connection created successfully for {}", sender_id),
                    Err(e) => eprintln!("[VOICE] Failed to create peer connection for {}: {}", sender_id, e),
                }
            }
            "offer" => {
                eprintln!("[VOICE] Received offer from {}", sender_id);
                let pc = self.create_peer_connection(sender_id.to_string(), false).await?;
                let desc: RTCSessionDescription = serde_json::from_str(data)?;
                pc.set_remote_description(desc).await?;
                
                let answer = pc.create_answer(None).await?;
                pc.set_local_description(answer.clone()).await?;
                
                if let Ok(json) = serde_json::to_string(&answer) {
                    eprintln!("[VOICE] Sending answer to {}", sender_id);
                    self.event_tx.send(VoiceEvent::Signal {
                        target_id: Some(sender_id.to_string()),
                        signal_type: "answer".to_string(),
                        data: json,
                    })?;
                }
            }
            "answer" => {
                eprintln!("[VOICE] Received answer from {}", sender_id);
                let peers = self.peers.lock().await;
                if let Some(pc) = peers.get(sender_id) {
                    let desc: RTCSessionDescription = serde_json::from_str(data)?;
                    pc.set_remote_description(desc).await?;
                    eprintln!("[VOICE] Set remote description for {}", sender_id);
                } else {
                    eprintln!("[VOICE] No peer connection found for {}", sender_id);
                }
            }
            "candidate" => {
                eprintln!("[VOICE] Received ICE candidate from {}", sender_id);
                let peers = self.peers.lock().await;
                if let Some(pc) = peers.get(sender_id) {
                    let candidate: RTCIceCandidateInit = serde_json::from_str(data)?;
                    pc.add_ice_candidate(candidate).await?;
                } else {
                    eprintln!("[VOICE] No peer connection found for {} to add candidate", sender_id);
                }
            }
            "leave_voice" => {
                eprintln!("[VOICE] Peer {} left voice", sender_id);
                let mut peers = self.peers.lock().await;
                if let Some(pc) = peers.remove(sender_id) {
                    tokio::spawn(async move {
                        let _ = pc.close().await;
                    });
                }
                self.event_tx.send(VoiceEvent::StatusUpdate(format!("{} left voice", sender_id)))?;
            }
            _ => {
                eprintln!("[VOICE] Unknown signal type: {}", signal_type);
            }
        }
        Ok(())
    }
}
