mod api;

use api::{
    ClientMessage, CreateRoomPayload, JoinRoomPayload, LoginPayload, MessagePayload,
    MessageWithAuthor, RegisterPayload, RoomInfo, ServerMessage,
};
use futures_util::{SinkExt, StreamExt};
use ratatui::{
    crossterm::{
        event::{
            self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind,
            KeyModifiers,
        },
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

#[derive(Clone)]
enum CurrentScreen {
    AuthChoice,
    Login,
    Signup,
    Main,
    RoomCreation,
    RoomJoining,
    RoomSelector,
}

#[derive(Clone)]
enum CurrentlyEditing {
    Username,
    Password,
    ConfirmPassword,
    RoomName,
    RoomId,
    Message,
}

#[derive(Clone)]
struct RoomInfo {
    id: String,
    name: String,
}

struct App<'a> {
    // Auth inputs
    username_input: TextArea<'a>,
    password_input: TextArea<'a>,
    confirm_password_input: TextArea<'a>,

    // Room inputs
    room_id_input: TextArea<'a>,
    room_name_input: TextArea<'a>,
    message_input: TextArea<'a>,

    // States
    current_screen: CurrentScreen,
    currently_editing: CurrentlyEditing,
    jwt: Option<String>,
    username: Option<String>,
    status_message: String,
    should_quit: bool,

    // room management
    joined_rooms: Vec<RoomInfo>, // List of rooms user is member of
    room_selector_index: usize,

    // Room Data
    current_room: Option<RoomInfo>,
    messages: Vec<MessageWithAuthor>,

    // WebSocket
    ws_sender: Option<mpsc::UnboundedSender<String>>,
}

impl<'a> Default for App<'a> {
    fn default() -> Self {
        let mut username_input = TextArea::default();
        username_input.set_placeholder_text("Enter username...");
        username_input.set_block(Block::default().borders(Borders::ALL).title("Username"));

        let mut password_input = TextArea::default();
        password_input.set_placeholder_text("Enter password...");
        password_input.set_block(Block::default().borders(Borders::ALL).title("Password"));
        password_input.set_style(Style::default().add_modifier(Modifier::HIDDEN));

        let mut confirm_password_input = TextArea::default();
        confirm_password_input.set_placeholder_text("Confirm password...");
        confirm_password_input.set_block(
            Block::default()
                .borders(Borders::ALL)
                .title("Confirm Password"),
        );
        confirm_password_input.set_style(Style::default().add_modifier(Modifier::HIDDEN));

        let mut room_name_input = TextArea::default();
        room_name_input.set_placeholder_text("Enter a valid room name...");
        room_name_input.set_block(Block::default().borders(Borders::ALL).title("Room Name"));

        let mut room_id_input = TextArea::default();
        room_id_input.set_placeholder_text("Enter Room Id...");
        room_id_input.set_block(Block::default().borders(Borders::ALL).title("Room ID"));

        let mut message_input = TextArea::default();
        message_input.set_placeholder_text("Type your message...");
        message_input.set_block(Block::default().borders(Borders::ALL).title("Message"));

        App {
            username_input,
            password_input,
            confirm_password_input,
            room_name_input,
            room_id_input,
            message_input,
            current_screen: CurrentScreen::AuthChoice,
            currently_editing: CurrentlyEditing::Username,
            jwt: None,
            should_quit: false,
            joined_rooms: Vec::new(),
            room_selector_index: 0,
            username: None,
            status_message: String::from("Choose Login or Signup"),
            current_room: None,
            messages: Vec::new(),
            ws_sender: None,
        }
    }
}

// --- Main Application Logic ---

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // Setup terminal
    let mut terminal = init_terminal()?;
    let mut app = App::default();
    run_app(&mut terminal, &mut app).await?;

    // Restore terminal
    restore_terminal(&mut terminal)?;
    Ok(())
}

async fn run_app<B: Backend>(terminal: &mut Terminal<B>, app: &mut App<'_>) -> io::Result<()> {
    loop {
        terminal.draw(|f| ui(f, app))?;

        if app.should_quit {
            break;
        }

        if let Event::Key(key) = event::read()? {
            if key.kind == KeyEventKind::Press {
                match app.current_screen {
                    CurrentScreen::AuthChoice => handle_auth_choice(app, key).await?,
                    CurrentScreen::Login => handle_login_screen(app, key).await?,
                    CurrentScreen::Signup => handle_signup_screen(app, key).await?,
                    CurrentScreen::Main => handle_main_screen(app, key).await?,
                    CurrentScreen::RoomJoining => handle_room_joining(app, key).await?,
                    CurrentScreen::RoomCreation => handle_room_creation(app, key).await?,
                    CurrentScreen::RoomSelector => handle_room_selector(app, key).await?,
                }
            }
        }
    }
    Ok(())
}

async fn handle_auth_choice(
    app: &mut App<'_>,
    key: ratatui::crossterm::event::KeyEvent,
) -> io::Result<()> {
    match key.code {
        KeyCode::Char('l') | KeyCode::Char('L') => {
            app.current_screen = CurrentScreen::Login;
            app.currently_editing = CurrentlyEditing::Username;
            app.status_message = "Enter your login credentials".to_string();
        }
        KeyCode::Char('s') | KeyCode::Char('S') => {
            app.current_screen = CurrentScreen::Signup;
            app.currently_editing = CurrentlyEditing::Username;
            app.status_message = "Create a new account".to_string();
        }
        KeyCode::Esc => {
            app.should_quit = true;
        }
        _ => {}
    }
    Ok(())
}

async fn handle_login_screen(
    app: &mut App<'_>,
    key: ratatui::crossterm::event::KeyEvent,
) -> io::Result<()> {
    match key.code {
        KeyCode::Enter => {
            let username = app.username_input.lines().join("").trim().to_string();
            let password = app.password_input.lines().join("").trim().to_string();

            if !username.is_empty() && !password.is_empty() {
                app.status_message = "Connecting...".to_string();
                app.username = Some(username.clone());

                match attempt_login(&username, &password).await {
                    Ok(token) => {
                        app.jwt = Some(token);
                        app.current_screen = CurrentScreen::Main;
                        app.status_message = "Login Successful".to_string();
                    }
                    Err(e) => {
                        app.status_message = format!("Login Failed: {}", e);
                    }
                }
            } else {
                app.status_message = "Please enter your credentials".to_string();
            }
        }
        KeyCode::Tab => {
            app.currently_editing = match app.currently_editing {
                CurrentlyEditing::Username => CurrentlyEditing::Password,
                CurrentlyEditing::Password => CurrentlyEditing::Username,
                _ => CurrentlyEditing::Username,
            };
        }
        KeyCode::Esc => {
            app.current_screen = CurrentScreen::AuthChoice;
            app.status_message = "Choose Login or Signup".to_string();
            clear_inputs(app);
        }
        _ => {
            let input = Event::Key(key);
            match app.currently_editing {
                CurrentlyEditing::Username => {
                    app.username_input.input(input);
                }
                CurrentlyEditing::Password => {
                    app.password_input.input(input);
                }
                _ => {}
            }
        }
    }
    Ok(())
}

async fn handle_signup_screen(
    app: &mut App<'_>,
    key: ratatui::crossterm::event::KeyEvent,
) -> io::Result<()> {
    match key.code {
        KeyCode::Enter => {
            let username = app.username_input.lines().join("").trim().to_string();
            let password = app.password_input.lines().join("").trim().to_string();
            let confirm_password = app
                .confirm_password_input
                .lines()
                .join("")
                .trim()
                .to_string();

            if username.is_empty() || password.is_empty() || confirm_password.is_empty() {
                app.status_message = "Please fill in all fields".to_string();
                return Ok(());
            }

            if password != confirm_password {
                app.status_message = "Passwords do not match".to_string();
                return Ok(());
            }

            if password.len() < 6 {
                app.status_message = "Password must be at least 6 characters".to_string();
                return Ok(());
            }

            app.status_message = "Creating account...".to_string();

            match attempt_signup(&username, &password).await {
                Ok(_) => {
                    app.status_message = "Account created! Please login.".to_string();
                    app.current_screen = CurrentScreen::Login;
                    clear_inputs(app);
                    app.currently_editing = CurrentlyEditing::Username;
                }
                Err(e) => {
                    app.status_message = format!("Signup Failed: {}", e);
                }
            }
        }
        KeyCode::Tab => {
            app.currently_editing = match app.currently_editing {
                CurrentlyEditing::Username => CurrentlyEditing::Password,
                CurrentlyEditing::Password => CurrentlyEditing::ConfirmPassword,
                CurrentlyEditing::ConfirmPassword => CurrentlyEditing::Username,
                _ => CurrentlyEditing::Username,
            };
        }
        KeyCode::Esc => {
            app.current_screen = CurrentScreen::AuthChoice;
            app.status_message = "Choose Login or Signup".to_string();
            clear_inputs(app);
        }
        _ => {
            let input = Event::Key(key);
            match app.currently_editing {
                CurrentlyEditing::Username => {
                    app.username_input.input(input);
                }
                CurrentlyEditing::Password => {
                    app.password_input.input(input);
                }
                CurrentlyEditing::ConfirmPassword => {
                    app.confirm_password_input.input(input);
                }
                _ => {}
            }
        }
    }
    Ok(())
}

async fn handle_main_screen(
    app: &mut App<'_>,
    key: ratatui::crossterm::event::KeyEvent,
) -> io::Result<()> {
    match key.code {
        KeyCode::Char('n')
            if key
                .modifiers
                .contains(ratatui::crossterm::event::KeyModifiers::CONTROL) =>
        {
            app.current_screen = CurrentScreen::RoomCreation;
            app.currently_editing = CurrentlyEditing::RoomName;
            app.status_message = "Enter room name...".to_string();
        }

        KeyCode::Char('j')
            if key
                .modifiers
                .contains(ratatui::crossterm::event::KeyModifiers::CONTROL) =>
        {
            app.current_screen = CurrentScreen::RoomJoining;
            app.currently_editing = CurrentlyEditing::RoomId;
            app.status_message = "Enter Room Id".to_string();
        }

        KeyCode::Enter => {
            if app.current_room.is_some() {
                let message = app.message_input.lines().join("").trim().to_string();
                if !message.is_empty() && app.jwt.is_some() {
                    if let Err(e) = send_message(app, &message).await {
                        app.status_message = format!("Failed to send message: {}", e);
                    } else {
                        app.message_input = TextArea::default();
                        app.message_input
                            .set_placeholder_text("Type your message...");
                        app.message_input
                            .set_block(Block::default().borders(Borders::ALL).title("Message"));
                    }
                }
            }
        }

        KeyCode::Esc => {
            if app.current_room.is_some() {
                app.current_room = None;
                app.messages.clear();
                app.status_message = "Left room".to_string();
            } else {
                app.should_quit = true;
            }
        }

        _ => {
            if app.current_room.is_some() {
                let input = Event::Key(key);
                app.message_input.input(input);
            }
        }
    }
    Ok(())
}

async fn handle_room_creation(
    app: &mut App<'_>,
    key: ratatui::crossterm::event::KeyEvent,
) -> io::Result<()> {
    match key.code {
        KeyCode::Enter => {
            let room_name = app.room_name_input.lines().join("").trim().to_string();
            if !room_name.is_empty() && app.jwt.is_some() {
                app.status_message = "Creating room...".to_string();
                if let Err(e) = create_room(app, &room_name).await {
                    app.status_message = format!("Failed to create room: {}", e);
                } else {
                    app.current_screen = CurrentScreen::Main;
                    app.room_name_input = TextArea::default();
                    app.room_name_input
                        .set_placeholder_text("Enter room name...");
                    app.room_name_input
                        .set_block(Block::default().borders(Borders::ALL).title("Room Name"));
                }
            }
        }
        KeyCode::Esc => {
            app.current_screen = CurrentScreen::Main;
            app.status_message = "Cancelled room creation".to_string();
            app.room_name_input = TextArea::default();
            app.room_name_input
                .set_placeholder_text("Enter room name...");
            app.room_name_input
                .set_block(Block::default().borders(Borders::ALL).title("Room Name"));
        }
        _ => {
            let input = Event::Key(key);
            app.room_name_input.input(input);
        }
    }
    Ok(())
}

async fn handle_room_joining(
    app: &mut App<'_>,
    key: ratatui::crossterm::event::KeyEvent,
) -> io::Result<()> {
    match key.code {
        KeyCode::Enter => {
            let room_id = app.room_id_input.lines().join("").trim().to_string();
            if !room_id.is_empty() && app.jwt.is_some() {
                app.status_message = "Joining room...".to_string();
                if let Err(e) = join_room(app, &room_id).await {
                    app.status_message = format!("Failed to join room: {}", e);
                } else {
                    app.current_screen = CurrentScreen::Main;
                    app.room_id_input = TextArea::default();
                    app.room_id_input.set_placeholder_text("Enter room ID...");
                    app.room_id_input
                        .set_block(Block::default().borders(Borders::ALL).title("Room ID"));
                }
            }
        }
        KeyCode::Esc => {
            app.current_screen = CurrentScreen::Main;
            app.status_message = "Cancelled room joining".to_string();
            app.room_id_input = TextArea::default();
            app.room_id_input.set_placeholder_text("Enter room ID...");
            app.room_id_input
                .set_block(Block::default().borders(Borders::ALL).title("Room ID"));
        }
        _ => {
            let input = Event::Key(key);
            app.room_id_input.input(input);
        }
    }
    Ok(())
}

// --- Helper Functions ---

fn clear_inputs(app: &mut App<'_>) {
    app.username_input = TextArea::default();
    app.username_input.set_placeholder_text("Enter username...");
    app.username_input
        .set_block(Block::default().borders(Borders::ALL).title("Username"));

    app.password_input = TextArea::default();
    app.password_input.set_placeholder_text("Enter password...");
    app.password_input
        .set_block(Block::default().borders(Borders::ALL).title("Password"));
    app.password_input
        .set_style(Style::default().add_modifier(Modifier::HIDDEN));

    app.confirm_password_input = TextArea::default();
    app.confirm_password_input
        .set_placeholder_text("Confirm password...");
    app.confirm_password_input.set_block(
        Block::default()
            .borders(Borders::ALL)
            .title("Confirm Password"),
    );
    app.confirm_password_input
        .set_style(Style::default().add_modifier(Modifier::HIDDEN));
}

// --- WebSocket Handling ---

async fn attempt_login(username: &str, password: &str) -> Result<String, Box<dyn Error>> {
    let (ws_stream, _) = connect_async("ws://tunnel.sreus.tech:8080").await?;
    let (mut write, mut read) = ws_stream.split();

    let login_payload = LoginPayload { username, password };
    let login_message = ClientMessage {
        typ: "login",
        payload: login_payload,
    };

    let message_json = serde_json::to_string(&login_message)?;
    write.send(Message::text(message_json)).await?;

    while let Some(message) = read.next().await {
        let message = message?;
        if let Message::Text(text) = message {
            let server_response: ServerMessage = serde_json::from_str(&text)?;

            match server_response {
                ServerMessage::LoggedIn(payload) => return Ok(payload.token),
                ServerMessage::Error(payload) => {
                    return Err(payload.message.into());
                }
                _ => {}
            }
        }
    }
    Err("Connection closed unexpectedly".into())
}

async fn attempt_signup(username: &str, password: &str) -> Result<(), Box<dyn Error>> {
    let (ws_stream, _) = connect_async("ws://tunnel.sreus.tech:8080").await?;
    let (mut write, mut read) = ws_stream.split();

    let signup_payload = RegisterPayload { username, password };
    let signup_message = ClientMessage {
        typ: "register",
        payload: signup_payload,
    };

    let message_json = serde_json::to_string(&signup_message)?;
    write.send(Message::text(message_json)).await?;

    while let Some(message) = read.next().await {
        let message = message?;
        if let Message::Text(text) = message {
            let server_response: ServerMessage = serde_json::from_str(&text)?;

            match server_response {
                ServerMessage::Registered(_) => return Ok(()),
                ServerMessage::Error(payload) => {
                    return Err(payload.message.into());
                }
                _ => {}
            }
        }
    }
    Err("Connection closed unexpectedly".into())
}

async fn create_room(app: &mut App<'_>, room_name: &str) -> Result<(), Box<dyn Error>> {
    let (ws_stream, _) = connect_async("ws://tunnel.sreus.tech:8080").await?;
    let (mut write, mut read) = ws_stream.split();

    if let Some(token) = &app.jwt {
        let create_payload = CreateRoomPayload {
            token,
            name: room_name,
        };
        let create_message = ClientMessage {
            typ: "createRoom",
            payload: create_payload,
        };

        let message_json = serde_json::to_string(&create_message)?;
        write.send(Message::text(message_json)).await?;

        while let Some(message) = read.next().await {
            let message = message?;
            if let Message::Text(text) = message {
                let server_response: ServerMessage = serde_json::from_str(&text)?;
                match server_response {
                    ServerMessage::RoomCreated(payload) => {
                        app.current_room = Some(RoomInfo {
                            id: payload.room_id,
                            name: payload.name,
                        });
                        app.status_message = "Room created successfully!".to_string();
                        return Ok(());
                    }
                    ServerMessage::Error(payload) => return Err(payload.message.into()),
                    _ => {}
                }
            }
        }
    }
    Err("No valid token".into())
}

async fn join_room(app: &mut App<'_>, room_id: &str) -> Result<(), Box<dyn Error>> {
    let (ws_stream, _) = connect_async("ws://tunnel.sreus.tech:8080").await?;
    let (mut write, mut read) = ws_stream.split();

    if let Some(token) = &app.jwt {
        let join_payload = JoinRoomPayload { token, room_id };
        let join_message = ClientMessage {
            typ: "joinRoom",
            payload: join_payload,
        };

        let message_json = serde_json::to_string(&join_message)?;
        write.send(Message::text(message_json)).await?;

        while let Some(message) = read.next().await {
            let message = message?;
            if let Message::Text(text) = message {
                let server_response: ServerMessage = serde_json::from_str(&text)?;
                match server_response {
                    ServerMessage::JoinedRoom(payload) => {
                        app.current_room = Some(RoomInfo {
                            id: payload.room_id,
                            name: payload.name,
                        });
                        app.status_message = "Joined room successfully!".to_string();
                        return Ok(());
                    }
                    ServerMessage::JoinRequestSent(payload) => {
                        app.status_message = payload.message;
                        return Ok(());
                    }
                    ServerMessage::Error(payload) => return Err(payload.message.into()),
                    _ => {}
                }
            }
        }
    }
    Err("No valid token".into())
}

async fn send_message(app: &mut App<'_>, content: &str) -> Result<(), Box<dyn Error>> {
    let (ws_stream, _) = connect_async("ws://tunnel.sreus.tech:8080").await?;
    let (mut write, _read) = ws_stream.split();

    if let (Some(token), Some(room)) = (&app.jwt, &app.current_room) {
        let message_payload = MessagePayload {
            token,
            room_id: &room.id,
            content,
        };
        let message = ClientMessage {
            typ: "message",
            payload: message_payload,
        };

        let message_json = serde_json::to_string(&message)?;
        write.send(Message::text(message_json)).await?;
    }
    Ok(())
}

// --- UI Rendering ---

fn ui(f: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Title
            Constraint::Min(1),    // Main content
            Constraint::Length(3), // Status/Footer
        ])
        .split(f.area());

    let title = Paragraph::new("radiochat")
        .style(Style::default().fg(Color::LightCyan))
        .alignment(Alignment::Center)
        .block(Block::default().borders(Borders::ALL));
    f.render_widget(title, chunks[0]);

    match app.current_screen {
        CurrentScreen::AuthChoice => render_auth_choice(f, chunks[1]),
        CurrentScreen::Login => render_login(f, app, chunks[1]),
        CurrentScreen::Signup => render_signup(f, app, chunks[1]),
        CurrentScreen::Main => render_main(f, app, chunks[1]),
        CurrentScreen::RoomCreation => render_room_creation(f, app, chunks[1]),
        CurrentScreen::RoomJoining => render_room_joining(f, app, chunks[1]),
    }

    render_footer(f, app, chunks[2]);
}

fn render_auth_choice(f: &mut Frame, area: Rect) {
    let auth_text = Text::from(vec![
        Line::from("Welcome to RadioChat! "),
        Line::from(""),
        Line::from("Tune your radio frequencies to enter this new world of comms"),
        Line::from(""),
        Line::from("Press:"),
        Line::from("  L - Login to existing account"),
        Line::from("  S - Sign up for new account"),
        Line::from("  Esc - Quit"),
    ]);

    let auth_widget = Paragraph::new(auth_text)
        .style(Style::default().fg(Color::White))
        .alignment(Alignment::Center)
        .block(Block::default().borders(Borders::ALL).title("Tune In"));
    f.render_widget(auth_widget, area);
}

fn render_login(f: &mut Frame, app: &mut App, area: Rect) {
    let login_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Length(3)])
        .margin(1)
        .split(area);

    update_field_styles(app, false);
    f.render_widget(&app.username_input, login_chunks[0]);
    f.render_widget(&app.password_input, login_chunks[1]);
}

fn render_signup(f: &mut Frame, app: &mut App, area: Rect) {
    let signup_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(3),
        ])
        .margin(1)
        .split(area);

    update_field_styles(app, true);
    f.render_widget(&app.username_input, signup_chunks[0]);
    f.render_widget(&app.password_input, signup_chunks[1]);
    f.render_widget(&app.confirm_password_input, signup_chunks[2]);
}

fn render_main(f: &mut Frame, app: &mut App, area: Rect) {
    match &app.current_room {
        Some(room) => {
            // Chat interface
            let chat_chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Min(1),    // Messages
                    Constraint::Length(3), // Input
                ])
                .split(area);

            // Messages area
            let messages_text = if app.messages.is_empty() {
                vec![Line::from("No messages yet. Start the conversation!")]
            } else {
                app.messages
                    .iter()
                    .map(|msg| {
                        Line::from(format!(
                            "[{}] {}: {}",
                            msg.created_at.format("%H:%M:%S"),
                            msg.author.username,
                            msg.content
                        ))
                    })
                    .collect()
            };

            let messages_widget = Paragraph::new(Text::from(messages_text))
                .style(Style::default().fg(Color::White))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(format!("Room: {}", room.name)),
                )
                .wrap(Wrap { trim: true });
            f.render_widget(messages_widget, chat_chunks[0]);

            // Message input
            f.render_widget(&app.message_input, chat_chunks[1]);
        }
        None => {
            // Placeholder
            let placeholder_text = Text::from(vec![
                Line::from("Radiocheck finish. Welcome to the next gen comms app!"),
                Line::from(""),
                Line::from("Tune into your frequency"),
                Line::from(""),
                Line::from("📻 Press SUPER + n to create a new room"),
                Line::from("🔗 Press SUPER + j to join a room by ID"),
                Line::from(""),
                Line::from("\"Broadcasting good vibes...\""),
            ]);

            let placeholder_widget = Paragraph::new(placeholder_text)
                .style(Style::default().fg(Color::White))
                .alignment(Alignment::Center)
                .block(Block::default().borders(Borders::ALL).title("Main"));
            f.render_widget(placeholder_widget, area);
        }
    }
}

fn render_room_creation(f: &mut Frame, app: &mut App, area: Rect) {
    let popup_area = centered_rect(60, 20, area);
    f.render_widget(Clear, popup_area);

    let create_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3)])
        .margin(1)
        .split(popup_area);

    f.render_widget(&app.room_name_input, create_chunks[0]);
}

fn render_room_joining(f: &mut Frame, app: &mut App, area: Rect) {
    let popup_area = centered_rect(60, 20, area);
    f.render_widget(Clear, popup_area);

    let join_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3)])
        .margin(1)
        .split(popup_area);

    f.render_widget(&app.room_id_input, join_chunks[0]);
}

fn render_footer(f: &mut Frame, app: &mut App, area: Rect) {
    let status_color = if app.status_message.contains("Failed")
        || app.status_message.contains("Error")
    {
        Color::Red
    } else if app.status_message.contains("Successful") || app.status_message.contains("created") {
        Color::Green
    } else {
        Color::Yellow
    };

    let footer_text = match app.current_screen {
        CurrentScreen::AuthChoice => &app.status_message,
        CurrentScreen::Login => "Tab: Switch fields | Enter: Login | Esc: Back",
        CurrentScreen::Signup => "Tab: Switch fields | Enter: Sign up | Esc: Back",
        CurrentScreen::Main => {
            if app.current_room.is_some() {
                "Enter: Send message | Esc: Leave room"
            } else {
                "SUPER + n: New room | Super + j: Join room | Esc: Quit"
            }
        }
        CurrentScreen::RoomCreation => "Enter: Create room | Esc: Cancel",
        CurrentScreen::RoomJoining => "Enter: Join room | Esc: Cancel",
    };

    let footer = Paragraph::new(footer_text)
        .style(Style::default().fg(status_color))
        .alignment(Alignment::Center)
        .block(Block::default().borders(Borders::ALL));
    f.render_widget(footer, area);
}

fn update_field_styles(app: &mut App, is_signup: bool) {
    // Reset all styles first
    app.username_input
        .set_style(Style::default().fg(Color::White));
    app.password_input.set_style(
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::HIDDEN),
    );
    app.confirm_password_input.set_style(
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::HIDDEN),
    );

    // Highlight the currently editing field
    match app.currently_editing {
        CurrentlyEditing::Username => {
            app.username_input
                .set_style(Style::default().fg(Color::Yellow));
        }
        CurrentlyEditing::Password => {
            app.password_input.set_style(
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::HIDDEN),
            );
        }
        CurrentlyEditing::ConfirmPassword if is_signup => {
            app.confirm_password_input.set_style(
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::HIDDEN),
            );
        }
        _ => {}
    }
}

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
