mod api;
mod crypto;
mod clipboard;
mod config;
mod vim;

use crate::crypto::{decrypt, encrypt, AesKey};
use crate::clipboard::ClipboardManager;
use crate::config::Config;
use crate::vim::{VimMode, VimState};
use api::{ClientMessage, CreateRoomPayload, JoinRoomPayload, ListRoomsPayload, SendMessagePayload, ServerMessage, RoomInfo};
use futures_util::{SinkExt, StreamExt};
use ratatui::{
    crossterm::{
        event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind},
        execute,
        terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    },
    prelude::*,
    widgets::*,
};
use std::{error::Error, io};
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tui_textarea::TextArea;

// --- Application State ---

#[derive(PartialEq)]
enum CurrentScreen {
    RoomChoice,
    RoomList,
    RoomCreation,
    CreateRoomInput,
    RoomJoining,
    InRoom,
    QuitConfirmation,
}

enum CurrentlyEditing {
    RoomName,
    RoomKey,
}

struct App<'a> {
    // Inputs
    room_name_input: TextArea<'a>,
    room_key_input: TextArea<'a>,
    message_input: TextArea<'a>,

    // State
    current_screen: CurrentScreen,
    currently_editing: Option<CurrentlyEditing>,
    status_message: String,
    should_quit: bool,
    vim_state: VimState,
    message_scroll_offset: usize,

    // Room Data
    room_id: Option<String>,
    room_name: Option<String>,
    room_key: Option<AesKey>,
    messages: Vec<String>,
    
    // Room List
    public_rooms: Vec<RoomInfo>,
    private_rooms: Vec<RoomInfo>,
    selected_room_index: usize,
    viewing_private: bool,

    // WebSocket
    ws_sender: Option<mpsc::UnboundedSender<String>>,
    reconnect_attempts: usize,
    is_reconnecting: bool,
    
    // Clipboard & Config
    clipboard: Option<ClipboardManager>,
    config: Config,
}

impl<'a> Default for App<'a> {
    fn default() -> Self {
        let mut room_name_input = TextArea::default();
        room_name_input.set_placeholder_text("Enter room name (e.g., general)...");
        room_name_input.set_block(Block::default().borders(Borders::ALL).title("Room Name"));

        let mut room_key_input = TextArea::default();
        room_key_input.set_placeholder_text("Enter the secret Room Key...");
        room_key_input.set_block(Block::default().borders(Borders::ALL).title("Room Key"));

        let mut message_input = TextArea::default();
        message_input.set_placeholder_text("Type your encrypted message...");
        message_input.set_block(Block::default().borders(Borders::ALL).title("Message"));

        let clipboard = ClipboardManager::new().ok();
        if clipboard.is_none() {
            eprintln!("Warning: Failed to initialize clipboard");
        }

        App {
            room_name_input,
            room_key_input,
            message_input,
            current_screen: CurrentScreen::RoomChoice,
            currently_editing: None,
            status_message: "Create or Join a secure room.".to_string(),
            should_quit: false,
            room_id: None,
            room_name: None,
            room_key: None,
            messages: Vec::new(),
            public_rooms: Vec::new(),
            private_rooms: Vec::new(),
            selected_room_index: 0,
            viewing_private: false,
            ws_sender: None,
            reconnect_attempts: 0,
            is_reconnecting: false,
            clipboard,
            config: Config::load(),
            vim_state: VimState::default(),
            message_scroll_offset: 0,
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

    // Establish WebSocket connection at startup
    if let Ok(()) = establish_connection(app, ws_incoming_tx.clone()).await {
        app.status_message = "Connected! Create or Join a secure room.".to_string();
    } else {
        app.status_message = "Failed to connect to server. Check if server is running.".to_string();
    }

    loop {
        terminal.draw(|f| ui(f, app))?;

        if app.should_quit {
            break;
        }

        // Handle incoming WebSocket messages without blocking UI
        if let Ok(text) = ws_incoming_rx.try_recv() {
            // Check for disconnect signal
            if text == "__DISCONNECT__" {
                app.messages.push("[SYSTEM] Connection lost. Attempting to reconnect...".to_string());
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
                                app.messages.push("[SYSTEM] Reconnected successfully!".to_string());
                            }
                            Err(_) => {
                                app.messages.push("[SYSTEM] Failed to reconnect. Please restart.".to_string());
                            }
                        }
                    }
                }
            } else {
                match serde_json::from_str::<ServerMessage>(&text) {
                    Ok(server_msg) => handle_server_message(app, server_msg),
                    Err(_) => {
                        // Raw info messages from the server
                        app.messages.push(format!("[SERVER] {}", text));
                    }
                };
            }
        }

        // Handle user input
        if event::poll(std::time::Duration::from_millis(50))? {
            let event = event::read()?;
            match event {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    // Global handler for Ctrl+Esc to show quit confirmation
                    if key.code == KeyCode::Esc && key.modifiers.contains(KeyModifiers::CONTROL) {
                        app.current_screen = CurrentScreen::QuitConfirmation;
                        continue;
                    }
                    
                    match app.current_screen {
                        CurrentScreen::RoomChoice => handle_room_choice_screen(app, key).await,
                        CurrentScreen::RoomList => handle_room_list_screen(app, key, ws_incoming_tx.clone()).await,
                        CurrentScreen::CreateRoomInput => {
                            handle_create_room_input_screen(app, key, ws_incoming_tx.clone()).await
                        }
                        CurrentScreen::RoomCreation => {
                            handle_room_creation_screen(app, key, ws_incoming_tx.clone()).await
                        }
                        CurrentScreen::RoomJoining => {
                            handle_room_joining_screen(app, key, ws_incoming_tx.clone()).await
                        }
                        CurrentScreen::InRoom => handle_in_room_screen(app, key).await,
                        CurrentScreen::QuitConfirmation => handle_quit_confirmation_screen(app, key),
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

async fn handle_room_choice_screen(app: &mut App<'_>, key: event::KeyEvent) {
    match key.code {
        KeyCode::Char('c') | KeyCode::Char('C') => {
            app.current_screen = CurrentScreen::CreateRoomInput;
            app.currently_editing = Some(CurrentlyEditing::RoomName);
            app.status_message = "Enter a name for your new room (e.g., team-chat)".to_string();
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
                let create_payload = CreateRoomPayload {
                    name: &room_name,
                    display_name: None,
                    room_type: "public",
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
    if let KeyCode::Enter = key.code {
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
        }
    }
}

async fn handle_room_joining_screen(
    app: &mut App<'_>,
    key: event::KeyEvent,
    ws_tx: mpsc::UnboundedSender<String>,
) {
    match key.code {
        KeyCode::Enter => {
            let room_name = app.room_name_input.lines().join("");
            let room_key_hex = app.room_key_input.lines().join("");

            if room_name.is_empty() || room_key_hex.is_empty() {
                app.status_message = "Both room name and key are required.".to_string();
                return;
            }

            let key_bytes = match hex::decode(room_key_hex) {
                Ok(bytes) => bytes,
                Err(_) => {
                    app.status_message = "Invalid room key format. Must be hex.".to_string();
                    return;
                }
            };

            let key = AesKey::from_slice(&key_bytes);

            // Set state before attempting to connect
            app.room_name = Some(room_name.clone());
            app.room_key = Some(*key);

            app.status_message = "Connecting...".to_string();
            if connect_to_room_by_name(app, ws_tx, &room_name).await.is_ok() {
                app.current_screen = CurrentScreen::InRoom;
                app.status_message = format!("Connected to room: #{}", room_name);
            } else {
                app.status_message =
                    "Failed to connect. Check room name/key and network.".to_string();
                // Clear invalid state
                app.room_name = None;
                app.room_key = None;
            }
        }
        KeyCode::Tab => {
            app.currently_editing = match app.currently_editing {
                Some(CurrentlyEditing::RoomName) => Some(CurrentlyEditing::RoomKey),
                _ => Some(CurrentlyEditing::RoomName),
            };
        }
        KeyCode::Esc => {
            app.current_screen = CurrentScreen::RoomChoice;
            app.status_message = "Create or Join a secure room.".to_string();
            app.room_name_input.move_cursor(tui_textarea::CursorMove::End);
            app.room_key_input
                .move_cursor(tui_textarea::CursorMove::End);
        }
        _ => {
            let input = Event::Key(key);
            match app.currently_editing {
                Some(CurrentlyEditing::RoomName) => {
                    app.room_name_input.input(input);
                }
                Some(CurrentlyEditing::RoomKey) => {
                    app.room_key_input.input(input);
                }
                _ => {}
            };
        }
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
        VimMode::Visual => handle_visual_mode(app, key).await,
    }
}

async fn handle_normal_mode(app: &mut App<'_>, key: event::KeyEvent) {
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
        
        // Enter Visual mode
        KeyCode::Char('v') => {
            app.vim_state.enter_visual_mode();
            app.status_message = "-- VISUAL --".to_string();
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

        // Command mode
        KeyCode::Char(':') => {
            // TODO: Implement command mode for :q, :q!, :w, etc.
            app.status_message = "Command mode not yet implemented (use Ctrl+Esc to quit)".to_string();
        }

        _ => {
            // Clear any pending commands
            app.vim_state.reset();
        }
    }
}

async fn handle_insert_mode(app: &mut App<'_>, key: event::KeyEvent) {
    match key.code {
        KeyCode::Esc => {
            // Exit Insert mode back to Normal mode
            app.vim_state.enter_normal_mode();
            app.status_message = "-- NORMAL --".to_string();
        }
        KeyCode::Enter => {
            // In Insert mode, Enter sends the message
            send_message(app).await;
        }
        _ => {
            // Pass all other keys to the text input
            app.message_input.input(Event::Key(key));
        }
    }
}

async fn handle_visual_mode(app: &mut App<'_>, key: event::KeyEvent) {
    match key.code {
        KeyCode::Esc | KeyCode::Char('v') => {
            // Exit Visual mode back to Normal mode
            app.vim_state.enter_normal_mode();
            app.status_message = "-- NORMAL --".to_string();
        }
        // Navigation works same as Normal mode
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
        // Yank selection
        KeyCode::Char('y') => {
            // TODO: Implement visual selection yank
            app.status_message = "Visual yank not yet implemented".to_string();
            app.vim_state.enter_normal_mode();
        }
        // Delete selection
        KeyCode::Char('d') | KeyCode::Char('x') => {
            // TODO: Implement visual selection delete
            app.status_message = "Visual delete not yet implemented".to_string();
            app.vim_state.enter_normal_mode();
        }
        _ => {}
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
        }
    }
}


fn handle_quit_confirmation_screen(app: &mut App<'_>, key: event::KeyEvent) {
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') => {
            app.should_quit = true;
        }
        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
            // Return to the previous screen (InRoom if in a room, otherwise RoomChoice)
            if app.room_id.is_some() {
                app.current_screen = CurrentScreen::InRoom;
            } else {
                app.current_screen = CurrentScreen::RoomChoice;
            }
        }
        _ => {}
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
                        let formatted = format!("[{}] {}", payload.username, plaintext);
                        app.messages.push(formatted);
                    }
                    Err(_) => app
                        .messages
                        .push("[DECRYPTION_ERROR] Received invalid message".to_string()),
                }
            }
        }
        ServerMessage::UserJoined(payload) => {
            app.messages
                .push(format!("→ {} joined the room", payload.username));
        }
        ServerMessage::UserLeft(payload) => {
            app.messages
                .push(format!("← {} left the room", payload.username));
        }
        ServerMessage::RoomJoined(payload) => {
            app.status_message = format!("Joined room: {}", payload.display_name);
            
            // Store room info
            app.room_id = Some(payload.room_id.clone());
            app.room_name = Some(payload.room_name.clone());
            
            // Store the room key from the server
            if !payload.encrypted_key.is_empty() {
                // For now, use the hex key directly as the AES key
                // In production, this would be encrypted per-user
                if let Ok(key_bytes) = hex::decode(&payload.encrypted_key) {
                    if key_bytes.len() == 32 {
                        use crate::crypto::AesKey;
                        app.room_key = Some(*AesKey::from_slice(&key_bytes));
                    }
                }
            }
            
            // Load message history
            if let Some(key) = &app.room_key {
                for msg in payload.messages {
                    match decrypt(key, &msg.ciphertext) {
                        Ok(plaintext) => {
                            let formatted = format!("[{}] {}", msg.username, plaintext);
                            app.messages.push(formatted);
                        }
                        Err(_) => app
                            .messages
                            .push("[DECRYPTION_ERROR] Could not decrypt old message".to_string()),
                    }
                }
            }
        }
        ServerMessage::RoomCreated(payload) => {
            app.status_message = format!("Room created: {}", payload.display_name);
            app.room_id = Some(payload.room_id);
            app.room_name = Some(payload.room_name);
            
            // Store the room key from the server
            if !payload.encrypted_key.is_empty() {
                if let Ok(key_bytes) = hex::decode(&payload.encrypted_key) {
                    if key_bytes.len() == 32 {
                        use crate::crypto::AesKey;
                        app.room_key = Some(*AesKey::from_slice(&key_bytes));
                    }
                }
            }
            
            app.messages = vec![
                format!("Room {} created successfully!", payload.display_name),
                "".to_string(),
                "Press Enter to join the room".to_string(),
            ];
        }
        ServerMessage::RoomsList(payload) => {
            app.public_rooms = payload.public_rooms;
            app.private_rooms = payload.private_rooms;
            app.selected_room_index = 0;
            app.status_message = format!(
                "{} public, {} private rooms. Use ↑↓ to navigate, Enter to join, Tab to switch, R to refresh",
                app.public_rooms.len(),
                app.private_rooms.len()
            );
        }
        ServerMessage::Info(payload) => {
            app.messages.push(format!("[SERVER] {}", payload.message));
        }
        ServerMessage::Error(payload) => {
            app.status_message = format!("[ERROR] {}", payload.message);
            app.messages.push(format!("[ERROR] {}", payload.message));
        }
    }
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
    let ws_url = &app.config.server.url;
    let (ws_stream, _) = connect_async(ws_url).await?;
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

    // Task to send outgoing messages from the app to the server
    tokio::spawn(async move {
        while let Some(json) = ws_outgoing_rx.recv().await {
            if write.send(Message::text(json)).await.is_err() {
                break;
            }
        }
    });

    Ok(())
}

async fn connect(
    app: &mut App<'_>,
    ws_incoming_tx: mpsc::UnboundedSender<String>,
) -> Result<(), Box<dyn Error>> {
    let room_id = app.room_id.as_ref().ok_or("Room ID not set")?;

    let (ws_stream, _) = connect_async(&app.config.server.url).await?;
    let (mut write, mut read) = ws_stream.split();

    // Create a channel for sending messages to the WebSocket task
    let (ws_outgoing_tx, mut ws_outgoing_rx) = mpsc::unbounded_channel::<String>();
    app.ws_sender = Some(ws_outgoing_tx);

    // Join the room
    let join_payload = JoinRoomPayload {
        room_id: Some(room_id),
        room_name: None,
    };
    let join_message = ClientMessage {
        message_type: "joinRoom",
        payload: join_payload,
    };
    let message_json = serde_json::to_string(&join_message)?;
    write.send(Message::text(message_json)).await?;

    // Task to listen for incoming messages from the server
    tokio::spawn(async move {
        while let Some(message_result) = read.next().await {
            match message_result {
                Ok(msg) => {
                    if let Message::Text(text) = msg {
                        if ws_incoming_tx.send(text).is_err() {
                            // Main app has closed, exit the loop
                            break;
                        }
                    }
                }
                Err(_) => {
                    // Connection closed or error
                    let _ = ws_incoming_tx.send("Connection to server lost.".to_string());
                    break;
                }
            }
        }
    });

    // Task to send outgoing messages from the app to the server
    tokio::spawn(async move {
        while let Some(json) = ws_outgoing_rx.recv().await {
            if write.send(Message::text(json)).await.is_err() {
                break;
            }
        }
    });

    Ok(())
}

async fn connect_to_room_by_name(
    app: &mut App<'_>,
    ws_incoming_tx: mpsc::UnboundedSender<String>,
    room_name: &str,
) -> Result<(), Box<dyn Error>> {
    let (ws_stream, _) = connect_async(&app.config.server.url).await?;
    let (mut write, mut read) = ws_stream.split();

    // Create a channel for sending messages to the WebSocket task
    let (ws_outgoing_tx, mut ws_outgoing_rx) = mpsc::unbounded_channel::<String>();
    app.ws_sender = Some(ws_outgoing_tx.clone());

    // Join the room by name
    let join_payload = JoinRoomPayload {
        room_id: None,
        room_name: Some(room_name),
    };
    let join_message = ClientMessage {
        message_type: "joinRoom",
        payload: join_payload,
    };
    let message_json = serde_json::to_string(&join_message)?;
    write.send(Message::text(message_json)).await?;

    // Request room list
    let list_message = ClientMessage {
        message_type: "listRooms",
        payload: ListRoomsPayload {},
    };
    let list_json = serde_json::to_string(&list_message)?;
    let _ = ws_outgoing_tx.send(list_json);

    // Task to listen for incoming messages from the server
    tokio::spawn(async move {
        while let Some(message_result) = read.next().await {
            match message_result {
                Ok(msg) => {
                    if let Message::Text(text) = msg {
                        if ws_incoming_tx.send(text).is_err() {
                            break;
                        }
                    }
                }
                Err(_) => {
                    let _ = ws_incoming_tx.send("Connection to server lost.".to_string());
                    break;
                }
            }
        }
    });

    // Task to send outgoing messages from the app to the server
    tokio::spawn(async move {
        while let Some(json) = ws_outgoing_rx.recv().await {
            if write.send(Message::text(json)).await.is_err() {
                break;
            }
        }
    });

    Ok(())
}

async fn create_room(
    app: &mut App<'_>,
    ws_incoming_tx: mpsc::UnboundedSender<String>,
    room_name: &str,
    room_type: &str,
) -> Result<(), Box<dyn Error>> {
    let (ws_stream, _) = connect_async(&app.config.server.url).await?;
    let (mut write, mut read) = ws_stream.split();

    // Create a channel for sending messages to the WebSocket task
    let (ws_outgoing_tx, mut ws_outgoing_rx) = mpsc::unbounded_channel::<String>();
    app.ws_sender = Some(ws_outgoing_tx);

    // Create the room
    let create_payload = CreateRoomPayload {
        name: room_name,
        display_name: None,
        room_type,
    };
    let create_message = ClientMessage {
        message_type: "createRoom",
        payload: create_payload,
    };
    let message_json = serde_json::to_string(&create_message)?;
    write.send(Message::text(message_json)).await?;

    // Task to listen for incoming messages from the server
    tokio::spawn(async move {
        while let Some(message_result) = read.next().await {
            match message_result {
                Ok(msg) => {
                    if let Message::Text(text) = msg {
                        if ws_incoming_tx.send(text).is_err() {
                            break;
                        }
                    }
                }
                Err(_) => {
                    let _ = ws_incoming_tx.send("Connection to server lost.".to_string());
                    break;
                }
            }
        }
    });

    // Task to send outgoing messages from the app to the server
    tokio::spawn(async move {
        while let Some(json) = ws_outgoing_rx.recv().await {
            if write.send(Message::text(json)).await.is_err() {
                break;
            }
        }
    });

    Ok(())
}

// --- UI Rendering ---

fn ui(f: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Title
            Constraint::Min(1),    // Main content
            Constraint::Length(3), // Footer
        ])
        .split(f.area());

    let title = Paragraph::new("RadioChat :: E2EE")
        .style(Style::default().fg(Color::LightCyan))
        .alignment(Alignment::Center)
        .block(Block::default().borders(Borders::ALL));
    f.render_widget(title, chunks[0]);

    match app.current_screen {
        CurrentScreen::RoomChoice => render_room_choice(f, chunks[1]),
        CurrentScreen::RoomList => render_room_list(f, app, chunks[1]),
        CurrentScreen::CreateRoomInput => render_create_room_input(f, app, chunks[1]),
        CurrentScreen::RoomCreation => render_room_creation(f, app, chunks[1]),
        CurrentScreen::RoomJoining => render_room_joining(f, app, chunks[1]),
        CurrentScreen::InRoom => render_in_room(f, app, chunks[1]),
        CurrentScreen::QuitConfirmation => render_quit_confirmation(f, chunks[1]),
    }

    render_footer(f, app, chunks[2]);
}

fn render_room_choice(f: &mut Frame, area: Rect) {
    let text = Text::from(vec![
        Line::from("Welcome to RadioChat - E2EE Chat"),
        Line::from(""),
        Line::from("Press 'c' to CREATE a new room"),
        Line::from("Press 'j' to JOIN / browse rooms"),
        Line::from(""),
        Line::from("Press 'Ctrl+Esc' to quit"),
    ]);
    let widget = Paragraph::new(text).alignment(Alignment::Center).block(
        Block::default()
            .borders(Borders::ALL)
            .title("Choose an Action"),
    );
    f.render_widget(widget, area);
}

fn render_room_list(f: &mut Frame, app: &App, area: Rect) {
    let rooms_to_display = if app.viewing_private {
        &app.private_rooms
    } else {
        &app.public_rooms
    };
    
    let room_type = if app.viewing_private { "PRIVATE" } else { "PUBLIC" };
    
    let items: Vec<ListItem> = rooms_to_display
        .iter()
        .enumerate()
        .map(|(i, room)| {
            let style = if i == app.selected_room_index {
                Style::default().bg(Color::DarkGray).fg(Color::White)
            } else {
                Style::default()
            };
            let content = format!(
                "{} ({} members)",
                room.display_name,
                room.member_count
            );
            ListItem::new(content).style(style)
        })
        .collect();
    
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!("{} Rooms - Use ↑↓ to navigate, Enter to join, Tab to switch, R to refresh, Esc to go back", room_type))
        );
    
    f.render_widget(list, area);
}

fn render_create_room_input(f: &mut Frame, app: &mut App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(5), Constraint::Min(1)])
        .margin(2)
        .split(area);
    
    f.render_widget(&app.room_name_input, chunks[0]);
    
    let help_text = Paragraph::new("Enter a room name (e.g., 'general', 'team-chat')\nPress Enter to create, Esc to cancel")
        .alignment(Alignment::Center)
        .block(Block::default().borders(Borders::ALL).title("Help"));
    f.render_widget(help_text, chunks[1]);
}

fn render_room_creation(f: &mut Frame, app: &mut App, area: Rect) {
    let text: Vec<Line> = app.messages.iter().map(|s| Line::from(s.clone())).collect();
    let widget = Paragraph::new(Text::from(text))
        .alignment(Alignment::Center)
        .wrap(Wrap { trim: true })
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Room Credentials"),
        );
    f.render_widget(widget, area);
}

fn render_room_joining(f: &mut Frame, app: &mut App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Length(3)])
        .margin(2)
        .split(area);

    // Highlight active field
    app.room_name_input.set_style(Style::default());
    app.room_key_input.set_style(Style::default());
    match app.currently_editing {
        Some(CurrentlyEditing::RoomName) => app
            .room_name_input
            .set_style(Style::default().fg(Color::Yellow)),
        Some(CurrentlyEditing::RoomKey) => app
            .room_key_input
            .set_style(Style::default().fg(Color::Yellow)),
        _ => {}
    }

    f.render_widget(&app.room_name_input, chunks[0]);
    f.render_widget(&app.room_key_input, chunks[1]);
}

fn render_in_room(f: &mut Frame, app: &mut App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(3)])
        .split(area);

    let messages: Vec<ListItem> = app
        .messages
        .iter()
        .map(|m| ListItem::new(m.clone()))
        .collect();
    let messages_list = List::new(messages).block(Block::default().borders(Borders::ALL).title(
        format!("Room: {}", app.room_id.as_deref().unwrap_or("Unknown")),
    ));
    f.render_widget(messages_list, chunks[0]);

    // Update message input block to show vim mode
    let vim_mode_str = app.vim_state.mode.as_str();
    let title = format!("Message [{}]", vim_mode_str);
    app.message_input.set_block(
        Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(match app.vim_state.mode {
                VimMode::Normal => Style::default().fg(Color::Cyan),
                VimMode::Insert => Style::default().fg(Color::Green),
                VimMode::Visual => Style::default().fg(Color::Yellow),
            })
    );
    f.render_widget(&app.message_input, chunks[1]);
}

fn render_quit_confirmation(f: &mut Frame, area: Rect) {
    // Center the dialog
    let dialog_area = centered_rect(60, 30, area);
    
    let text = Text::from(vec![
        Line::from(""),
        Line::from(""),
        Line::from("Are you sure you want to quit?").alignment(Alignment::Center),
        Line::from(""),
        Line::from("Press 'y' to quit").alignment(Alignment::Center),
        Line::from("Press 'n' or Esc to cancel").alignment(Alignment::Center),
    ]);
    
    let widget = Paragraph::new(text)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Quit Confirmation")
                .border_style(Style::default().fg(Color::Red))
        )
        .alignment(Alignment::Center);
    
    // Clear the background
    f.render_widget(Clear, dialog_area);
    f.render_widget(widget, dialog_area);
}

fn render_footer(f: &mut Frame, app: &App, area: Rect) {
    let footer_text = Paragraph::new(app.status_message.as_str())
        .style(Style::default().fg(Color::Yellow))
        .alignment(Alignment::Center)
        .block(Block::default().borders(Borders::ALL).title("Status"));
    f.render_widget(footer_text, area);
}

// --- Terminal Helper Functions ---

fn init_terminal() -> Result<Terminal<CrosstermBackend<io::Stdout>>, Box<dyn Error>> {
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
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
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    Ok(())
}

/// Helper function to create a centered rectangle for dialogs
fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

