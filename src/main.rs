mod api;
mod crypto;

use crate::crypto::{decrypt, encrypt, generate_key, AesKey};
use api::{ClientMessage, JoinRoomPayload, SendMessagePayload, ServerMessage};
use futures_util::{SinkExt, StreamExt};
use ratatui::{
    crossterm::{
        event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind},
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

enum CurrentScreen {
    RoomChoice,
    RoomCreation,
    RoomJoining,
    InRoom,
}

enum CurrentlyEditing {
    RoomId,
    RoomKey,
}

struct App<'a> {
    // Inputs
    room_id_input: TextArea<'a>,
    room_key_input: TextArea<'a>,
    message_input: TextArea<'a>,

    // State
    current_screen: CurrentScreen,
    currently_editing: Option<CurrentlyEditing>,
    status_message: String,
    should_quit: bool,

    // Room Data
    room_id: Option<String>,
    room_key: Option<AesKey>,
    messages: Vec<String>,

    // WebSocket
    ws_sender: Option<mpsc::UnboundedSender<String>>,
}

impl<'a> Default for App<'a> {
    fn default() -> Self {
        let mut room_id_input = TextArea::default();
        room_id_input.set_placeholder_text("Enter the Room ID...");
        room_id_input.set_block(Block::default().borders(Borders::ALL).title("Room ID"));

        let mut room_key_input = TextArea::default();
        room_key_input.set_placeholder_text("Enter the secret Room Key...");
        room_key_input.set_block(Block::default().borders(Borders::ALL).title("Room Key"));

        let mut message_input = TextArea::default();
        message_input.set_placeholder_text("Type your encrypted message...");
        message_input.set_block(Block::default().borders(Borders::ALL).title("Message"));

        App {
            room_id_input,
            room_key_input,
            message_input,
            current_screen: CurrentScreen::RoomChoice,
            currently_editing: None,
            status_message: "Create or Join a secure room.".to_string(),
            should_quit: false,
            room_id: None,
            room_key: None,
            messages: Vec::new(),
            ws_sender: None,
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

    loop {
        terminal.draw(|f| ui(f, app))?;

        if app.should_quit {
            break;
        }

        // Handle incoming WebSocket messages without blocking UI
        if let Ok(text) = ws_incoming_rx.try_recv() {
            match serde_json::from_str::<ServerMessage>(&text) {
                Ok(server_msg) => handle_server_message(app, server_msg),
                Err(_) => {
                    // Raw info messages from the server
                    app.messages.push(format!("[SERVER] {}", text));
                }
            };
        }

        // Handle user input
        if event::poll(std::time::Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    match app.current_screen {
                        CurrentScreen::RoomChoice => handle_room_choice_screen(app, key).await,
                        CurrentScreen::RoomCreation => {
                            handle_room_creation_screen(app, key, ws_incoming_tx.clone()).await
                        }
                        CurrentScreen::RoomJoining => {
                            handle_room_joining_screen(app, key, ws_incoming_tx.clone()).await
                        }
                        CurrentScreen::InRoom => handle_in_room_screen(app, key).await,
                    }
                }
            }
        }
    }
    Ok(())
}

// --- Screen & Input Handlers ---

async fn handle_room_choice_screen(app: &mut App<'_>, key: event::KeyEvent) {
    match key.code {
        KeyCode::Char('c') | KeyCode::Char('C') => {
            let new_room_id = hex::encode(rand::random::<[u8; 16]>());
            let new_key = generate_key();

            app.status_message =
                format!("New Room Created! SAVE THESE DETAILS. Press Enter to continue.");
            app.messages = vec![
                "IMPORTANT: Share this Room ID and Key SECURELY with others.".to_string(),
                "There is no way to recover them.".to_string(),
                "".to_string(),
                format!("Room ID: {}", new_room_id),
                format!("Room Key: {}", hex::encode(new_key.as_slice())),
            ];

            app.room_id = Some(new_room_id);
            app.room_key = Some(new_key);
            app.current_screen = CurrentScreen::RoomCreation;
        }
        KeyCode::Char('j') | KeyCode::Char('J') => {
            app.current_screen = CurrentScreen::RoomJoining;
            app.currently_editing = Some(CurrentlyEditing::RoomId);
            app.status_message = "Enter Room ID and Key to join.".to_string();
        }
        KeyCode::Esc => {
            app.should_quit = true;
        }
        _ => {}
    }
}

async fn handle_room_creation_screen(
    app: &mut App<'_>,
    key: event::KeyEvent,
    ws_tx: mpsc::UnboundedSender<String>,
) {
    if let KeyCode::Enter = key.code {
        app.status_message = "Connecting...".to_string();
        if connect(app, ws_tx).await.is_ok() {
            app.messages.clear();
            app.current_screen = CurrentScreen::InRoom;
            app.status_message = "Connected to room!".to_string();
        } else {
            app.status_message = "Failed to connect.".to_string();
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
            let room_id = app.room_id_input.lines().join("");
            let room_key_hex = app.room_key_input.lines().join("");

            if room_id.is_empty() || room_key_hex.is_empty() {
                app.status_message = "Both Room ID and Key are required.".to_string();
                return;
            }

            let key_bytes = match hex::decode(room_key_hex) {
                Ok(bytes) => bytes,
                Err(_) => {
                    app.status_message = "Invalid Room Key format. Must be hex.".to_string();
                    return;
                }
            };

            let key = AesKey::from_slice(&key_bytes);

            // Set state before attempting to connect
            app.room_id = Some(room_id);
            app.room_key = Some(*key);

            app.status_message = "Connecting...".to_string();
            if connect(app, ws_tx).await.is_ok() {
                app.current_screen = CurrentScreen::InRoom;
                app.status_message = "Connected to room!".to_string();
            } else {
                app.status_message =
                    "Failed to connect. Check Room ID/Key and network.".to_string();
                // Clear invalid state
                app.room_id = None;
                app.room_key = None;
            }
        }
        KeyCode::Tab => {
            app.currently_editing = match app.currently_editing {
                Some(CurrentlyEditing::RoomId) => Some(CurrentlyEditing::RoomKey),
                _ => Some(CurrentlyEditing::RoomId),
            };
        }
        KeyCode::Esc => {
            app.current_screen = CurrentScreen::RoomChoice;
            app.status_message = "Create or Join a secure room.".to_string();
            app.room_id_input.move_cursor(tui_textarea::CursorMove::End);
            app.room_key_input
                .move_cursor(tui_textarea::CursorMove::End);
        }
        _ => {
            let input = Event::Key(key);
            match app.currently_editing {
                Some(CurrentlyEditing::RoomId) => {
                    app.room_id_input.input(input);
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
    match key.code {
        KeyCode::Enter => {
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
                                typ: "message",
                                payload,
                            };
                            if let Ok(json) = serde_json::to_string(&msg) {
                                if sender.send(json).is_ok() {
                                    app.message_input = TextArea::default();
                                    app.message_input
                                        .set_placeholder_text("Type your encrypted message...");
                                    app.message_input.set_block(
                                        Block::default().borders(Borders::ALL).title("Message"),
                                    );
                                } else {
                                    app.status_message =
                                        "Connection lost. Restart to reconnect.".to_string();
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
        KeyCode::Esc => {
            app.should_quit = true; // For simplicity, Esc now quits the app.
        }
        _ => {
            app.message_input.input(Event::Key(key));
        }
    }
}

// --- WebSocket & Message Handling ---

fn handle_server_message(app: &mut App, msg: ServerMessage) {
    match msg {
        ServerMessage::Message(payload) => {
            if let Some(key) = &app.room_key {
                match decrypt(key, &payload.ciphertext) {
                    Ok(plaintext) => app.messages.push(plaintext),
                    Err(_) => app
                        .messages
                        .push("[DECRYPTION_ERROR] Received invalid message".to_string()),
                }
            }
        }
        ServerMessage::Info(payload) => {
            app.messages.push(format!("[SERVER] {}", payload.message));
        }
        ServerMessage::Error(payload) => {
            app.status_message = format!("[ERROR] {}", payload.message);
        }
    }
}

async fn connect(
    app: &mut App<'_>,
    ws_incoming_tx: mpsc::UnboundedSender<String>,
) -> Result<(), Box<dyn Error>> {
    let room_id = app.room_id.as_ref().ok_or("Room ID not set")?;

    let (ws_stream, _) = connect_async("ws://tunnel.sreus.tech:8080").await?;
    let (mut write, mut read) = ws_stream.split();

    // Create a channel for sending messages to the WebSocket task
    let (ws_outgoing_tx, mut ws_outgoing_rx) = mpsc::unbounded_channel::<String>();
    app.ws_sender = Some(ws_outgoing_tx);

    // Join the room
    let join_payload = JoinRoomPayload { room_id };
    let join_message = ClientMessage {
        typ: "joinRoom",
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
        CurrentScreen::RoomCreation => render_room_creation(f, app, chunks[1]),
        CurrentScreen::RoomJoining => render_room_joining(f, app, chunks[1]),
        CurrentScreen::InRoom => render_in_room(f, app, chunks[1]),
    }

    render_footer(f, app, chunks[2]);
}

fn render_room_choice(f: &mut Frame, area: Rect) {
    let text = Text::from(vec![
        Line::from("Welcome to the secure zone."),
        Line::from(""),
        Line::from("Press 'c' to CREATE a new encrypted room."),
        Line::from("Press 'j' to JOIN an existing room."),
        Line::from(""),
        Line::from("Press 'Esc' to quit."),
    ]);
    let widget = Paragraph::new(text).alignment(Alignment::Center).block(
        Block::default()
            .borders(Borders::ALL)
            .title("Choose an Action"),
    );
    f.render_widget(widget, area);
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
    app.room_id_input.set_style(Style::default());
    app.room_key_input.set_style(Style::default());
    match app.currently_editing {
        Some(CurrentlyEditing::RoomId) => app
            .room_id_input
            .set_style(Style::default().fg(Color::Yellow)),
        Some(CurrentlyEditing::RoomKey) => app
            .room_key_input
            .set_style(Style::default().fg(Color::Yellow)),
        _ => {}
    }

    f.render_widget(&app.room_id_input, chunks[0]);
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

    f.render_widget(&app.message_input, chunks[1]);
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

