mod api;
mod crypto;
mod clipboard;
mod config;
mod vim;
mod emoji;
mod voice;

use crate::crypto::{decrypt, encrypt, key_from_hex, AesKey};
use crate::clipboard::ClipboardManager;
use crate::config::Config;
use crate::vim::{VimMode, VimState};
use crate::voice::manager::{VoiceManager, VoiceEvent};
use api::{ClientMessage, CreateRoomPayload, JoinRoomPayload, ListRoomsPayload, SendMessagePayload, ServerMessage, RoomInfo, TypingPayload, CreateInvitePayload, JoinViaInvitePayload, RenameRoomPayload, DeleteRoomPayload, TransferOwnershipPayload, CreateDMPayload, VoiceSignalPayload};
use futures_util::{SinkExt, StreamExt};
use ratatui::{
    crossterm::{
        event::{self, DisableMouseCapture, EnableMouseCapture, EnableFocusChange, Event, KeyCode, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind},
        execute,
        terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    },
    prelude::*,
    widgets::*,
};
use std::{error::Error, io};
use tokio::sync::mpsc;
use tokio::time::{interval, Duration};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tui_textarea::TextArea;
use notify_rust::Notification;

// --- Application State ---

#[derive(PartialEq)]
enum CurrentScreen {
    // Registration flow
    Registration,         // No SSH keys found error
    KeySelection,         // Select SSH key from list
    UsernameInput,        // Enter username
    RegistrationSuccess,  // Show token + success
    
    // Main flow
    RoomChoice,
    RoomList,
    RoomTypeSelection,
    RoomCreation,
    CreateRoomInput,
    InRoom,
    RoomSwitcher,
    Help,
}

enum CurrentlyEditing {
    RoomName,
}

/// A chat message with content and timestamp
#[derive(Clone)]
struct ChatMessage {
    content: String,      // The actual text content
    sender: Option<String>, // Username of sender (None for system messages)
    timestamp: String,    // Formatted time like "2:34 PM"
    date: String,         // Formatted date like "January 24, 2026"
    is_system: bool,      // Whether it is a system message
}

impl ChatMessage {
    fn new(content: String, sender: Option<String>, timestamp: Option<String>) -> Self {
        let (formatted_time, formatted_date) = if let Some(ts) = timestamp {
            // Parse ISO timestamp and format as local time and date
            if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&ts) {
                let local_dt = dt.with_timezone(&chrono::Local);
                (
                    local_dt.format("%I:%M %p").to_string(),
                    local_dt.format("%B %d, %Y").to_string()
                )
            } else {
                let now = chrono::Local::now();
                (
                    now.format("%I:%M %p").to_string(),
                    now.format("%B %d, %Y").to_string()
                )
            }
        } else {
            let now = chrono::Local::now();
            (
                now.format("%I:%M %p").to_string(),
                now.format("%B %d, %Y").to_string()
            )
        };
        
        Self {
            content,
            sender,
            timestamp: formatted_time,
            date: formatted_date,
            is_system: false,
        }
    }
    
    fn system(content: String) -> Self {
        let now = chrono::Local::now();
        Self {
            content,
            sender: None,
            timestamp: now.format("%I:%M %p").to_string(),
            date: now.format("%B %d, %Y").to_string(),
            is_system: true,
        }
    }
}

struct App<'a> {
    // Inputs
    room_name_input: TextArea<'a>,
    message_input: TextArea<'a>,

    // State
    current_screen: CurrentScreen,
    currently_editing: Option<CurrentlyEditing>,
    status_message: String,
    should_quit: bool,
    vim_state: VimState,
    message_scroll_offset: usize,
    current_username: Option<String>,

    // Room Data
    room_id: Option<String>,
    room_name: Option<String>,
    room_display_name: Option<String>,
    room_key: Option<AesKey>,
    messages: Vec<ChatMessage>,
    online_users: Vec<String>,  // Usernames of online users in current room
    
    // Typing indicators (username -> timestamp when they started typing)
    typing_users: std::collections::HashMap<String, std::time::Instant>,
    last_typing_sent: Option<std::time::Instant>,
    
    // UI State
    show_user_list: bool,  // Show user list overlay
    is_focused: bool,      // Is terminal focused?
    
    // Room List
    public_rooms: Vec<RoomInfo>,
    private_rooms: Vec<RoomInfo>,
    selected_room_index: usize,
    viewing_private: bool,
    
    // Room Creation
    selected_room_type: bool,  // false = public, true = private
    
    // Room Switcher
    user_rooms: Vec<RoomInfo>,  // Rooms user is a member of
    switcher_selected_index: usize,

    // WebSocket
    ws_sender: Option<mpsc::UnboundedSender<String>>,
    reconnect_attempts: usize,
    is_reconnecting: bool,
    
    // Clipboard & Config
    clipboard: Option<ClipboardManager>,
    config: Config,
    
    // Command Mode
    command_input: Option<String>,
    
    // Registration
    available_keys: Vec<(String, String)>,  // (path, content)
    selected_key_index: usize,
    username_input: TextArea<'a>,
    registration_token: Option<String>,
    registration_error: Option<String>,
    
    // Emoji Picker
    emoji_picker_active: bool,
    emoji_partial: String,
    emoji_matches: Vec<(&'static str, &'static str, &'static str)>,
    emoji_selected_index: usize,

    // Voice Chat
    voice_tx: Option<mpsc::UnboundedSender<voice::manager::VoiceCommand>>,
    voice_active: bool,
    voice_users: Vec<String>,
}

impl<'a> Default for App<'a> {
    fn default() -> Self {
        let mut room_name_input = TextArea::default();
        room_name_input.set_placeholder_text("Enter room name (e.g., general)...");
        room_name_input.set_block(Block::default().borders(Borders::ALL).title("Room Name"));

        let mut message_input = TextArea::default();
        message_input.set_placeholder_text("Type your encrypted message...");
        message_input.set_block(Block::default().borders(Borders::ALL).title("Message"));

        let clipboard = ClipboardManager::new().ok();
        if clipboard.is_none() {
            eprintln!("Warning: Failed to initialize clipboard");
        }

        App {
            room_name_input,
            message_input,
            current_screen: CurrentScreen::RoomChoice,
            currently_editing: None,
            status_message: "Create or Join a secure room.".to_string(),
            should_quit: false,
            current_username: None,
            room_id: None,
            room_name: None,
            room_display_name: None,
            room_key: None,
            messages: Vec::new(),
            online_users: Vec::new(),
            typing_users: std::collections::HashMap::new(),
            last_typing_sent: None,
            show_user_list: false,
            is_focused: true, // Assume focused initially
            public_rooms: Vec::new(),
            private_rooms: Vec::new(),
            selected_room_index: 0,
            viewing_private: false,
            selected_room_type: false,  // false = public, true = private
            user_rooms: Vec::new(),
            switcher_selected_index: 0,
            ws_sender: None,
            reconnect_attempts: 0,
            is_reconnecting: false,
            clipboard,
            config: Config::load(),
            vim_state: VimState::default(),
            message_scroll_offset: 0,
            command_input: None,
            available_keys: Vec::new(),
            selected_key_index: 0,
            username_input: {
                let mut input = TextArea::default();
                input.set_placeholder_text("Enter your username...");
                input.set_block(Block::default().borders(Borders::ALL).title("Username"));
                input
            },
            registration_token: None,
            registration_error: None,
            
            emoji_picker_active: false,
            emoji_matches: Vec::new(),
            emoji_selected_index: 0,
            emoji_partial: String::new(),
            voice_tx: None,
            voice_active: false,
            voice_users: Vec::new(),
        }
    }
}

// --- Main Application Logic ---

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let mut terminal = init_terminal()?;
    let mut app = App::default();
    run_app(&mut terminal, &mut app).await?;
    restore_terminal(&mut terminal)?;
    Ok(())
}

async fn run_app<B: Backend>(terminal: &mut Terminal<B>, app: &mut App<'_>) -> io::Result<()> {
    let (ws_incoming_tx, mut ws_incoming_rx) = mpsc::unbounded_channel::<String>();

    // Setup Voice Manager
    let (voice_cmd_tx, voice_cmd_rx) = mpsc::unbounded_channel::<voice::manager::VoiceCommand>();
    let (voice_event_tx, mut voice_event_rx) = mpsc::unbounded_channel::<voice::manager::VoiceEvent>();
    app.voice_tx = Some(voice_cmd_tx);
    
    // Spawn Voice Manager Task
    tokio::spawn(async move {
        let mut manager = VoiceManager::new(voice_event_tx);
        manager.run(voice_cmd_rx).await;
    });

    // Check if user is registered (has auth token)
    let token = load_auth_token(&app.config.auth.token_path);
    let token_exists = token.is_some();
    
    if let Some(t) = &token {
        app.current_username = extract_username_from_token(t);
    }
    
    if !token_exists {
        // No token found - need to register
        app.available_keys = scan_ssh_keys();
        if app.available_keys.is_empty() {
            app.current_screen = CurrentScreen::Registration;
            app.status_message = "No SSH keys found. Create one to register.".to_string();
        } else {
            app.current_screen = CurrentScreen::KeySelection;
            app.selected_key_index = 0;
            app.status_message = "Welcome! Select an SSH key to register.".to_string();
        }
    } else {
        // Token exists - establish WebSocket connection
        if let Ok(()) = establish_connection(app, ws_incoming_tx.clone()).await {
            app.status_message = "Connected! Create or Join a secure room.".to_string();
        } else {
            app.status_message = "Failed to connect to server. Check if server is running.".to_string();
        }
    }

    loop {
        terminal.draw(|f| ui(f, app))?;

        // Clear expired typing indicators (older than 3 seconds)
        app.typing_users.retain(|_, timestamp| {
            timestamp.elapsed() < std::time::Duration::from_secs(3)
        });

        if app.should_quit {
            break;
        }

        // Establish connection after registration completes
        if app.current_screen == CurrentScreen::RoomChoice 
            && app.ws_sender.is_none() 
            && load_auth_token(&app.config.auth.token_path).is_some() 
        {
            if let Ok(()) = establish_connection(app, ws_incoming_tx.clone()).await {
                app.status_message = "Connected! Create or Join a secure room.".to_string();
            } else {
                app.status_message = "Failed to connect to server.".to_string();
            }
        }

        // Handle incoming WebSocket messages without blocking UI
        if let Ok(text) = ws_incoming_rx.try_recv() {
            // Check for disconnect signal
            if text == "__DISCONNECT__" {
                app.messages.push(ChatMessage::system("[SYSTEM] Connection lost. Attempting to reconnect...".to_string()));
                app.ws_sender = None;
                
                // Attempt reconnection in background
                if app.current_screen == CurrentScreen::InRoom {
                    if let Some(room_id) = app.room_id.clone() {
                        // Try to reconnect and rejoin the room
                        match establish_connection(app, ws_incoming_tx.clone()).await {
                            Ok(_) => {
                                // Rejoin the room after reconnection
                                if let Some(sender) = &app.ws_sender {
                                    let join_payload = JoinRoomPayload {
                                        room_id: Some(&room_id),
                                        room_name: None,
                                    };
                                    let join_msg = ClientMessage {
                                        message_type: "joinRoom",
                                        payload: join_payload,
                                    };
                                    if let Ok(json) = serde_json::to_string(&join_msg) {
                                        let _ = sender.send(json);
                                    }
                                }
                                app.messages.push(ChatMessage::system("[SYSTEM] Reconnected successfully!".to_string()));
                            }
                            Err(_) => {
                                app.messages.push(ChatMessage::system("[SYSTEM] Failed to reconnect. Please restart.".to_string()));
                            }
                        }
                    }
                }
            } else {
                match serde_json::from_str::<ServerMessage>(&text) {
                    Ok(server_msg) => handle_server_message(app, server_msg),
                    Err(_) => {
                        // Raw info messages from the server
                        app.messages.push(ChatMessage::system(format!("[SERVER] {}", text)));
                    }
                };
            }
        }

        // Handle Voice Events
        if let Ok(event) = voice_event_rx.try_recv() {
            match event {
                VoiceEvent::Signal { target_id, signal_type, data } => {
                    // Send this signal to the server via WebSocket
                    if let (Some(ws_sender), Some(room_id)) = (&app.ws_sender, &app.room_id) {
                        let payload = VoiceSignalPayload {
                            room_id: room_id.clone(),
                            target_user_id: target_id,
                            sender_user_id: None, // Server fills this
                            sender_username: None,
                            signal_type,
                            data,
                        };
                        let msg = ClientMessage {
                            message_type: "voiceSignal",
                            payload,
                        };
                        if let Ok(json) = serde_json::to_string(&msg) {
                            let _ = ws_sender.send(json);
                        }
                    }
                }
                VoiceEvent::StatusUpdate(msg) => {
                    app.status_message = format!("VOICE: {}", msg);
                }
                VoiceEvent::Error(e) => {
                    app.status_message = format!("VOICE ERROR: {}", e);
                }
            }
        }

        // Handle user input
        if event::poll(std::time::Duration::from_millis(50))? {
            let event = event::read()?;
            match event {
                Event::FocusGained => {
                    app.is_focused = true;
                }
                Event::FocusLost => {
                    app.is_focused = false;
                }
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    // Handle command mode input
                    if app.command_input.is_some() {
                        match key.code {
                            KeyCode::Esc => {
                                // Cancel command mode
                                app.command_input = None;
                            }
                            KeyCode::Enter => {
                                // Execute command
                                if let Some(cmd) = app.command_input.take() {
                                    execute_command(app, &cmd).await;
                                }
                            }
                            KeyCode::Backspace => {
                                // Delete last character
                                if let Some(ref mut cmd) = app.command_input {
                                    cmd.pop();
                                }
                            }
                            KeyCode::Char(c) => {
                                // Append character
                                if let Some(ref mut cmd) = app.command_input {
                                    cmd.push(c);
                                }
                            }
                            _ => {}
                        }
                        continue;
                    }
                    
                    match app.current_screen {
                        // Registration screens
                        CurrentScreen::Registration => handle_registration_screen(app, key).await,
                        CurrentScreen::KeySelection => handle_key_selection_screen(app, key).await,
                        CurrentScreen::UsernameInput => handle_username_input_screen(app, key).await,
                        CurrentScreen::RegistrationSuccess => handle_registration_success_screen(app, key).await,
                        
                        // Main screens
                        CurrentScreen::RoomChoice => handle_room_choice_screen(app, key).await,
                        CurrentScreen::RoomList => handle_room_list_screen(app, key, ws_incoming_tx.clone()).await,
                        CurrentScreen::RoomTypeSelection => handle_room_type_selection_screen(app, key).await,
                        CurrentScreen::CreateRoomInput => {
                            handle_create_room_input_screen(app, key, ws_incoming_tx.clone()).await
                        }
                        CurrentScreen::RoomCreation => {
                            handle_room_creation_screen(app, key, ws_incoming_tx.clone()).await
                        }
                        CurrentScreen::InRoom => handle_in_room_screen(app, key).await,
                        CurrentScreen::RoomSwitcher => handle_room_switcher_screen(app, key).await,
                        CurrentScreen::Help => handle_help_screen(app, key),
                    }
                }
                Event::Mouse(mouse_event) => {
                    // Handle mouse events (selection, scrolling, etc.)
                    if app.current_screen == CurrentScreen::InRoom {
                        handle_mouse_in_room(app, mouse_event);
                    }
                }
                _ => {}
            }
        }
    }
    Ok(())
}

// --- Screen & Input Handlers ---

// Registration screen handlers

async fn handle_registration_screen(app: &mut App<'_>, key: event::KeyEvent) {
    // This screen shows when no SSH keys are found
    match key.code {
        KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => {
            app.should_quit = true;
        }
        KeyCode::Char(':') => {
            app.command_input = Some(String::new());
        }
        KeyCode::Char('r') | KeyCode::Char('R') => {
            // Retry scanning for keys
            app.available_keys = scan_ssh_keys();
            if !app.available_keys.is_empty() {
                app.current_screen = CurrentScreen::KeySelection;
                app.selected_key_index = 0;
                app.status_message = "Select SSH key to use for registration".to_string();
            } else {
                app.status_message = "Still no SSH keys found. Create one first.".to_string();
            }
        }
        _ => {}
    }
}

async fn handle_key_selection_screen(app: &mut App<'_>, key: event::KeyEvent) {
    match key.code {
        KeyCode::Up | KeyCode::Char('k') => {
            if app.selected_key_index > 0 {
                app.selected_key_index -= 1;
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if app.selected_key_index + 1 < app.available_keys.len() {
                app.selected_key_index += 1;
            }
        }
        KeyCode::Enter => {
            // Proceed to username input
            app.current_screen = CurrentScreen::UsernameInput;
            app.status_message = "Enter your desired username".to_string();
        }
        KeyCode::Char(':') => {
            app.command_input = Some(String::new());
        }
        KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => {
            app.should_quit = true;
        }
        _ => {}
    }
}

async fn handle_username_input_screen(app: &mut App<'_>, key: event::KeyEvent) {
    match key.code {
        KeyCode::Enter => {
            let username = app.username_input.lines().join("").trim().to_string();
            if username.is_empty() {
                app.status_message = "Username cannot be empty".to_string();
                return;
            }
            
            // Get selected key
            if let Some((_, public_key)) = app.available_keys.get(app.selected_key_index) {
                let public_key = public_key.clone();
                
                // Perform registration
                app.status_message = "Registering...".to_string();
                
                match register_user(&app.config.server.url, &username, &public_key).await {
                    Ok(token) => {
                        // Save token
                        if let Err(e) = save_auth_token(&token) {
                            app.registration_error = Some(format!("Failed to save token: {}", e));
                        }
                        app.current_username = extract_username_from_token(&token);
                        app.registration_token = Some(token);
                        app.current_screen = CurrentScreen::RegistrationSuccess;
                        app.status_message = "Registration successful!".to_string();
                    }
                    Err(e) => {
                        app.registration_error = Some(e.to_string());
                        app.status_message = format!("Registration failed: {}", e);
                    }
                }
            }
        }
        KeyCode::Esc => {
            app.current_screen = CurrentScreen::KeySelection;
            app.status_message = "Select SSH key to use for registration".to_string();
        }
        KeyCode::Char(':') => {
            app.command_input = Some(String::new());
        }
        _ => {
            app.username_input.input(Event::Key(key));
        }
    }
}

async fn handle_registration_success_screen(app: &mut App<'_>, key: event::KeyEvent) {
    match key.code {
        KeyCode::Enter => {
            // Continue to main app
            app.current_screen = CurrentScreen::RoomChoice;
            app.status_message = "Create or Join a secure room.".to_string();
        }
        KeyCode::Char(':') => {
            app.command_input = Some(String::new());
        }
        _ => {}
    }
}

async fn handle_room_choice_screen(app: &mut App<'_>, key: event::KeyEvent) {
    match key.code {
        KeyCode::Char('c') | KeyCode::Char('C') => {
            app.current_screen = CurrentScreen::RoomTypeSelection;
            app.selected_room_type = false;  // Default to public
            app.status_message = "Select room type: Tab to switch, Enter to continue".to_string();
        }
        KeyCode::Char('j') | KeyCode::Char('J') => {
            app.current_screen = CurrentScreen::RoomList;
            app.status_message = "Loading rooms...".to_string();
            
            // Request room list
            if let Some(ws_sender) = &app.ws_sender {
                let list_message = ClientMessage {
                    message_type: "listRooms",
                    payload: ListRoomsPayload {},
                };
                if let Ok(json) = serde_json::to_string(&list_message) {
                    let _ = ws_sender.send(json);
                }
            }
        }
        KeyCode::Char(':') => {
            app.command_input = Some(String::new());
        }
        _ => {}
    }
}

async fn handle_room_type_selection_screen(app: &mut App<'_>, key: event::KeyEvent) {
    match key.code {
        KeyCode::Tab => {
            // Toggle between public and private
            app.selected_room_type = !app.selected_room_type;
            let type_str = if app.selected_room_type { "Private" } else { "Public" };
            app.status_message = format!("Room type: {} - Tab to switch, Enter to continue", type_str);
        }
        KeyCode::Enter => {
            // Continue to room name input
            app.current_screen = CurrentScreen::CreateRoomInput;
            app.currently_editing = Some(CurrentlyEditing::RoomName);
            let type_str = if app.selected_room_type { "private" } else { "public" };
            app.status_message = format!("Creating {} room - enter a name", type_str);
        }
        KeyCode::Esc => {
            // Go back to main menu
            app.current_screen = CurrentScreen::RoomChoice;
            app.status_message = "Create or Join a secure room.".to_string();
        }
        KeyCode::Char(':') => {
            app.command_input = Some(String::new());
        }
        _ => {}
    }
}

async fn handle_room_list_screen(
    app: &mut App<'_>,
    key: event::KeyEvent,
    _ws_tx: mpsc::UnboundedSender<String>,
) {
    match key.code {
        KeyCode::Up => {
            if app.selected_room_index > 0 {
                app.selected_room_index -= 1;
            }
        }
        KeyCode::Down => {
            let room_count = if app.viewing_private {
                app.private_rooms.len()
            } else {
                app.public_rooms.len()
            };
            if app.selected_room_index + 1 < room_count {
                app.selected_room_index += 1;
            }
        }
        KeyCode::Tab => {
            app.viewing_private = !app.viewing_private;
            app.selected_room_index = 0;
        }
        KeyCode::Enter => {
            let room_name = if app.viewing_private {
                app.private_rooms.get(app.selected_room_index).map(|r| r.name.clone())
            } else {
                app.public_rooms.get(app.selected_room_index).map(|r| r.name.clone())
            };
            
            if let Some(name) = room_name {
                app.status_message = format!("Joining room: {}", name);
                
                // Join room using existing WebSocket connection
                if let Some(ws_sender) = &app.ws_sender {
                    let join_payload = JoinRoomPayload {
                        room_id: None,
                        room_name: Some(&name),
                    };
                    let join_message = ClientMessage {
                        message_type: "joinRoom",
                        payload: join_payload,
                    };
                    if let Ok(json) = serde_json::to_string(&join_message) {
                        if ws_sender.send(json).is_ok() {
                            app.messages.clear();
                            app.current_screen = CurrentScreen::InRoom;
                        }
                    }
                }
            }
        }
        KeyCode::Char('r') | KeyCode::Char('R') => {
            // Refresh room list
            if let Some(ws_sender) = &app.ws_sender {
                let list_message = ClientMessage {
                    message_type: "listRooms",
                    payload: ListRoomsPayload {},
                };
                if let Ok(json) = serde_json::to_string(&list_message) {
                    let _ = ws_sender.send(json);
                }
            }
        }
        KeyCode::Esc => {
            app.current_screen = CurrentScreen::RoomChoice;
        }
        // Vim-style navigation
        KeyCode::Char('j') => {
            let room_count = if app.viewing_private {
                app.private_rooms.len()
            } else {
                app.public_rooms.len()
            };
            if app.selected_room_index + 1 < room_count {
                app.selected_room_index += 1;
            }
        }
        KeyCode::Char('k') => {
            if app.selected_room_index > 0 {
                app.selected_room_index -= 1;
            }
        }
        KeyCode::Char(':') => {
            app.command_input = Some(String::new());
        }
        _ => {}
    }
}

async fn handle_create_room_input_screen(
    app: &mut App<'_>,
    key: event::KeyEvent,
    _ws_tx: mpsc::UnboundedSender<String>,
) {
    match key.code {
        KeyCode::Enter => {
            let room_name = app.room_name_input.lines().join("");
            if room_name.is_empty() {
                app.status_message = "Room name cannot be empty!".to_string();
                return;
            }
            
            app.status_message = "Creating room...".to_string();
            
            // Create room using existing WebSocket connection
            if let Some(ws_sender) = &app.ws_sender {
                let room_type_str = if app.selected_room_type { "private" } else { "public" };
                let create_payload = CreateRoomPayload {
                    name: &room_name,
                    display_name: None,
                    room_type: room_type_str,
                };
                let create_message = ClientMessage {
                    message_type: "createRoom",
                    payload: create_payload,
                };
                if let Ok(json) = serde_json::to_string(&create_message) {
                    if ws_sender.send(json).is_ok() {
                        app.current_screen = CurrentScreen::RoomCreation;
                    }
                }
            }
        }
        KeyCode::Esc => {
            app.current_screen = CurrentScreen::RoomChoice;
        }
        KeyCode::Char(':') => {
            app.command_input = Some(String::new());
        }
        _ => {
            app.room_name_input.input(key);
        }
    }
}

async fn handle_room_creation_screen(
    app: &mut App<'_>,
    key: event::KeyEvent,
    _ws_tx: mpsc::UnboundedSender<String>,
) {
    match key.code {
        KeyCode::Enter => {
            if let Some(room_name) = &app.room_name {
                // Join the room we just created using joinRoom message
                if let Some(ws_sender) = &app.ws_sender {
                    let join_payload = JoinRoomPayload {
                        room_id: None,
                        room_name: Some(room_name),
                    };
                    let join_message = ClientMessage {
                        message_type: "joinRoom",
                        payload: join_payload,
                    };
                    if let Ok(json) = serde_json::to_string(&join_message) {
                        if ws_sender.send(json).is_ok() {
                            app.messages.clear();
                            app.current_screen = CurrentScreen::InRoom;
                            app.status_message = format!("Joined room: #{}", room_name);
                        }
                    }
                }
            } else {
                // If no room was created (e.g. error), Enter should just go back
                app.current_screen = CurrentScreen::RoomChoice;
                app.status_message = "Create or Join a secure room.".to_string();
            }
        }
        KeyCode::Esc => {
            // Go back to main menu
            app.current_screen = CurrentScreen::RoomChoice;
            app.status_message = "Create or Join a secure room.".to_string();
        }
        KeyCode::Char('q') => {
             // Go back to main menu
            app.current_screen = CurrentScreen::RoomChoice;
            app.status_message = "Create or Join a secure room.".to_string();
        }
        KeyCode::Char(':') => {
            app.command_input = Some(String::new());
        }
        _ => {}
    }
}

async fn handle_in_room_screen(app: &mut App<'_>, key: event::KeyEvent) {
    // Handle clipboard keybindings (work in any mode)
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::SHIFT) => {
                if let Some(clipboard) = &mut app.clipboard {
                    let text = app.message_input.lines().join("\n");
                    if !text.is_empty() {
                        match clipboard.copy_text(&text) {
                            Ok(_) => app.status_message = "Text copied to clipboard".to_string(),
                            Err(e) => app.status_message = format!("Failed to copy: {}", e),
                        }
                    }
                }
                return;
            }
            KeyCode::Char('v') if key.modifiers.contains(KeyModifiers::SHIFT) => {
                if app.vim_state.mode == VimMode::Insert {
                    if let Some(clipboard) = &mut app.clipboard {
                        match clipboard.paste_text() {
                            Ok(text) => {
                                for line in text.lines() {
                                    app.message_input.insert_str(line);
                                    app.message_input.insert_newline();
                                }
                                app.status_message = "Text pasted from clipboard".to_string();
                            }
                            Err(e) => app.status_message = format!("Failed to paste: {}", e),
                        }
                    }
                }
                return;
            }
            KeyCode::Char('v') => {
                if let Some(clipboard) = &mut app.clipboard {
                    if clipboard.has_image() {
                        app.status_message = "Image paste not yet implemented".to_string();
                    } else {
                        app.status_message = "No image in clipboard".to_string();
                    }
                }
                return;
            }
            _ => {}
        }
    }

    // Vim mode handling
    match app.vim_state.mode {
        VimMode::Normal => handle_normal_mode(app, key).await,
        VimMode::Insert => handle_insert_mode(app, key).await,
    }
}

async fn handle_normal_mode(app: &mut App<'_>, key: event::KeyEvent) {
    // Close user list overlay if open
    if app.show_user_list {
        if key.code == KeyCode::Esc {
            app.show_user_list = false;
            app.status_message = "-- NORMAL --".to_string();
            return;
        }
    }
    
    match key.code {
        // Enter Insert mode
        KeyCode::Char('i') => {
            app.vim_state.enter_insert_mode();
            app.status_message = "-- INSERT --".to_string();
        }
        KeyCode::Char('a') => {
            app.vim_state.enter_insert_mode();
            // Move cursor right by one (append after cursor)
            app.message_input.move_cursor(tui_textarea::CursorMove::Forward);
            app.status_message = "-- INSERT --".to_string();
        }
        KeyCode::Char('A') => {
            app.vim_state.enter_insert_mode();
            app.message_input.move_cursor(tui_textarea::CursorMove::End);
            app.status_message = "-- INSERT --".to_string();
        }
        KeyCode::Char('I') => {
            app.vim_state.enter_insert_mode();
            app.message_input.move_cursor(tui_textarea::CursorMove::Head);
            app.status_message = "-- INSERT --".to_string();
        }
        KeyCode::Char('o') => {
            app.vim_state.enter_insert_mode();
            app.message_input.move_cursor(tui_textarea::CursorMove::End);
            app.message_input.insert_newline();
            app.status_message = "-- INSERT --".to_string();
        }
        KeyCode::Char('O') => {
            app.vim_state.enter_insert_mode();
            app.message_input.move_cursor(tui_textarea::CursorMove::Head);
            app.message_input.insert_newline();
            app.message_input.move_cursor(tui_textarea::CursorMove::Up);
            app.status_message = "-- INSERT --".to_string();
        }

        // Navigation (hjkl)
        KeyCode::Char('h') | KeyCode::Left => {
            app.message_input.move_cursor(tui_textarea::CursorMove::Back);
        }
        KeyCode::Char('j') | KeyCode::Down => {
            app.message_input.move_cursor(tui_textarea::CursorMove::Down);
        }
        KeyCode::Char('k') | KeyCode::Up => {
            app.message_input.move_cursor(tui_textarea::CursorMove::Up);
        }
        KeyCode::Char('l') | KeyCode::Right => {
            app.message_input.move_cursor(tui_textarea::CursorMove::Forward);
        }

        // Word movement
        KeyCode::Char('w') => {
            app.message_input.move_cursor(tui_textarea::CursorMove::WordForward);
        }
        KeyCode::Char('b') => {
            app.message_input.move_cursor(tui_textarea::CursorMove::WordBack);
        }

        // Line movement
        KeyCode::Char('0') => {
            app.message_input.move_cursor(tui_textarea::CursorMove::Head);
        }
        KeyCode::Char('$') => {
            app.message_input.move_cursor(tui_textarea::CursorMove::End);
        }

        // Document movement
        KeyCode::Char('g') => {
            if app.vim_state.pending_command == Some('g') {
                // gg - go to top
                app.message_input.move_cursor(tui_textarea::CursorMove::Top);
                app.vim_state.reset();
            } else {
                app.vim_state.pending_command = Some('g');
            }
        }
        KeyCode::Char('G') => {
            app.message_input.move_cursor(tui_textarea::CursorMove::Bottom);
        }

        // Delete operations
        KeyCode::Char('d') => {
            if app.vim_state.pending_command == Some('d') {
                // dd - delete line
                app.message_input.delete_line_by_head();
                app.vim_state.reset();
            } else {
                app.vim_state.pending_command = Some('d');
            }
        }
        KeyCode::Char('x') => {
            app.message_input.delete_next_char();
        }

        // Yank (copy)
        KeyCode::Char('y') => {
            if app.vim_state.pending_command == Some('y') {
                // yy - yank line
                if let Some(clipboard) = &mut app.clipboard {
                    let (row, _) = app.message_input.cursor();
                    if let Some(line) = app.message_input.lines().get(row) {
                        let _ = clipboard.copy_text(line);
                        app.status_message = "Line yanked".to_string();
                    }
                }
                app.vim_state.reset();
            } else {
                app.vim_state.pending_command = Some('y');
            }
        }

        // Paste
        KeyCode::Char('p') => {
            if let Some(clipboard) = &mut app.clipboard {
                if let Ok(text) = clipboard.paste_text() {
                    for line in text.lines() {
                        app.message_input.insert_str(line);
                    }
                }
            }
        }

        // Undo/Redo
        KeyCode::Char('u') => {
            app.message_input.undo();
        }
        KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.message_input.redo();
        }

        // Send message (Enter in Normal mode)
        KeyCode::Enter => {
            send_message(app).await;
        }

        // Enter command mode
        KeyCode::Char(':') => {
            app.command_input = Some(String::new());
        }

        _ => {
            // Clear any pending commands
            app.vim_state.reset();
        }
    }
}

async fn handle_insert_mode(app: &mut App<'_>, key: event::KeyEvent) {
    // If emoji picker is active, handle picker navigation
    if app.emoji_picker_active {
        match key.code {
            KeyCode::Esc => {
                // Close emoji picker
                app.emoji_picker_active = false;
                app.emoji_matches.clear();
                app.emoji_partial.clear();
                app.emoji_selected_index = 0;
            }
            KeyCode::Enter | KeyCode::Tab => {
                // Select the current emoji
                if let Some((_, emoji, _)) = app.emoji_matches.get(app.emoji_selected_index) {
                    // Delete the partial shortcode (including the leading :)
                    let delete_count = app.emoji_partial.len() + 1; // +1 for the ':'
                    for _ in 0..delete_count {
                        app.message_input.delete_char();
                    }
                    // Insert the emoji
                    app.message_input.insert_str(*emoji);
                }
                // Close picker
                app.emoji_picker_active = false;
                app.emoji_matches.clear();
                app.emoji_partial.clear();
                app.emoji_selected_index = 0;
            }
            KeyCode::Up => {
                if app.emoji_selected_index > 0 {
                    app.emoji_selected_index -= 1;
                }
            }
            KeyCode::Down => {
                if app.emoji_selected_index + 1 < app.emoji_matches.len() {
                    app.emoji_selected_index += 1;
                }
            }
            KeyCode::Char(c) => {
                // Continue typing the shortcode
                app.emoji_partial.push(c);
                app.emoji_matches = emoji::find_matching_emojis(&app.emoji_partial);
                app.emoji_selected_index = 0;
                // If no matches, close picker
                if app.emoji_matches.is_empty() {
                    app.emoji_picker_active = false;
                    app.emoji_partial.clear();
                }
                // Also pass to input
                app.message_input.input(Event::Key(key));
            }
            KeyCode::Backspace => {
                if !app.emoji_partial.is_empty() {
                    app.emoji_partial.pop();
                    if app.emoji_partial.is_empty() {
                        // User deleted back to just ':', close picker
                        app.emoji_picker_active = false;
                        app.emoji_matches.clear();
                    } else {
                        app.emoji_matches = emoji::find_matching_emojis(&app.emoji_partial);
                        app.emoji_selected_index = 0;
                    }
                }
                app.message_input.input(Event::Key(key));
            }
            _ => {
                // Any other key closes the picker and passes through
                app.emoji_picker_active = false;
                app.emoji_matches.clear();
                app.emoji_partial.clear();
                app.emoji_selected_index = 0;
                app.message_input.input(Event::Key(key));
            }
        }
        return;
    }
    
    // Normal insert mode handling
    match key.code {
        KeyCode::Esc => {
            // Exit Insert mode back to Normal mode
            app.vim_state.enter_normal_mode();
            app.status_message = "-- NORMAL --".to_string();
        }
        KeyCode::Enter => {
            // Shift+Enter inserts a newline, plain Enter sends the message
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                // Insert newline for multi-line messages
                app.message_input.insert_newline();
            } else {
                // Send the message
                send_message(app).await;
            }
        }
        KeyCode::Char(':') => {
            // Start emoji picker detection
            app.message_input.input(Event::Key(key));
            app.emoji_picker_active = true;
            app.emoji_partial.clear();
            app.emoji_matches.clear();
            app.emoji_selected_index = 0;
            send_typing_indicator(app);
        }
        _ => {
            // Pass all other keys to the text input
            app.message_input.input(Event::Key(key));
            // Send typing indicator (debounced)
            if matches!(key.code, KeyCode::Char(_) | KeyCode::Backspace) {
                send_typing_indicator(app);
            }
        }
    }
}

async fn send_message(app: &mut App<'_>) {
    let content = app.message_input.lines().join("\n");
    if !content.is_empty() {
        if let (Some(sender), Some(key), Some(room_id)) =
            (&app.ws_sender, &app.room_key, &app.room_id)
        {
            match encrypt(key, content.as_bytes()) {
                Ok(ciphertext) => {
                    let payload = SendMessagePayload {
                        room_id,
                        ciphertext: &ciphertext,
                    };
                    let msg = ClientMessage {
                        message_type: "sendMessage",
                        payload,
                    };
                    if let Ok(json) = serde_json::to_string(&msg) {
                        if sender.send(json).is_ok() {
                            app.message_input = TextArea::default();
                            app.message_input.set_placeholder_text("Type your encrypted message...");
                            app.message_input.set_block(
                                Block::default().borders(Borders::ALL).title("Message"),
                            );
                            // Reset emoji picker state
                            app.emoji_picker_active = false;
                            app.emoji_matches.clear();
                            app.emoji_partial.clear();
                            app.emoji_selected_index = 0;
                            // Stay in current vim mode after sending
                            if app.vim_state.mode == VimMode::Normal {
                                app.status_message = "-- NORMAL --".to_string();
                            } else {
                                app.status_message = "-- INSERT --".to_string();
                            }
                        } else {
                            app.status_message = "Connection lost. Restart to reconnect.".to_string();
                        }
                    }
                }
                Err(_) => {
                    app.status_message = "FATAL: Failed to encrypt message.".to_string();
                }
            }
        } else {
            app.status_message = "Error: Not connected to a room or missing encryption key.".to_string();
        }
    }
}

fn send_typing_indicator(app: &mut App<'_>) {
    // Debounce: only send typing event every 2 seconds
    let should_send = match app.last_typing_sent {
        Some(last) => last.elapsed() > std::time::Duration::from_secs(2),
        None => true,
    };
    
    if should_send {
        if let (Some(sender), Some(room_id)) = (&app.ws_sender, &app.room_id) {
            let payload = TypingPayload { room_id };
            let msg = ClientMessage {
                message_type: "typing",
                payload,
            };
            if let Ok(json) = serde_json::to_string(&msg) {
                let _ = sender.send(json);
            }
            app.last_typing_sent = Some(std::time::Instant::now());
        }
    }
}

async fn handle_room_switcher_screen(app: &mut App<'_>, key: event::KeyEvent) {
    match key.code {
        KeyCode::Esc => {
            // Close switcher, return to room
            app.current_screen = CurrentScreen::InRoom;
            app.status_message = "-- NORMAL --".to_string();
        }
        KeyCode::Up | KeyCode::Char('k') => {
            if app.switcher_selected_index > 0 {
                app.switcher_selected_index -= 1;
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            if app.switcher_selected_index + 1 < app.user_rooms.len() {
                app.switcher_selected_index += 1;
            }
        }
        KeyCode::Enter => {
            // Switch to selected room
            if let Some(room) = app.user_rooms.get(app.switcher_selected_index) {
                let room_name = room.name.clone();
                
                // Join the new room
                if let Some(ws_sender) = &app.ws_sender {
                    let join_payload = JoinRoomPayload {
                        room_id: None,
                        room_name: Some(&room_name),
                    };
                    let join_message = ClientMessage {
                        message_type: "joinRoom",
                        payload: join_payload,
                    };
                    if let Ok(json) = serde_json::to_string(&join_message) {
                        let _ = ws_sender.send(json);
                    }
                }
                app.messages.clear();
                app.current_screen = CurrentScreen::InRoom;
                app.status_message = format!("Switching to #{}", room_name);
            }
        }
        KeyCode::Char(':') => {
            app.command_input = Some(String::new());
        }
        _ => {}
    }
}

fn handle_help_screen(app: &mut App, key: event::KeyEvent) {
    match key.code {
        KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => {
            // Return to previous screen (RoomChoice or InRoom)
            if app.room_id.is_some() {
                app.current_screen = CurrentScreen::InRoom;
                app.status_message = "-- NORMAL --".to_string();
            } else {
                app.current_screen = CurrentScreen::RoomChoice;
                app.status_message = "Create or Join a secure room.".to_string();
            }
        }
        KeyCode::Char(':') => {
            app.command_input = Some(String::new());
        }
        _ => {}
    }
}

// --- Command Mode ---

async fn execute_command(app: &mut App<'_>, cmd: &str) {
    let cmd = cmd.trim();
    let parts: Vec<&str> = cmd.split_whitespace().collect();
    let command = parts.first().map(|s| *s).unwrap_or("");
    
    match command {
        // Quit commands
        "q" | "quit" | "leave" => {
            match app.current_screen {
                CurrentScreen::InRoom => {
                    // Leave room, return to main menu
                    if let Some(voice_tx) = &app.voice_tx {
                        let _ = voice_tx.send(voice::manager::VoiceCommand::Leave);
                    }
                    app.voice_active = false;
                    app.voice_users.clear();
                    
                    app.room_id = None;
                    app.room_name = None;
                    app.room_key = None;
                    app.messages.clear();
                    app.online_users.clear();
                    app.typing_users.clear();
                    app.current_screen = CurrentScreen::RoomChoice;
                    app.status_message = "Left room. Press C to create or J to join.".to_string();
                }
                CurrentScreen::RoomChoice => {
                    // Quit application
                    app.should_quit = true;
                }
                _ => {
                    // For all other screens (menus, inputs, etc.), go back to Main Menu
                    app.current_screen = CurrentScreen::RoomChoice;
                    app.status_message = "Create or Join a secure room.".to_string();
                }
            }
        }
        // Force quit
        "qq" | "qa" | "quit!" => {
            app.should_quit = true;
        }
        // Help command
        "h" | "help" => {
            app.current_screen = CurrentScreen::Help;
            app.status_message = "Press Esc, q, or Enter to close help".to_string();
        }
        // Users command - show online users
        "u" | "users" => {
            if app.current_screen == CurrentScreen::InRoom {
                app.show_user_list = !app.show_user_list;
                if app.show_user_list {
                    app.status_message = format!("{} users online. Press Esc to close.", app.online_users.len());
                } else {
                    app.status_message = "-- NORMAL --".to_string();
                }
            } else {
                app.status_message = ":users only works inside a room".to_string();
            }
        }
        // List rooms (room switcher)
        "l" | "list" => {
            if app.current_screen == CurrentScreen::InRoom {
                // Request room list from server
                if let Some(ws_sender) = &app.ws_sender {
                    let list_message = ClientMessage {
                        message_type: "listRooms",
                        payload: ListRoomsPayload {},
                    };
                    if let Ok(json) = serde_json::to_string(&list_message) {
                        let _ = ws_sender.send(json);
                    }
                }
                // Populate user_rooms from public + private rooms
                app.user_rooms.clear();
                app.user_rooms.extend(app.public_rooms.iter().cloned());
                app.user_rooms.extend(app.private_rooms.iter().cloned());
                app.switcher_selected_index = 0;
                app.current_screen = CurrentScreen::RoomSwitcher;
                app.status_message = "Select room to switch".to_string();
            } else {
                app.status_message = ":list only works inside a room".to_string();
            }
        }
        // Switch to room by name
        "s" | "switch" => {
            if let Some(room_name) = parts.get(1) {
                if app.current_screen == CurrentScreen::InRoom {
                    // Find room and switch
                    let target_room = app.public_rooms.iter()
                        .chain(app.private_rooms.iter())
                        .find(|r| r.name == *room_name || r.display_name == *room_name)
                        .cloned();
                    
                    if let Some(room) = target_room {
                        // Join the new room
                        if let Some(ws_sender) = &app.ws_sender {
                            let join_payload = JoinRoomPayload {
                                room_id: None,
                                room_name: Some(&room.name),
                            };
                            let join_message = ClientMessage {
                                message_type: "joinRoom",
                                payload: join_payload,
                            };
                            if let Ok(json) = serde_json::to_string(&join_message) {
                                let _ = ws_sender.send(json);
                            }
                        }
                        app.messages.clear();
                        app.status_message = format!("Switching to #{}", room.name);
                    } else {
                        app.status_message = format!("Room '{}' not found", room_name);
                    }
                } else {
                    app.status_message = ":switch only works inside a room".to_string();
                }
            } else {
                app.status_message = "Usage: :switch <room-name>".to_string();
            }
        }
        // Register command
        "register" | "reg" => {
            app.available_keys = scan_ssh_keys();
            if app.available_keys.is_empty() {
                app.current_screen = CurrentScreen::Registration;
                app.status_message = "No SSH keys found".to_string();
            } else {
                app.current_screen = CurrentScreen::KeySelection;
                app.selected_key_index = 0;
                app.status_message = "Select SSH key to use for registration".to_string();
            }
        }
        // Share/Invite command
        "share" | "invite" => {
            if app.current_screen == CurrentScreen::InRoom {
                if let Some(room_id) = &app.room_id {
                    if let Some(ws_sender) = &app.ws_sender {
                        let payload = CreateInvitePayload {
                            room_id,
                        };
                        let msg = ClientMessage {
                            message_type: "createInvite",
                            payload,
                        };
                        if let Ok(json) = serde_json::to_string(&msg) {
                            let _ = ws_sender.send(json);
                            app.status_message = "Generating invite code...".to_string();
                        }
                    }
                }
            } else {
                app.status_message = ":share only works inside a room".to_string();
            }
        }
        // Join via invite code
        "join" | "j" => {
            if let Some(code) = parts.get(1) {
                if let Some(ws_sender) = &app.ws_sender {
                    let payload = JoinViaInvitePayload {
                        code,
                    };
                    let msg = ClientMessage {
                        message_type: "joinViaInvite",
                        payload,
                    };
                    if let Ok(json) = serde_json::to_string(&msg) {
                        let _ = ws_sender.send(json);
                        app.status_message = format!("Joining via invite code {}...", code);
                    }
                }
            } else {
                app.status_message = "Usage: :join <code>".to_string();
            }
        }
        // Rename room
        "rename" => {
            if app.current_screen == CurrentScreen::InRoom {
                if let Some(room_id) = &app.room_id {
                    if let Some(new_name) = parts.get(1) {
                        if let Some(ws_sender) = &app.ws_sender {
                            let payload = RenameRoomPayload {
                                room_id,
                                new_name,
                            };
                            let msg = ClientMessage {
                                message_type: "renameRoom",
                                payload,
                            };
                            if let Ok(json) = serde_json::to_string(&msg) {
                                let _ = ws_sender.send(json);
                                app.status_message = format!("Renaming room to {}...", new_name);
                            }
                        }
                    } else {
                        app.status_message = "Usage: :rename <new_name>".to_string();
                    }
                }
            } else {
                app.status_message = ":rename only works inside a room".to_string();
            }
        }
        // Delete room
        "delete" => {
            if app.current_screen == CurrentScreen::InRoom {
                if let Some(room_id) = &app.room_id {
                    if let Some(ws_sender) = &app.ws_sender {
                        let payload = DeleteRoomPayload {
                            room_id,
                        };
                        let msg = ClientMessage {
                            message_type: "deleteRoom",
                            payload,
                        };
                        if let Ok(json) = serde_json::to_string(&msg) {
                            let _ = ws_sender.send(json);
                            app.status_message = "Deleting room...".to_string();
                        }
                    }
                }
            } else {
                app.status_message = ":delete only works inside a room".to_string();
            }
        }
        // Transfer ownership
        "transfer" => {
            if app.current_screen == CurrentScreen::InRoom {
                if let Some(room_id) = &app.room_id {
                    if let Some(new_owner) = parts.get(1) {
                        if let Some(ws_sender) = &app.ws_sender {
                            let payload = TransferOwnershipPayload {
                                room_id,
                                new_owner_username: new_owner,
                            };
                            let msg = ClientMessage {
                                message_type: "transferOwnership",
                                payload,
                            };
                            if let Ok(json) = serde_json::to_string(&msg) {
                                let _ = ws_sender.send(json);
                                app.status_message = format!("Transferring ownership to {}...", new_owner);
                            }
                        }
                    } else {
                        app.status_message = "Usage: :transfer <username>".to_string();
                    }
                }
            } else {
                app.status_message = ":transfer only works inside a room".to_string();
            }
        }
        // Direct Message
        "dm" => {
            if let Some(target_user) = parts.get(1) {
                if let Some(ws_sender) = &app.ws_sender {
                    let payload = CreateDMPayload {
                        target_username: target_user,
                    };
                    let msg = ClientMessage {
                        message_type: "createDM",
                        payload,
                    };
                    if let Ok(json) = serde_json::to_string(&msg) {
                        let _ = ws_sender.send(json);
                        app.status_message = format!("Opening DM with {}...", target_user);
                    }
                }
            } else {
                app.status_message = "Usage: :dm <username>".to_string();
            }
        }
        // Voice Chat
        "vc" => {
            if app.current_screen == CurrentScreen::InRoom {
                if let Some(room_id) = &app.room_id {
                    let subcmd = parts.get(1).map(|s| *s).unwrap_or("");
                    if let Some(voice_tx) = &app.voice_tx {
                        match subcmd {
                            "join" | "" => {
                                if app.voice_active && app.voice_users.contains(&app.current_username.clone().unwrap_or_default()) {
                                    app.status_message = "Already in voice chat.".to_string();
                                } else {
                                    let _ = voice_tx.send(voice::manager::VoiceCommand::Join(room_id.clone()));
                                    app.status_message = "Joining voice...".to_string();
                                }
                            }
                            "leave" | "l" => {
                                let _ = voice_tx.send(voice::manager::VoiceCommand::Leave);
                                app.status_message = "Leaving voice...".to_string();
                            }
                            "mute" | "m" => {
                                let _ = voice_tx.send(voice::manager::VoiceCommand::Mute(true));
                                app.status_message = "Microphone muted.".to_string();
                            }
                            "unmute" | "um" => {
                                let _ = voice_tx.send(voice::manager::VoiceCommand::Mute(false));
                                app.status_message = "Microphone unmuted.".to_string();
                            }
                            _ => {
                                app.status_message = "Usage: :vc [join|leave|mute|unmute]".to_string();
                            }
                        }
                    } else {
                        app.status_message = "Voice Chat not initialized.".to_string();
                    }
                }
            } else {
                app.status_message = ":vc only works inside a room".to_string();
            }
        }
        // Aliases
        "m" | "mute" => {
            if let Some(voice_tx) = &app.voice_tx {
                let _ = voice_tx.send(voice::manager::VoiceCommand::Mute(true));
                app.status_message = "Microphone muted.".to_string();
            }
        }
        "um" | "unmute" => {
            if let Some(voice_tx) = &app.voice_tx {
                let _ = voice_tx.send(voice::manager::VoiceCommand::Mute(false));
                app.status_message = "Microphone unmuted.".to_string();
            }
        }
        "vcl" => {
            if let Some(voice_tx) = &app.voice_tx {
                let _ = voice_tx.send(voice::manager::VoiceCommand::Leave);
                app.status_message = "Leaving voice...".to_string();
            }
        }
        
        // Unknown command
        "" => {
            // Empty command, do nothing
        }
        _ => {
            app.status_message = format!("Unknown command: {}. Type :help for list.", command);
        }
    }
}

fn handle_mouse_in_room(app: &mut App, mouse: MouseEvent) {
    match mouse.kind {
        MouseEventKind::ScrollDown => {
            // Scroll messages down
            if app.message_scroll_offset > 0 {
                app.message_scroll_offset = app.message_scroll_offset.saturating_sub(1);
            }
        }
        MouseEventKind::ScrollUp => {
            // Scroll messages up
            let max_scroll = app.messages.len().saturating_sub(10);
            if app.message_scroll_offset < max_scroll {
                app.message_scroll_offset += 1;
            }
        }
        MouseEventKind::Down(_button) => {
            // Mouse click - could be used for text selection in the future
            // For now, just auto-copy selected text if supported by terminal
        }
        _ => {}
    }
}

// --- WebSocket & Message Handling ---

fn handle_server_message(app: &mut App, msg: ServerMessage) {
    match msg {
        ServerMessage::Message(payload) => {
            if let Some(key) = &app.room_key {
                match decrypt(key, &payload.ciphertext) {
                    Ok(plaintext) => {
                        app.messages.push(ChatMessage::new(plaintext, Some(payload.username.clone()), Some(payload.timestamp.clone())));
                        app.message_scroll_offset = 0; // Auto-scroll to bottom

                        // Desktop Notification
                        if !app.is_focused && Some(&payload.username) != app.current_username.as_ref() {
                            let _ = Notification::new()
                                .summary(&format!("New message from {}", payload.username))
                                .body("You have a new encrypted message")
                                .appname("eurus")
                                .show();
                        }
                    }
                    Err(_) => app.messages.push(ChatMessage::system(format!(
                        "Failed to decrypt message from {}",
                        payload.username
                    ))),
                }
            }
        }
        ServerMessage::UserJoined(payload) => {
            // Add user to online list if not already present
            if !app.online_users.contains(&payload.username) {
                app.online_users.push(payload.username.clone());
            }
            app.messages.push(ChatMessage::system(format!(
                "{} joined the room",
                payload.username
            )));
        }
        ServerMessage::UserLeft(payload) => {
            // Remove user from online list
            app.online_users.retain(|u| u != &payload.username);
            app.messages.push(ChatMessage::system(format!(
                "{} left the room",
                payload.username
            )));
        }
        ServerMessage::RoomJoined(payload) => {
            app.status_message = format!("Joined room: {}", payload.display_name);
            
            // Force switch to InRoom screen
            app.current_screen = CurrentScreen::InRoom;
            
            // Store room info
            app.room_id = Some(payload.room_id.clone());
            app.room_name = Some(payload.room_name.clone());
            app.room_display_name = Some(payload.display_name.clone());
            
            // Store the room key from the server
            if !payload.encrypted_key.is_empty() {
                if let Some(key) = key_from_hex(&payload.encrypted_key) {
                    app.room_key = Some(key);
                } else {
                    app.status_message = "Error: Failed to decode room key".to_string();
                }
            }
            
            // Load message history
            app.messages.clear();
            for msg in payload.messages {
                if let Some(key) = &app.room_key {
                    if let Ok(plaintext) = decrypt(key, &msg.ciphertext) {
                        app.messages.push(ChatMessage::new(plaintext, Some(msg.username), Some(msg.timestamp)));
                    } else {
                        app.messages.push(ChatMessage::new(
                            "<Encrypted Message>".to_string(),
                            Some(msg.username),
                            Some(msg.timestamp),
                        ));
                    }
                } else {
                     // We need the room key to decrypt!
                     // The key exchange logic happens separately.
                }
            }
            
            // Update online users
            app.online_users = payload.online_users.into_iter().map(|u| u.username).collect();
        }
        ServerMessage::RoomCreated(payload) => {
            app.status_message = format!("Room created: {}", payload.display_name);
            app.room_id = Some(payload.room_id);
            app.room_name = Some(payload.room_name);
            app.room_display_name = Some(payload.display_name.clone());
            
            // Store the room key from the server
            if !payload.encrypted_key.is_empty() {
                if let Some(key) = key_from_hex(&payload.encrypted_key) {
                    app.room_key = Some(key);
                } else {
                    app.status_message = "Error: Failed to decode room key".to_string();
                }
            }
            
            // Switch to room view
            app.current_screen = CurrentScreen::InRoom;
        }
        ServerMessage::RoomsList(payload) => {
            app.public_rooms = payload.public_rooms;
            app.private_rooms = payload.private_rooms;
            app.status_message = format!(
                "Loaded {} public and {} private rooms",
                app.public_rooms.len(),
                app.private_rooms.len()
            );
        }
        ServerMessage::Info(payload) => {
            app.status_message = payload.message.clone();
            app.messages.push(ChatMessage::system(payload.message));
        }
        ServerMessage::Error(payload) => {
            app.status_message = format!("Error: {}", payload.message);
            app.messages.push(ChatMessage::system(format!("Error: {}", payload.message)));
        }
        ServerMessage::UserTyping(payload) => {
            // Add user to typing list with current timestamp
            app.typing_users.insert(payload.username.clone(), std::time::Instant::now());
        }
        ServerMessage::InviteCreated(payload) => {
            app.messages.push(ChatMessage::system(format!(
                "Invite code generated: {}",
                payload.code
            )));
            app.messages.push(ChatMessage::system(
                "Share this code with others to let them join this room.".to_string(),
            ));
            app.messages.push(ChatMessage::system(
                "This code expires in 24 hours.".to_string(),
            ));
            app.message_scroll_offset = 0;
            
            // Try to copy to clipboard
            if let Some(clipboard) = &mut app.clipboard {
                match clipboard.copy_text(&payload.code) {
                    Ok(_) => app.status_message = format!("Invite code {} copied to clipboard!", payload.code),
                    Err(_) => app.status_message = format!("Invite code: {}", payload.code),
                }
            } else {
                app.status_message = format!("Invite code: {}", payload.code);
            }
        }
        ServerMessage::RoomRenamed(payload) => {
            if let Some(current_room_id) = &app.room_id {
                if current_room_id == &payload.room_id {
                    app.room_name = Some(payload.new_name.clone());
                    app.room_display_name = Some(payload.display_name.clone());
                    app.status_message = format!("Room renamed to {}", payload.display_name);
                }
            }
            app.messages.push(ChatMessage::system(format!(
                "Room renamed to {}",
                payload.display_name
            )));
        }
        ServerMessage::RoomDeleted(payload) => {
            if let Some(current_room_id) = &app.room_id {
                if current_room_id == &payload.room_id {
                    // Leave room
                    app.room_id = None;
                    app.room_name = None;
                    app.room_display_name = None;
                    app.room_key = None;
                    app.messages.clear();
                    app.online_users.clear();
                    app.typing_users.clear();
                    app.current_screen = CurrentScreen::RoomChoice;
                    app.status_message = "Room was deleted by owner.".to_string();
                }
            }
        }
        ServerMessage::OwnershipTransferred(payload) => {
            app.messages.push(ChatMessage::system(format!(
                "Room ownership transferred to {}",
                payload.new_owner_username
            )));
        }
        ServerMessage::VoiceSignal(payload) => {
            if let Some(voice_tx) = &app.voice_tx {
                if let (Some(sender_id), Some(_sender_username)) = (payload.sender_user_id, payload.sender_username) {
                    let _ = voice_tx.send(voice::manager::VoiceCommand::Signal {
                        sender_id,
                        signal_type: payload.signal_type,
                        data: payload.data,
                    });
                }
            }
        }
        ServerMessage::VoiceState(payload) => {
            if Some(payload.room_id) == app.room_id {
                app.voice_users = payload.active_users;
                app.voice_active = !app.voice_users.is_empty();
                // TODO: Show notification if focused
            }
        }
    }
}


fn extract_username_from_token(token: &str) -> Option<String> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    
    // JWT payload is the second part
    // Add padding if needed
    let payload_b64 = parts[1];
    let padding = match payload_b64.len() % 4 {
        2 => "==",
        3 => "=",
        _ => "",
    };
    let payload_padded = format!("{}{}", payload_b64, padding);
    
    use base64::{Engine as _, engine::general_purpose};
    if let Ok(decoded) = general_purpose::URL_SAFE_NO_PAD.decode(payload_b64) 
        .or_else(|_| general_purpose::STANDARD.decode(&payload_padded)) 
    {
        if let Ok(json_str) = String::from_utf8(decoded) {
            #[derive(serde::Deserialize)]
            struct JwtPayload {
                username: String,
            }
            if let Ok(payload) = serde_json::from_str::<JwtPayload>(&json_str) {
                return Some(payload.username);
            }
        }
    }
    None
}

fn load_auth_token(token_path: &str) -> Option<String> {
    use std::fs;
    use std::path::Path;
    
    // Expand ~ to home directory
    let expanded_path = if token_path.starts_with("~/") {
        if let Some(home) = dirs::home_dir() {
            home.join(&token_path[2..])
        } else {
            return None;
        }
    } else {
        Path::new(token_path).to_path_buf()
    };
    
    // Read token file
    fs::read_to_string(expanded_path)
        .ok()
        .map(|s| s.trim().to_string())
}

fn scan_ssh_keys() -> Vec<(String, String)> {
    use std::fs;
    
    let ssh_dir = dirs::home_dir()
        .map(|h| h.join(".ssh"))
        .unwrap_or_else(|| std::path::PathBuf::from("~/.ssh"));
    
    let mut keys = Vec::new();
    
    if let Ok(entries) = fs::read_dir(&ssh_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if let Some(ext) = path.extension() {
                if ext == "pub" {
                    if let Ok(content) = fs::read_to_string(&path) {
                        let content = content.trim().to_string();
                        if content.starts_with("ssh-ed25519") || content.starts_with("ssh-rsa") {
                            let path_str = path.to_string_lossy().to_string();
                            keys.push((path_str, content));
                        }
                    }
                }
            }
        }
    }
    
    // Sort: ed25519 keys first, then alphabetically
    keys.sort_by(|a, b| {
        let a_is_ed25519 = a.1.starts_with("ssh-ed25519");
        let b_is_ed25519 = b.1.starts_with("ssh-ed25519");
        
        match (a_is_ed25519, b_is_ed25519) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.0.cmp(&b.0),
        }
    });
    
    keys
}

fn detect_key_type(public_key: &str) -> Option<&'static str> {
    if public_key.starts_with("ssh-ed25519") {
        Some("ed25519")
    } else if public_key.starts_with("ssh-rsa") {
        Some("rsa")
    } else {
        None
    }
}

async fn register_user(server_url: &str, username: &str, public_key: &str) -> Result<String, Box<dyn Error>> {
    use serde::{Deserialize, Serialize};
    
    #[derive(Serialize)]
    struct RegisterRequest {
        username: String,
        #[serde(rename = "publicKey")]
        public_key: String,
        #[serde(rename = "keyType")]
        key_type: String,
    }
    
    #[derive(Deserialize)]
    struct RegisterResponse {
        #[serde(rename = "userId")]
        _user_id: String,
        #[serde(rename = "username")]
        _username: String,
        token: String,
    }
    
    #[derive(Deserialize)]
    struct ErrorResponse {
        error: String,
    }
    
    let key_type = detect_key_type(public_key)
        .ok_or("Invalid SSH key type")?
        .to_string();
    
    let request = RegisterRequest {
        username: username.to_string(),
        public_key: public_key.to_string(),
        key_type,
    };
    
    // Convert WebSocket URL to HTTP URL for API call
    let api_url = server_url
        .replace("wss://", "https://")
        .replace("ws://", "http://")
        .replace("/ws", "");
    
    let client = reqwest::Client::new();
    let response = client
        .post(format!("{}/api/auth/register", api_url))
        .json(&request)
        .send()
        .await?;
    
    if !response.status().is_success() {
        let error: ErrorResponse = response.json().await.unwrap_or(ErrorResponse {
            error: "Unknown error".to_string(),
        });
        return Err(error.error.into());
    }
    
    let result: RegisterResponse = response.json().await?;
    Ok(result.token)
}

fn save_auth_token(token: &str) -> Result<(), Box<dyn Error>> {
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    
    // Get config directory
    let config_dir = dirs::config_dir()
        .ok_or("Config directory not found")?
        .join("eurus");
    
    // Create directory if it doesn't exist
    fs::create_dir_all(&config_dir)?;
    
    // Write token file
    let token_path = config_dir.join("token");
    fs::write(&token_path, token)?;
    
    // Set permissions to 0600 (owner read/write only) on Unix
    #[cfg(unix)]
    {
        let mut perms = fs::metadata(&token_path)?.permissions();
        perms.set_mode(0o600);
        fs::set_permissions(&token_path, perms)?;
    }
    
    Ok(())
}

async fn establish_connection(
    app: &mut App<'_>,
    ws_incoming_tx: mpsc::UnboundedSender<String>,
) -> Result<(), Box<dyn Error>> {
    // Try to connect with exponential backoff
    let max_attempts = 5;
    let mut delay_ms = 1000;
    
    for attempt in 0..max_attempts {
        app.reconnect_attempts = attempt;
        app.is_reconnecting = attempt > 0;
        
        if attempt > 0 {
            app.status_message = format!("Reconnecting... attempt {}/{}", attempt + 1, max_attempts);
            tokio::time::sleep(tokio::time::Duration::from_millis(delay_ms)).await;
            delay_ms = (delay_ms * 2).min(30000); // Max 30 seconds
        }
        
        match try_connect(app, ws_incoming_tx.clone()).await {
            Ok(_) => {
                app.reconnect_attempts = 0;
                app.is_reconnecting = false;
                app.status_message = "Connected".to_string();
                return Ok(());
            }
            Err(e) if attempt == max_attempts - 1 => {
                app.status_message = format!("Connection failed: {}", e);
                return Err(e);
            }
            Err(_) => continue,
        }
    }
    
    Err("Failed to connect after multiple attempts".into())
}

async fn try_connect(
    app: &mut App<'_>,
    ws_incoming_tx: mpsc::UnboundedSender<String>,
) -> Result<(), Box<dyn Error>> {
    let mut ws_url = app.config.server.url.clone();
    
    // Try to load token and append to URL
    if let Some(token) = load_auth_token(&app.config.auth.token_path) {
        // Append token as query parameter
        let separator = if ws_url.contains('?') { '&' } else { '?' };
        ws_url = format!("{}{}token={}", ws_url, separator, token);
    }
    
    let (ws_stream, _) = connect_async(&ws_url).await?;
    let (mut write, mut read) = ws_stream.split();

    // Create a channel for sending messages to the WebSocket task
    let (ws_outgoing_tx, mut ws_outgoing_rx) = mpsc::unbounded_channel::<String>();
    app.ws_sender = Some(ws_outgoing_tx);

    // Task to listen for incoming messages from the server
    let incoming_tx = ws_incoming_tx.clone();
    tokio::spawn(async move {
        while let Some(message_result) = read.next().await {
            match message_result {
                Ok(msg) => {
                    if let Message::Text(text) = msg {
                        if incoming_tx.send(text).is_err() {
                            break;
                        }
                    }
                }
                Err(_) => {
                    let _ = incoming_tx.send("__DISCONNECT__".to_string());
                    break;
                }
            }
        }
    });

    // Task to send outgoing messages from the app to the server, with periodic pings
    tokio::spawn(async move {
        let mut ping_interval = interval(Duration::from_secs(30));
        ping_interval.tick().await; // Skip the first immediate tick
        
        loop {
            tokio::select! {
                // Handle outgoing messages
                Some(json) = ws_outgoing_rx.recv() => {
                    if write.send(Message::text(json)).await.is_err() {
                        break;
                    }
                }
                // Send periodic pings to keep connection alive
                _ = ping_interval.tick() => {
                    if write.send(Message::Ping(vec![])).await.is_err() {
                        break;
                    }
                }
            }
        }
    });

    Ok(())
}


// --- UI Rendering ---

fn ui(f: &mut Frame, app: &mut App) {
    // Force the entire background to be Pure Black (RGB 0,0,0) to override terminal theme palette
    let background_block = Block::default().style(Style::default().bg(Color::Rgb(0, 0, 0)));
    f.render_widget(background_block, f.area());

    // Layout: Header (1), Chat (Min 1), Status/Padding (3)
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // Header
            Constraint::Min(1),    // Main content
            Constraint::Length(3), // Footer padding (where status + floating input will sit)
        ])
        .split(f.area());

    // --- Header Rendering ---
    let header_text = match app.current_screen {
        CurrentScreen::InRoom => {
            let room_name = app.room_display_name.as_deref().unwrap_or("Unknown");
            Line::from(vec![
                Span::styled(" eurus ", Style::default().bg(Color::Blue).fg(Color::Black).add_modifier(Modifier::BOLD)),
                Span::raw(" "),
                Span::styled(format!(" {} ", room_name), Style::default().bg(Color::DarkGray).fg(Color::White)),
                Span::raw(" "),
                Span::styled(format!(" {} online ", app.online_users.len()), Style::default().fg(Color::Gray)),
            ])
        },
        _ => Line::from(vec![
            Span::styled(" eurus ", Style::default().bg(Color::Blue).fg(Color::Black).add_modifier(Modifier::BOLD)),
            Span::raw(" - private messaging"),
        ]),
    };

    let header = Paragraph::new(header_text).style(Style::default().bg(Color::Rgb(0, 0, 0)));
    f.render_widget(header, chunks[0]);

    // --- Body Rendering ---
    
    // Split the body into Sidebar (Voice) and Main Chat area
    // Sidebar visible only in InRoom (or always? Let's do always for consistency, or active room)
    // For now, only InRoom makes sense.
    
    let (sidebar_area, main_area) = if app.current_screen == CurrentScreen::InRoom {
        let body_layout = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(25), // Voice Sidebar
                Constraint::Min(1),     // Main Content
            ])
            .split(chunks[1]);
        (Some(body_layout[0]), body_layout[1])
    } else {
        (None, chunks[1])
    };

    // Calculate the floating input area RECT relative to MAIN AREA
    // Centered in main_area, max width 100 chars, or 90% of main_area
    let input_width = 100.min((main_area.width as f32 * 0.95) as u16); // 95% of remaining space
    let input_height = 4; 
    
    let input_y = main_area.y + main_area.height.saturating_sub(input_height); // Bottom of main_area (chunks[1])
    // Wait, chunks[1] does NOT include the footer padding (chunks[2]).
    // But we want it to float *above* the footer.
    // chunks[1] is "Min(1)". chunks[2] is "Length(3)".
    // So main_area.y + main_area.height IS the top of the footer.
    // So we want y = (main_area.y + main_area.height) - input_height.
    
    let input_x = main_area.x + (main_area.width.saturating_sub(input_width)) / 2;
    
    let floating_input_area = Rect {
        x: input_x,
        y: input_y,
        width: input_width,
        height: input_height,
    };

    // Render Voice Sidebar if active
    if let Some(area) = sidebar_area {
        render_voice_sidebar(f, app, area);
    }

    match app.current_screen {
        // Registration screens
        CurrentScreen::Registration => render_registration_error(f, main_area),
        CurrentScreen::KeySelection => render_key_selection(f, app, main_area),
        CurrentScreen::UsernameInput => render_username_input(f, app, main_area),
        CurrentScreen::RegistrationSuccess => render_registration_success(f, app, main_area),
        
        // Main screens
        CurrentScreen::RoomChoice => render_room_choice(f, main_area),
        CurrentScreen::RoomList => render_room_list(f, app, main_area),
        CurrentScreen::RoomTypeSelection => render_room_type_selection(f, app, main_area),
        CurrentScreen::CreateRoomInput => render_create_room_input(f, app, main_area),
        CurrentScreen::RoomCreation => render_room_creation(f, app, main_area),
        CurrentScreen::InRoom => render_in_room(f, app, main_area, floating_input_area), 
        CurrentScreen::RoomSwitcher => {
            render_in_room(f, app, main_area, floating_input_area);
            render_room_switcher_overlay(f, app, main_area);
        }
        CurrentScreen::Help => render_help(f, main_area),
    }

    // Render footer status at the very bottom line
    // Use the last line of the screen
    let footer_area = Rect {
        x: 0,
        y: f.area().height.saturating_sub(1),
        width: f.area().width,
        height: 1,
    };
    render_footer(f, app, footer_area);
}

fn render_room_choice(f: &mut Frame, area: Rect) {
    // Add some padding
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(2), Constraint::Percentage(100), Constraint::Min(2)])
        .split(area);
        
    let text = Text::from(vec![
        Line::from(""),
        Line::from("Welcome to eurus").style(Style::default().add_modifier(Modifier::BOLD).fg(Color::Cyan)),
        Line::from(""),
        Line::from("  c   Create new room"),
        Line::from("  j   Join / browse rooms"),
        Line::from(""),
        Line::from("  :   Command mode"),
        Line::from(""),
    ]);
    
    let widget = Paragraph::new(text).alignment(Alignment::Left);
    f.render_widget(widget, chunks[1]);
}

fn render_room_list(f: &mut Frame, app: &App, area: Rect) {
    let rooms_to_display = if app.viewing_private {
        &app.private_rooms
    } else {
        &app.public_rooms
    };
    
    let room_type = if app.viewing_private { "PRIVATE" } else { "PUBLIC" };
    
    // Custom header for the list
    let header_text = format!("{} ROOMS (Tab to switch)", room_type);
    
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(1)])
        .split(area);
        
    f.render_widget(
        Paragraph::new(header_text).style(Style::default().add_modifier(Modifier::BOLD)), 
        chunks[0]
    );
    
    let items: Vec<ListItem> = rooms_to_display
        .iter()
        .enumerate()
        .map(|(i, room)| {
            let style = if i == app.selected_room_index {
                Style::default().bg(Color::DarkGray).fg(Color::White).add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            
            // Simple indicator
            let cursor = if i == app.selected_room_index { ">" } else { " " };
            
            let content = format!(
                "{} {:<20} ({} users)",
                cursor,
                room.display_name,
                room.member_count
            );
            ListItem::new(content).style(style)
        })
        .collect();
    
    let list = List::new(items);
    f.render_widget(list, chunks[1]);
}

fn render_room_type_selection(f: &mut Frame, app: &App, area: Rect) {
    let public_style = if !app.selected_room_type {
        Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    };
    
    let private_style = if app.selected_room_type {
        Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray)
    };
    
    let public_marker = if !app.selected_room_type { "●" } else { "○" };
    let private_marker = if app.selected_room_type { "●" } else { "○" };
    
    let text = Text::from(vec![
        Line::from(""),
        Line::from("Select Room Type").style(Style::default().add_modifier(Modifier::BOLD)),
        Line::from(""),
        Line::from(vec![
            Span::styled(format!("  {} Public  ", public_marker), public_style),
            Span::raw("Anyone can join"),
        ]),
        Line::from(vec![
            Span::styled(format!("  {} Private ", private_marker), private_style),
            Span::raw("Invite only"),
        ]),
        Line::from(""),
        Line::from("Tab to switch • Enter to confirm"),
    ]);
    
    let paragraph = Paragraph::new(text).alignment(Alignment::Left);
    f.render_widget(paragraph, area);
}

fn render_create_room_input(f: &mut Frame, app: &mut App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(3)])
        .split(area);
    
    let help_text = Paragraph::new("Enter room name:")
        .style(Style::default().fg(Color::Cyan));
    f.render_widget(help_text, chunks[0]);
    
    // Render input without block
    app.room_name_input.set_block(Block::default());
    f.render_widget(&app.room_name_input, chunks[1]);
}

fn render_room_creation(f: &mut Frame, app: &mut App, area: Rect) {
    let text: Vec<Line> = app.messages.iter().map(|m| Line::from(m.content.clone())).collect();
    let widget = Paragraph::new(Text::from(text))
        .alignment(Alignment::Left)
        .wrap(Wrap { trim: true });
    f.render_widget(widget, area);
}

fn render_in_room(f: &mut Frame, app: &mut App, chat_area: Rect, input_area: Rect) {
    // --- Message Area ---
    // The chat column should align with the input box horizontally.
    // Vertically, it should start at the top of chat_area and END above the input_area.
    
    // Calculate available height above input box
    let chat_height = input_area.y.saturating_sub(chat_area.y).saturating_sub(1); // 1 line gap
    
    let aligned_chat_area = Rect {
        x: input_area.x,
        y: chat_area.y,
        width: input_area.width,
        height: chat_height,
    };
    
    // Apply background color to the chat column
    let chat_bg_color = Color::Rgb(25, 25, 25);
    
    // Create a block with padding to fix the "text touching edges" issue
    let chat_block = Block::default()
        .style(Style::default().bg(chat_bg_color))
        .padding(ratatui::widgets::Padding::horizontal(2)); 
        
    f.render_widget(chat_block.clone(), aligned_chat_area);

    // Calculate inner area after padding
    let inner_area = chat_block.inner(aligned_chat_area);
    let inner_width = inner_area.width as usize;

    let mut text_content: Vec<Line> = Vec::new();
    let mut last_sender: Option<String> = None;
    let mut last_date: Option<String> = None;

    for msg in &app.messages {
        // Date Separator
        if last_date.as_ref() != Some(&msg.date) {
            let date_str = &msg.date;
            // Calculate padding based on inner_width
            let total_padding = inner_width.saturating_sub(date_str.len() + 2);
            let padding_side = total_padding / 2;
            
            let separator = "─".repeat(padding_side);
            
            text_content.push(Line::from(vec![
                Span::styled(format!("{} {} {}", separator, date_str, separator), Style::default().fg(Color::DarkGray).bg(chat_bg_color)),
            ]));
            last_date = Some(msg.date.clone());
            last_sender = None; // Reset sender on new date
        }

        if msg.is_system {
            text_content.push(Line::from(vec![
                Span::styled(format!("! {}", msg.content), Style::default().fg(Color::Magenta).bg(chat_bg_color)),
            ]));
            last_sender = None;
        } else {
            // Group consecutive messages
            let is_consecutive = last_sender.as_ref() == msg.sender.as_ref();
            
            if !is_consecutive {
                // Render User Header: [Username] ... [Time]
                let sender_name = msg.sender.as_deref().unwrap_or("Unknown");
                let timestamp = &msg.timestamp;
                let prefix = " > ";
                
                // Calculate space between name and timestamp
                let content_len = prefix.len() + sender_name.len() + timestamp.len();
                let spacer_len = inner_width.saturating_sub(content_len);
                
                text_content.push(Line::from("")); // Spacing between groups
                text_content.push(Line::from(vec![
                    // Prefix Symbol
                    Span::styled(prefix, Style::default().fg(Color::Green).add_modifier(Modifier::BOLD).bg(chat_bg_color)),
                    // Username
                    Span::styled(sender_name, Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD).bg(chat_bg_color)),
                    // Spacer
                    Span::styled(" ".repeat(spacer_len), Style::default().bg(chat_bg_color)),
                    // Timestamp
                    Span::styled(timestamp, Style::default().fg(Color::DarkGray).bg(chat_bg_color)),
                ]));
                last_sender = msg.sender.clone();
            }
            
            // Render content 
            // Manual wrapping logic to preserve indentation on wrapped lines
            let content = &msg.content;
            let available_width = inner_width.saturating_sub(3); // 3 spaces indentation
            
            if available_width > 0 {
                // Simple wrapping logic (char based for simplicity in TUI context, or could split by words)
                // Ideally use textwrap crate but we don't have it.
                // We'll iterate chars.
                let mut current_line = String::new();
                let mut current_width = 0;
                
                for word in content.split_whitespace() {
                    let word_len = word.chars().count();
                    
                    if current_width + word_len + (if current_width > 0 { 1 } else { 0 }) > available_width {
                        // Flush current line
                        text_content.push(Line::from(vec![
                            Span::styled("   ", Style::default().bg(chat_bg_color)), 
                            Span::styled(current_line.clone(), Style::default().bg(chat_bg_color)),
                        ]));
                        current_line.clear();
                        current_width = 0;
                    }
                    
                    if current_width > 0 {
                        current_line.push(' ');
                        current_width += 1;
                    }
                    current_line.push_str(word);
                    current_width += word_len;
                }
                // Flush remaining
                if !current_line.is_empty() {
                    text_content.push(Line::from(vec![
                        Span::styled("   ", Style::default().bg(chat_bg_color)), 
                        Span::styled(current_line, Style::default().bg(chat_bg_color)),
                    ]));
                }
            } else {
                // Fallback if width is too small
                text_content.push(Line::from(vec![
                    Span::styled("   ", Style::default().bg(chat_bg_color)), 
                    Span::styled(content.clone(), Style::default().bg(chat_bg_color)),
                ]));
            }
        }
    }

    // Add typing indicator
    if !app.typing_users.is_empty() {
        let typing_names: Vec<&String> = app.typing_users.keys().collect();
        let typing_text = if typing_names.len() == 1 {
            format!("{} is typing...", typing_names[0])
        } else if typing_names.len() == 2 {
            format!("{} and {} are typing...", typing_names[0], typing_names[1])
        } else {
            format!("{} people are typing...", typing_names.len())
        };
        text_content.push(Line::from(vec![
            Span::styled(typing_text, Style::default().fg(Color::Yellow).add_modifier(Modifier::ITALIC).bg(chat_bg_color)),
        ]));
    }
    
    // Calculate scroll based on VISUAL lines (accounting for wrapping)
    let visible_height = inner_area.height as usize;
    
    // Calculate total wrapped lines
    let mut total_visual_lines = 0;
    for line in &text_content {
        let content_len = line.width(); // Get visual width of the line content
        if content_len == 0 {
            total_visual_lines += 1; // Empty line still takes 1 row
        } else {
            // How many rows does this line take when wrapped at inner_width?
            // Use rounding up division: (len + width - 1) / width
            let rows = (content_len + inner_width - 1) / inner_width;
            total_visual_lines += rows.max(1);
        }
    }
    
    let scroll_y = if total_visual_lines > visible_height {
        (total_visual_lines - visible_height).saturating_sub(app.message_scroll_offset)
    } else {
        0
    };
    
    let messages_paragraph = Paragraph::new(text_content)
        .wrap(Wrap { trim: false })
        .scroll((scroll_y as u16, 0));
    
    // Render text into the padded inner area
    f.render_widget(messages_paragraph, inner_area);

    // --- Input Area ---
    let vim_mode_str = match app.vim_state.mode {
        VimMode::Normal => "NORMAL",
        VimMode::Insert => "INSERT",
    };
    
    // Clear area behind the floating input
    f.render_widget(Clear, input_area);
    
    // Input Box Colors: Lighter Gray (Rgb 45, 45, 45)
    let input_bg_color = Color::Rgb(45, 45, 45);
    
    // Floating Input Block style
    let input_block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", vim_mode_str))
        .title_style(Style::default().fg(match app.vim_state.mode {
            VimMode::Normal => Color::Cyan,
            VimMode::Insert => Color::Green,
        }).add_modifier(Modifier::BOLD))
        .border_style(Style::default().fg(Color::Gray)) // Lighter border
        .style(Style::default().bg(input_bg_color)); 

    app.message_input.set_block(input_block);
    app.message_input.set_cursor_style(Style::default().add_modifier(Modifier::REVERSED));
    app.message_input.set_style(Style::default().fg(Color::White).bg(input_bg_color));
    
    f.render_widget(&app.message_input, input_area);
    
    // Removed "? for help" hint as requested
    
    // Render emoji picker overlay if active
    if app.emoji_picker_active && !app.emoji_matches.is_empty() {
        render_emoji_picker(f, app, input_area);
    }
    
    // Render user list overlay if active
    if app.show_user_list {
        render_user_list_overlay(f, app, f.area());
    }
}

fn render_user_list_overlay(f: &mut Frame, app: &App, area: Rect) {
    // Calculate centered overlay area
    let overlay_width = 40.min(area.width.saturating_sub(4));
    let overlay_height = (app.online_users.len() as u16 + 4).min(area.height.saturating_sub(4));
    let overlay_x = area.x + (area.width - overlay_width) / 2;
    let overlay_y = area.y + (area.height - overlay_height) / 2;
    
    let overlay_area = Rect {
        x: overlay_x,
        y: overlay_y,
        width: overlay_width,
        height: overlay_height,
    };
    
    // Clear the area behind the overlay
    f.render_widget(Clear, overlay_area);
    
    // Create list items for online users
    let items: Vec<ListItem> = app
        .online_users
        .iter()
        .map(|username| {
            let is_typing = app.typing_users.contains_key(username);
            let display = if is_typing {
                format!("● {} (typing...)", username)
            } else {
                format!("● {}", username)
            };
            ListItem::new(display).style(Style::default().fg(Color::Green))
        })
        .collect();
    
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan))
                .title(format!("Online Users ({}) - Esc to close", app.online_users.len()))
        );
    
    f.render_widget(list, overlay_area);
}

fn render_voice_sidebar(f: &mut Frame, app: &App, area: Rect) {
    // Only render if there are voice users or I am in voice
    if app.voice_users.is_empty() {
        return;
    }
    
    let items: Vec<ListItem> = app.voice_users.iter().map(|u| {
        let style = if Some(u) == app.current_username.as_ref() {
            Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        ListItem::new(format!("🔊 {}", u)).style(style)
    }).collect();
    
    let list = List::new(items)
        .block(Block::default()
            .borders(Borders::RIGHT) // Separator line
            .border_style(Style::default().fg(Color::DarkGray))
            .title(" Voice ")
            .title_style(Style::default().fg(Color::Green)));
            
    f.render_widget(list, area);
}

fn render_emoji_picker(f: &mut Frame, app: &App, input_area: Rect) {
    // Position the picker above the input area
    let picker_height = (app.emoji_matches.len() as u16).min(8) + 2; // +2 for borders
    let picker_width = 40;
    
    // Position above the input, aligned to the left
    let picker_x = input_area.x + 1;
    let picker_y = input_area.y.saturating_sub(picker_height);
    
    let picker_area = Rect {
        x: picker_x,
        y: picker_y,
        width: picker_width.min(input_area.width.saturating_sub(2)),
        height: picker_height,
    };
    
    // Clear the area behind the picker
    f.render_widget(Clear, picker_area);
    
    // Create list items for emoji suggestions
    let items: Vec<ListItem> = app
        .emoji_matches
        .iter()
        .enumerate()
        .map(|(i, (shortcode, emoji, _desc))| {
            let style = if i == app.emoji_selected_index {
                Style::default().bg(Color::DarkGray).fg(Color::White)
            } else {
                Style::default()
            };
            ListItem::new(format!("{} :{}", emoji, shortcode)).style(style)
        })
        .collect();
    
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Yellow))
                .title(format!(":{} - ↑↓ Tab/Enter", app.emoji_partial))
        );
    
    f.render_widget(list, picker_area);
}

fn render_room_switcher_overlay(f: &mut Frame, app: &App, area: Rect) {
    // Calculate centered overlay area (50% width, 60% height)
    let overlay_width = area.width / 2;
    let overlay_height = (area.height * 3) / 5;
    let overlay_x = area.x + (area.width - overlay_width) / 2;
    let overlay_y = area.y + (area.height - overlay_height) / 2;
    
    let overlay_area = Rect {
        x: overlay_x,
        y: overlay_y,
        width: overlay_width,
        height: overlay_height,
    };
    
    // Create list items from user's rooms
    let items: Vec<ListItem> = app.user_rooms
        .iter()
        .enumerate()
        .map(|(i, room)| {
            let is_current = app.room_name.as_ref() == Some(&room.name);
            let marker = if is_current { "* " } else { "  " };
            let content = format!("{}#{}", marker, room.display_name);
            
            let style = if i == app.switcher_selected_index {
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
            } else if is_current {
                Style::default().fg(Color::Green)
            } else {
                Style::default()
            };
            
            ListItem::new(content).style(style)
        })
        .collect();
    
    let list = List::new(items)
        .block(Block::default()
            .borders(Borders::ALL)
            .title("Your Rooms - [Enter] Switch [Esc] Cancel")
            .border_style(Style::default().fg(Color::Cyan)));
    
    // Clear background and render overlay
    f.render_widget(Clear, overlay_area);
    f.render_widget(list, overlay_area);
}

fn render_registration_error(f: &mut Frame, area: Rect) {
    let text = Text::from(vec![
        Line::from(""),
        Line::from("No SSH keys found!").style(Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
        Line::from(""),
        Line::from("eurus requires an SSH key for authentication."),
        Line::from(""),
        Line::from("To create one, run:"),
        Line::from("  ssh-keygen -t ed25519").style(Style::default().fg(Color::Cyan)),
        Line::from(""),
        Line::from("Then press 'r' to retry or 'q' to quit."),
    ]);
    
    let paragraph = Paragraph::new(text)
        .alignment(Alignment::Center)
        .block(Block::default()
            .borders(Borders::ALL)
            .title("Registration")
            .border_style(Style::default().fg(Color::Red)));
    f.render_widget(paragraph, area);
}

fn render_key_selection(f: &mut Frame, app: &App, area: Rect) {
    let items: Vec<ListItem> = app.available_keys
        .iter()
        .enumerate()
        .map(|(i, (path, content))| {
            // Extract key type and truncate the key
            let key_type = if content.starts_with("ssh-ed25519") {
                "ed25519"
            } else {
                "rsa"
            };
            
            // Get filename from path
            let filename = path.split('/').last().unwrap_or(path);
            let display = format!("{} ({})", filename, key_type);
            
            let style = if i == app.selected_key_index {
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            
            ListItem::new(display).style(style)
        })
        .collect();
    
    let list = List::new(items)
        .block(Block::default()
            .borders(Borders::ALL)
            .title("Select SSH Key - [j/k] Navigate [Enter] Select [q] Quit")
            .border_style(Style::default().fg(Color::Cyan)));
    
    f.render_widget(list, area);
}

fn render_username_input(f: &mut Frame, app: &mut App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),  // Info
            Constraint::Length(3),  // Username input
            Constraint::Min(1),     // Help
        ])
        .margin(2)
        .split(area);
    
    // Show selected key info
    let key_info = if let Some((path, _)) = app.available_keys.get(app.selected_key_index) {
        let filename = path.split('/').last().unwrap_or(path);
        format!("Using key: {}", filename)
    } else {
        "No key selected".to_string()
    };
    
    let info = Paragraph::new(key_info)
        .style(Style::default().fg(Color::Green))
        .alignment(Alignment::Center)
        .block(Block::default().borders(Borders::ALL).title("Selected Key"));
    f.render_widget(info, chunks[0]);
    
    // Username input
    f.render_widget(&app.username_input, chunks[1]);
    
    // Help text
    let help = Paragraph::new("Enter a username for your account.\nPress Enter to register, Esc to go back.")
        .alignment(Alignment::Center)
        .block(Block::default().borders(Borders::ALL).title("Help"));
    f.render_widget(help, chunks[2]);
}

fn render_registration_success(f: &mut Frame, app: &App, area: Rect) {
    let token_display = app.registration_token
        .as_ref()
        .map(|t| {
            if t.len() > 40 {
                format!("{}...", &t[..40])
            } else {
                t.clone()
            }
        })
        .unwrap_or_else(|| "No token".to_string());
    
    let text = Text::from(vec![
        Line::from(""),
        Line::from("Registration Successful!").style(Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
        Line::from(""),
        Line::from("Your authentication token has been saved to:"),
        Line::from("  ~/.config/eurus/token").style(Style::default().fg(Color::Cyan)),
        Line::from(""),
        Line::from(format!("Token: {}", token_display)).style(Style::default().fg(Color::DarkGray)),
        Line::from(""),
        Line::from(""),
        Line::from("Press Enter to continue to eurus"),
    ]);
    
    let paragraph = Paragraph::new(text)
        .alignment(Alignment::Center)
        .block(Block::default()
            .borders(Borders::ALL)
            .title("Registration Complete")
            .border_style(Style::default().fg(Color::Green)));
    f.render_widget(paragraph, area);
}

fn render_help(f: &mut Frame, area: Rect) {
    let text = Text::from(vec![
        Line::from("eurus Help").style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Line::from(""),
        Line::from("COMMANDS").style(Style::default().add_modifier(Modifier::BOLD)),
        Line::from("  :q, :quit, :leave    Quit app or leave room"),
        Line::from("  :qq, :qa, :quit!     Force quit from anywhere"),
        Line::from("  :help, :h            Show this help screen"),
        Line::from("  :users, :u           Show online users in room"),
        Line::from("  :register, :reg      Start registration flow"),
        Line::from("  :list, :l            Show room switcher (in room)"),
        Line::from("  :switch <room>, :s   Switch to room by name"),
        Line::from("  :share, :invite      Generate invite code for current room"),
        Line::from("  :join <code>, :j     Join room using invite code"),
        Line::from("  :rename <name>       Rename current room (owner only)"),
        Line::from("  :delete              Delete current room (owner only)"),
        Line::from("  :transfer <user>     Transfer ownership (owner only)"),
        Line::from("  :dm <username>       Start a direct message chat"),
        Line::from(""),
        Line::from("MAIN MENU").style(Style::default().add_modifier(Modifier::BOLD)),
        Line::from("  c                    Create a new room"),
        Line::from("  j                    Join / browse rooms"),
        Line::from("  :                    Enter command mode"),
        Line::from(""),
        Line::from("ROOM LIST").style(Style::default().add_modifier(Modifier::BOLD)),
        Line::from("  j/k or Up/Down       Navigate rooms"),
        Line::from("  Tab                  Switch public/private tabs"),
        Line::from("  Enter                Join selected room"),
        Line::from("  r                    Refresh room list"),
        Line::from("  Esc                  Go back"),
        Line::from(""),
        Line::from("IN ROOM (Vim Mode)").style(Style::default().add_modifier(Modifier::BOLD)),
        Line::from("  i, a, A, I, o, O     Enter insert mode"),
        Line::from("  Esc                  Exit to normal mode"),
        Line::from("  h/j/k/l              Move cursor"),
        Line::from("  w/b                  Word forward/back"),
        Line::from("  0/$                  Line start/end"),
        Line::from("  gg/G                 Document top/bottom"),
        Line::from("  dd                   Delete line"),
        Line::from("  yy                   Yank (copy) line"),
        Line::from("  p                    Paste"),
        Line::from("  u / Ctrl+r           Undo / Redo"),
        Line::from("  Enter                Send message"),
        Line::from("  Shift+Enter          New line (multi-line msg)"),
        Line::from("  :emoji:              Emoji picker (e.g. :smile:)"),
        Line::from("  Mouse scroll         Scroll message history"),
        Line::from(""),
        Line::from("Press Esc, q, or Enter to close this help"),
    ]);
    
    let paragraph = Paragraph::new(text)
        .block(Block::default()
            .borders(Borders::ALL)
            .title("Help")
            .border_style(Style::default().fg(Color::Cyan)));
    f.render_widget(paragraph, area);
}

fn render_footer(f: &mut Frame, app: &App, area: Rect) {
    // Show command input if in command mode, otherwise show status message
    let (text, style) = if let Some(ref cmd) = app.command_input {
        (format!(":{}", cmd), Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))
    } else {
        let status = &app.status_message;
        
        // Suppress duplicate mode messages in the footer
        if status == "-- NORMAL --" || status == "-- INSERT --" {
            ("".to_string(), Style::default())
        } else if status.to_lowercase().contains("error") || status.to_lowercase().contains("unknown") {
            (status.clone(), Style::default().fg(Color::Red).add_modifier(Modifier::BOLD))
        } else {
            (status.clone(), Style::default().fg(Color::White))
        }
    };
    
    // Render without block to keep it minimal
    let footer_text = Paragraph::new(text)
        .style(style)
        .alignment(Alignment::Left);
        
    f.render_widget(footer_text, area);
}

// --- Terminal Helper Functions ---

fn init_terminal() -> Result<Terminal<CrosstermBackend<io::Stdout>>, Box<dyn Error>> {
    let mut stdout = io::stdout();
    // Removed EnableMouseCapture to allow native terminal selection
    execute!(stdout, EnterAlternateScreen, EnableFocusChange)?;
    enable_raw_mode()?;
    let backend = CrosstermBackend::new(stdout);
    let terminal = Terminal::new(backend)?;
    Ok(terminal)
}

fn restore_terminal(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
) -> Result<(), Box<dyn Error>> {
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen
        // Removed DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    Ok(())
}

