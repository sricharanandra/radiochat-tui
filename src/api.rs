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

#[derive(Serialize)]
pub struct CreateRoomPayload<'a> {
    pub token: &'a str,
    pub name: &'a str,
}

#[derive(Serialize)]
pub struct JoinRoomPayload<'a> {
    pub token: &'a str,
    #[serde(rename = "roomId")]
    pub room_id: &'a str,
}

#[derive(Serialize)]
pub struct MessagePayload<'a> {
    pub token: &'a str,
    #[serde(rename = "roomId")]
    pub room_id: &'a str,
    pub content: &'a str,
}

#[derive(Serialize)]
pub struct GetUserRoomsPayload<'a> {
    pub token: &'a str,
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
pub struct RoomCreatedPayload {
    #[serde(rename = "roomId")]
    pub room_id: String,
    pub name: String,
}

#[derive(Deserialize, Debug)]
pub struct JoinedRoomPayload {
    #[serde(rename = "roomId")]
    pub room_id: String,
    pub name: String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct RoomInfo {
    pub id: String,
    pub name: String,
    #[serde(rename = "createdAt")]
    pub created_at: DateTime<Utc>,
}

#[derive(Deserialize, Debug)]
pub struct UserRoomsPayload {
    pub rooms: Vec<RoomInfo>,
}

#[derive(Deserialize, Debug)]
pub struct Author {
    pub username: String,
}

#[derive(Deserialize, Debug)]
pub struct MessageWithAuthor {
    pub content: String,
    #[serde(rename = "createdAt")]
    pub created_at: DateTime<Utc>,
    pub author: Author,
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
    JoinedRoom(JoinedRoomPayload),
    History(HistoryPayload),
    Message(MessageWithAuthor),
    UserJoined(SimpleMessagePayload),
    UserLeft(SimpleMessagePayload),
    JoinRequest(SimpleMessagePayload),
    JoinApproved(JoinedRoomPayload),
    JoinRejected(SimpleMessagePayload),
    JoinRequestSent(SimpleMessagePayload),
    Info(SimpleMessagePayload),
    RoomDeleted(SimpleMessagePayload),
    Error(SimpleMessagePayload),
    RoomCreated(RoomCreatedPayload),
    UserRooms(UserRoomsPayload),
    // Add other server message types here as you implement them
    // e.g., RoomCreated, JoinedRoom, Message, etc.
}
