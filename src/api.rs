use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// --- Payloads Sent from Client to Server ---

#[derive(Serialize)]
pub struct RegisterPayload<'a> {
    pub username: &'a str,
    pub password: &'a str,
}

#[derive(Serialize)]
pub struct LoginPayload<'a> {
    pub username: &'a str,
    pub password: &'a str,
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
pub struct SimpleMessagePayload {
    pub message: String,
}

#[derive(Deserialize, Debug)]
pub struct LoggedInPayload {
    pub token: String,
}

#[derive(Deserialize, Debug)]
pub struct Author {
    pub username: String,
}

#[derive(Deserialize, Debug)]
pub struct HistoryMessage {
    pub content: String,
    #[serde(rename = "createdAt")]
    pub created_at: DateTime<Utc>,
    pub author: Author,
}

#[derive(Deserialize, Debug)]
pub struct HistoryPayload {
    #[serde(rename = "roomId")]
    pub room_id: String,
    pub messages: Vec<HistoryMessage>,
}

// An enum to represent all possible incoming server message payloads
#[derive(Deserialize, Debug)]
#[serde(tag = "type", content = "payload")]
#[serde(rename_all = "camelCase")]
pub enum ServerMessage {
    Registered(SimpleMessagePayload),
    LoggedIn(LoggedInPayload),
    Error(SimpleMessagePayload),
    // Add other server message types here as you implement them
    // e.g., RoomCreated, JoinedRoom, Message, etc.
}
