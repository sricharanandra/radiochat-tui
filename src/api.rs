use serde::{Deserialize, Serialize};

// ============================================================================
// CLIENT → SERVER MESSAGES
// ============================================================================

#[derive(Serialize)]
pub struct JoinRoomPayload<'a> {
    #[serde(rename = "roomId", skip_serializing_if = "Option::is_none")]
    pub room_id: Option<&'a str>,
    #[serde(rename = "roomName", skip_serializing_if = "Option::is_none")]
    pub room_name: Option<&'a str>,
}

#[derive(Serialize)]
pub struct SendMessagePayload<'a> {
    #[serde(rename = "roomId")]
    pub room_id: &'a str,
    pub ciphertext: &'a str,
}

#[derive(Serialize)]
pub struct CreateRoomPayload<'a> {
    pub name: &'a str,
    #[serde(rename = "displayName", skip_serializing_if = "Option::is_none")]
    pub display_name: Option<&'a str>,
    #[serde(rename = "roomType")]
    pub room_type: &'a str, // "public" or "private"
}

#[derive(Serialize)]
pub struct ListRoomsPayload {
    // Empty payload
}

#[derive(Serialize)]
pub struct TypingPayload<'a> {
    #[serde(rename = "roomId")]
    pub room_id: &'a str,
}

#[derive(Serialize)]
pub struct CreateInvitePayload<'a> {
    #[serde(rename = "roomId")]
    pub room_id: &'a str,
}

#[derive(Serialize)]
pub struct JoinViaInvitePayload<'a> {
    pub code: &'a str,
}

#[derive(Serialize)]
pub struct RenameRoomPayload<'a> {
    #[serde(rename = "roomId")]
    pub room_id: &'a str,
    #[serde(rename = "newName")]
    pub new_name: &'a str,
}

#[derive(Serialize)]
pub struct DeleteRoomPayload<'a> {
    #[serde(rename = "roomId")]
    pub room_id: &'a str,
}

#[derive(Serialize)]
pub struct TransferOwnershipPayload<'a> {
    #[serde(rename = "roomId")]
    pub room_id: &'a str,
    #[serde(rename = "newOwnerUsername")]
    pub new_owner_username: &'a str,
}

#[derive(Serialize)]
pub struct CreateDMPayload<'a> {
    #[serde(rename = "targetUsername")]
    pub target_username: &'a str,
}

// Generic wrapper for all client-sent messages
#[derive(Serialize)]
pub struct ClientMessage<'a, T> {
    #[serde(rename = "type")]
    pub message_type: &'a str,
    pub payload: T,
}

// ============================================================================
// SERVER → CLIENT MESSAGES
// ============================================================================

#[derive(Deserialize, Debug, Clone)]
pub struct MessagePayload {
    #[allow(dead_code)]
    pub id: String,
    pub username: String,
    pub ciphertext: String,
    #[allow(dead_code)]
    pub timestamp: String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct UserJoinedPayload {
    pub username: String,
    #[serde(rename = "userId")]
    #[allow(dead_code)]
    pub user_id: String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct UserLeftPayload {
    pub username: String,
    #[serde(rename = "userId")]
    #[allow(dead_code)]
    pub user_id: String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct OnlineUser {
    pub username: String,
    #[serde(rename = "userId")]
    #[allow(dead_code)]
    pub user_id: String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct RoomJoinedPayload {
    #[serde(rename = "roomId")]
    pub room_id: String,
    #[serde(rename = "roomName")]
    pub room_name: String,
    #[serde(rename = "displayName")]
    pub display_name: String,
    #[serde(rename = "roomType")]
    #[allow(dead_code)]
    pub room_type: String,
    #[serde(rename = "encryptedKey")]
    pub encrypted_key: String,
    pub messages: Vec<MessagePayload>,
    #[serde(rename = "onlineUsers", default)]
    pub online_users: Vec<OnlineUser>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct RoomCreatedPayload {
    #[serde(rename = "roomId")]
    pub room_id: String,
    #[serde(rename = "roomName")]
    pub room_name: String,
    #[serde(rename = "displayName")]
    pub display_name: String,
    #[serde(rename = "roomType")]
    #[allow(dead_code)]
    pub room_type: String,
    #[serde(rename = "encryptedKey")]
    pub encrypted_key: String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct RoomInfo {
    #[serde(rename = "roomId")]
    #[allow(dead_code)]
    pub room_id: String,
    pub name: String,
    #[serde(rename = "displayName")]
    pub display_name: String,
    #[serde(rename = "roomType")]
    #[allow(dead_code)]
    pub room_type: String,
    #[serde(rename = "memberCount")]
    pub member_count: usize,
    #[serde(rename = "isJoined")]
    #[allow(dead_code)]
    pub is_joined: bool,
}

#[derive(Deserialize, Debug, Clone)]
pub struct RoomsListPayload {
    #[serde(rename = "publicRooms")]
    pub public_rooms: Vec<RoomInfo>,
    #[serde(rename = "privateRooms")]
    pub private_rooms: Vec<RoomInfo>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct ErrorPayload {
    pub message: String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct InfoPayload {
    pub message: String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct UserTypingPayload {
    pub username: String,
    #[serde(rename = "userId")]
    #[allow(dead_code)]
    pub user_id: String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct InviteCreatedPayload {
    pub code: String,
    #[serde(rename = "roomId")]
    #[allow(dead_code)]
    pub room_id: String,
    #[serde(rename = "roomName")]
    #[allow(dead_code)]
    pub room_name: String,
    #[serde(rename = "expiresAt")]
    #[allow(dead_code)]
    pub expires_at: String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct RoomRenamedPayload {
    #[serde(rename = "roomId")]
    pub room_id: String,
    #[serde(rename = "newName")]
    pub new_name: String,
    #[serde(rename = "displayName")]
    pub display_name: String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct RoomDeletedPayload {
    #[serde(rename = "roomId")]
    pub room_id: String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct OwnershipTransferredPayload {
    #[serde(rename = "roomId")]
    pub room_id: String,
    #[serde(rename = "newOwnerUsername")]
    pub new_owner_username: String,
    #[serde(rename = "newOwnerId")]
    #[allow(dead_code)]
    pub new_owner_id: String,
}

// Enum to represent all possible incoming server messages
#[derive(Deserialize, Debug, Clone)]
#[serde(tag = "type", content = "payload")]
#[serde(rename_all = "camelCase")]
pub enum ServerMessage {
    Message(MessagePayload),
    UserJoined(UserJoinedPayload),
    UserLeft(UserLeftPayload),
    RoomJoined(RoomJoinedPayload),
    RoomCreated(RoomCreatedPayload),
    RoomsList(RoomsListPayload),
    Info(InfoPayload),
    Error(ErrorPayload),
    UserTyping(UserTypingPayload),
    InviteCreated(InviteCreatedPayload),
    RoomRenamed(RoomRenamedPayload),
    RoomDeleted(RoomDeletedPayload),
    OwnershipTransferred(OwnershipTransferredPayload),
}
