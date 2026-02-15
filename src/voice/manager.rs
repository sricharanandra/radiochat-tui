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
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
use webrtc::track::track_local::TrackLocal;
use webrtc::media::Sample;

use crate::voice::audio::{AudioEngine, AudioDeviceError};

/// Internal commands sent from async callbacks back to the VoiceManager
enum InternalCmd {
    /// Peer connection entered Failed/Disconnected/Closed state - clean it up
    CleanupPeer(String),
}

/// Voice connection status for UI display
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

/// Events sent from VoiceManager to main app
/// ALL voice state changes in the app should be driven by these events
#[derive(Debug)]
pub enum VoiceEvent {
    /// WebRTC signaling data to send to server
    Signal { target_id: Option<String>, signal_type: String, data: String },
    
    /// Voice connection state changes (single source of truth)
    Connecting,                    // Starting to join voice
    Connected,                     // Successfully joined and audio is ready
    Disconnected,                  // Clean disconnect completed
    ConnectionFailed(String),      // Failed to connect (with reason)
    
    /// Peer connection state changes  
    PeerConnected(String),         // WebRTC connection to peer established
    PeerDisconnected(String),      // WebRTC connection to peer lost
    PeerConnectionFailed(String),  // Failed to connect to specific peer
    
    /// Mute state changes (single source of truth)
    MuteStateChanged(bool),        // true = muted, false = unmuted
    
    /// Transmit activity for UI indicator
    TxActivity(bool),              // true = transmitting, false = quiet
    
    /// Audio system errors
    AudioError(String),            // Audio device/stream error
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
    peers: Arc<Mutex<HashMap<String, Arc<RTCPeerConnection>>>>,
    local_track: Option<Arc<TrackLocalStaticSample>>,
    is_muted: Arc<AtomicBool>,
    is_joined: Arc<AtomicBool>,
    /// Buffered ICE candidates that arrived before the peer connection was created
    /// FIX for Bug 1: candidates that arrive before the offer are queued here
    /// and applied once the peer connection is created via the offer handler.
    pending_candidates: HashMap<String, Vec<RTCIceCandidateInit>>,
    /// Channel for internal commands from async callbacks (e.g., peer cleanup)
    internal_tx: mpsc::UnboundedSender<InternalCmd>,
    internal_rx: Option<mpsc::UnboundedReceiver<InternalCmd>>,
}

impl VoiceManager {
    pub fn new(event_tx: mpsc::UnboundedSender<VoiceEvent>) -> Self {
        // Create audio error channel
        let (audio_error_tx, audio_error_rx) = mpsc::unbounded_channel::<AudioDeviceError>();
        
        // Create audio engine with error channel
        let mut audio_engine = AudioEngine::new();
        audio_engine.set_error_channel(audio_error_tx);
        
        // Create internal command channel for async callback -> manager communication
        let (internal_tx, internal_rx) = mpsc::unbounded_channel::<InternalCmd>();
        
        Self {
            room_id: None,
            event_tx,
            audio_engine: Arc::new(Mutex::new(audio_engine)),
            audio_error_rx: Some(audio_error_rx),
            peers: Arc::new(Mutex::new(HashMap::new())),
            local_track: None,
            is_muted: Arc::new(AtomicBool::new(false)),
            is_joined: Arc::new(AtomicBool::new(false)),
            pending_candidates: HashMap::new(),
            internal_tx,
            internal_rx: Some(internal_rx),
        }
    }

    pub async fn run(&mut self, mut command_rx: mpsc::UnboundedReceiver<VoiceCommand>) {
        // Take ownership of the audio error receiver and internal command receiver
        let mut audio_error_rx = self.audio_error_rx.take();
        let mut internal_rx = self.internal_rx.take();
        
        loop {
            tokio::select! {
                // Handle voice commands
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
                // Handle internal commands from async callbacks (Bug 5 fix)
                Some(internal) = async {
                    if let Some(ref mut rx) = internal_rx {
                        rx.recv().await
                    } else {
                        None
                    }
                } => {
                    match internal {
                        InternalCmd::CleanupPeer(peer_id) => {
                            let mut peers = self.peers.lock().await;
                            if let Some(pc) = peers.remove(&peer_id) {
                                let _ = pc.close().await;
                            }
                            // Also clean up any associated output stream
                            {
                                let mut audio = self.audio_engine.lock().await;
                                audio.remove_peer_stream(&peer_id);
                            }
                            self.pending_candidates.remove(&peer_id);
                        }
                    }
                }
                // Handle audio errors
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
        // Send Connecting event immediately
        let _ = self.event_tx.send(VoiceEvent::Connecting);
        
        // Reset any existing state first (in case of rejoin)
        {
            let mut audio = self.audio_engine.lock().await;
            audio.reset();
        }
        self.local_track = None;
        self.pending_candidates.clear();
        
        // Close any existing peer connections from previous session
        {
            let mut peers = self.peers.lock().await;
            for (_, pc) in peers.drain() {
                let _ = pc.close().await;
            }
        }
        
        self.room_id = Some(room_id.clone());
        self.is_joined.store(true, Ordering::Relaxed);
        
        // 1. Setup Audio Engine
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
        
        // Send Connected event - audio is ready
        let _ = self.event_tx.send(VoiceEvent::Connected);
        
        Ok(())
    }

    async fn create_peer_connection(&self, remote_user_id: String, initiate_offer: bool) -> Result<Arc<RTCPeerConnection>> {
        // Close any existing peer connection to this user first
        // This handles the case where a user leaves and rejoins quickly
        {
            let mut peers = self.peers.lock().await;
            if let Some(old_pc) = peers.remove(&remote_user_id) {
                let _ = old_pc.close().await;
            }
        }
        
        // Also clean up any stale output stream for this peer (Bug 4 fix)
        {
            let mut audio = self.audio_engine.lock().await;
            audio.remove_peer_stream(&remote_user_id);
        }
        
        // Setup Media Engine
        let mut m = MediaEngine::default();
        m.register_default_codecs()?;
        
        // Setup API
        let registry = register_default_interceptors(webrtc::interceptor::registry::Registry::new(), &mut m)?;
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

        // Monitor peer connection state changes
        // Bug 5 fix: send InternalCmd::CleanupPeer on failure so the manager
        // removes the dead PC from its peers map and cleans up the output stream.
        let event_tx_for_pc_state = self.event_tx.clone();
        let internal_tx_for_pc_state = self.internal_tx.clone();
        let remote_id_for_pc_state = remote_user_id.clone();
        pc.on_peer_connection_state_change(Box::new(move |state| {
            let tx = event_tx_for_pc_state.clone();
            let itx = internal_tx_for_pc_state.clone();
            let peer_id = remote_id_for_pc_state.clone();
            Box::pin(async move {
                match state {
                    RTCPeerConnectionState::Connected => {
                        let _ = tx.send(VoiceEvent::PeerConnected(peer_id));
                    }
                    RTCPeerConnectionState::Disconnected | RTCPeerConnectionState::Closed => {
                        let _ = tx.send(VoiceEvent::PeerDisconnected(peer_id.clone()));
                        let _ = itx.send(InternalCmd::CleanupPeer(peer_id));
                    }
                    RTCPeerConnectionState::Failed => {
                        let _ = tx.send(VoiceEvent::PeerConnectionFailed(peer_id.clone()));
                        let _ = itx.send(InternalCmd::CleanupPeer(peer_id));
                    }
                    _ => {}
                }
            })
        }));

        // Monitor ICE connection state changes
        pc.on_ice_connection_state_change(Box::new(move |_state| {
            Box::pin(async move {
                // ICE state changes are handled by peer_connection_state_change
            })
        }));

        // Handle Incoming Tracks (Remote Audio)
        // Bug 4 fix: pass peer_id so AudioEngine tracks streams per-peer
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
                if !joined_state.load(Ordering::Relaxed) {
                    return;
                }
                let (packet_tx, packet_rx) = mpsc::unbounded_channel();
                
                // Start playback thread for this track, keyed by peer_id
                {
                    let mut engine = audio_engine.lock().await;
                    if let Err(e) = engine.start_playback_for_peer(&peer_id, packet_rx) {
                        let _ = event_tx.send(VoiceEvent::AudioError(format!("Audio playback failed: {}", e)));
                        return;
                    }
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
        // Set joined flag to false FIRST to stop on_track callbacks
        self.is_joined.store(false, Ordering::Relaxed);
        
        if let Some(_room_id) = &self.room_id {
            // Notify server
            let _ = self.event_tx.send(VoiceEvent::Signal {
                target_id: None,
                signal_type: "leave_voice".to_string(),
                data: "".to_string(),
            });
        }
        
        // Reset audio engine (stops all streams)
        {
            let mut audio = self.audio_engine.lock().await;
            audio.reset();
        }
        
        // Clear local track
        self.local_track = None;
        
        // Clear pending candidates
        self.pending_candidates.clear();
        
        // Close all peers and wait for completion (don't fire-and-forget)
        {
            let mut peers = self.peers.lock().await;
            for (peer_id, pc) in peers.drain() {
                // Close synchronously to ensure cleanup completes
                let _ = pc.close().await;
                let _ = self.event_tx.send(VoiceEvent::PeerDisconnected(peer_id));
            }
        }
        
        // Reset mute state
        self.is_muted.store(false, Ordering::Relaxed);
        
        self.room_id = None;
        
        // Send Disconnected event AFTER all cleanup is complete
        let _ = self.event_tx.send(VoiceEvent::TxActivity(false));
        let _ = self.event_tx.send(VoiceEvent::MuteStateChanged(false));
        let _ = self.event_tx.send(VoiceEvent::Disconnected);
        
        Ok(())
    }

    pub async fn handle_signal(&mut self, sender_id: &str, signal_type: &str, data: &str) -> Result<()> {
        match signal_type {
            "join_voice" => {
                // Remote user joined - existing users initiate connections to the new user
                let _ = self.create_peer_connection(sender_id.to_string(), true).await;
            }
            "offer" => {
                let pc = self.create_peer_connection(sender_id.to_string(), false).await?;
                let desc: RTCSessionDescription = serde_json::from_str(data)?;
                pc.set_remote_description(desc).await?;
                
                // Bug 1 fix: apply any buffered ICE candidates that arrived before the offer
                if let Some(candidates) = self.pending_candidates.remove(sender_id) {
                    for candidate in candidates {
                        let _ = pc.add_ice_candidate(candidate).await;
                    }
                }
                
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
                    // Peer connection exists - add candidate directly
                    let candidate: RTCIceCandidateInit = serde_json::from_str(data)?;
                    pc.add_ice_candidate(candidate).await?;
                } else {
                    // Bug 1 fix: peer connection doesn't exist yet (candidate arrived
                    // before the offer). Buffer it for later application.
                    let candidate: RTCIceCandidateInit = serde_json::from_str(data)?;
                    self.pending_candidates
                        .entry(sender_id.to_string())
                        .or_insert_with(Vec::new)
                        .push(candidate);
                }
            }
            "leave_voice" => {
                let mut peers = self.peers.lock().await;
                if let Some(pc) = peers.remove(sender_id) {
                    let _ = pc.close().await;
                }
                // Bug 4 fix: clean up the output stream for this peer
                {
                    let mut audio = self.audio_engine.lock().await;
                    audio.remove_peer_stream(sender_id);
                }
                self.pending_candidates.remove(sender_id);
                let _ = self.event_tx.send(VoiceEvent::PeerDisconnected(sender_id.to_string()));
            }
            _ => {}
        }
        Ok(())
    }
}
