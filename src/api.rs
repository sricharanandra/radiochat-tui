use serde::{Deserialize, Serialize};

// --- Payloads Sent from Client to Server ---

#[derive(Serialize)]
pub struct JoinRoomPayload<'a> {
    #[serde(rename = "roomId")]
    pub room_id: &'a str,
}

#[derive(Serialize)]
pub struct SendMessagePayload<'a> {
    #[serde(rename = "roomId")]
    pub room_id: &'a str,
    // The encrypted message content, encoded as a hex string
    pub ciphertext: &'a str,
}

// A generic wrapper for all client-sent messages
#[derive(Serialize)]
pub struct ClientMessage<'a, T> {
    #[serde(rename = "type")]
    pub typ: &'a str,
    pub payload: T,
}

// --- Payloads Received from Server ---

#[derive(Deserialize, Debug)]
pub struct MessagePayload {
    // The encrypted message content, encoded as a hex string
    pub ciphertext: String,
}

#[derive(Deserialize, Debug)]
pub struct SimpleMessagePayload {
    pub message: String,
}

// An enum to represent all possible incoming server message payloads
#[derive(Deserialize, Debug)]
#[serde(tag = "type", content = "payload")]
#[serde(rename_all = "camelCase")]
pub enum ServerMessage {
    Message(MessagePayload),
    Info(SimpleMessagePayload),
    Error(SimpleMessagePayload),
}