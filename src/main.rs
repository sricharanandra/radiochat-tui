mod api;

use api::{ClientMessage, LoginPayload, ServerMessage};
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
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tui_textarea::TextArea;

// --- Application State ---

enum CurrentScreen {
    Login,
    // Main, // To be added later
}

enum CurrentlyEditing {
    Username,
    Password,
}

struct App<'a> {
    username_input: TextArea<'a>,
    password_input: TextArea<'a>,
    current_screen: CurrentScreen,
    currently_editing: CurrentlyEditing,
    jwt: Option<String>,
}

impl<'a> Default for App<'a> {
    fn default() -> Self {
        let mut username_input = TextArea::default();
        username_input.set_placeholder_text("Enter username...");
        username_input.set_block(Block::default().borders(Borders::ALL).title("Username"));

        let mut password_input = TextArea::default();
        password_input.set_placeholder_text("Enter password...");
        password_input.set_block(Block::default().borders(Borders::ALL).title("Password"));
        // Obscure password text
        password_input.set_style(Style::default().add_modifier(Modifier::HIDDEN));

        App {
            username_input,
            password_input,
            current_screen: CurrentScreen::Login,
            currently_editing: CurrentlyEditing::Username,
            jwt: None,
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

        if let Event::Key(key) = event::read()? {
            if key.kind == KeyEventKind::Press {
                match app.current_screen {
                    CurrentScreen::Login => match key.code {
                        KeyCode::Enter => {
                            // Here you would trigger the login/register logic
                            // For now, it does nothing.
                        }
                        KeyCode::Tab => {
                            app.currently_editing = match app.currently_editing {
                                CurrentlyEditing::Username => CurrentlyEditing::Password,
                                CurrentlyEditing::Password => CurrentlyEditing::Username,
                            };
                        }
                        KeyCode::Esc => {
                            return Ok(());
                        }
                        _ => {
                            // Pass the input to the currently focused textarea
                            let input = Event::Key(key);
                            match app.currently_editing {
                                CurrentlyEditing::Username => {
                                    app.username_input.input(input);
                                }
                                CurrentlyEditing::Password => {
                                    app.password_input.input(input);
                                }
                            }
                        }
                    },
                }
            }
        }
    }
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

    let title = Paragraph::new("RadioChat TUI")
        .style(Style::default().fg(Color::LightCyan))
        .alignment(Alignment::Center)
        .block(Block::default().borders(Borders::ALL));
    f.render_widget(title, chunks[0]);

    // Render different content based on the current screen
    match app.current_screen {
        CurrentScreen::Login => {
            let login_chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(3), Constraint::Length(3)])
                .margin(1)
                .split(chunks[1]);

            // Highlight the currently editing box
            match app.currently_editing {
                CurrentlyEditing::Username => {
                    app.username_input
                        .set_style(Style::default().fg(Color::Yellow));
                    app.password_input.set_style(
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::HIDDEN),
                    );
                }
                CurrentlyEditing::Password => {
                    app.username_input
                        .set_style(Style::default().fg(Color::White));
                    app.password_input.set_style(
                        Style::default()
                            .fg(Color::Yellow)
                            .add_modifier(Modifier::HIDDEN),
                    );
                }
            }

            f.render_widget(&app.username_input, login_chunks[0]);
            f.render_widget(&app.password_input, login_chunks[1]);
        }
    }

    let footer_text = "Tab: Switch fields | Enter: Submit | Esc: Quit";
    let footer = Paragraph::new(footer_text)
        .style(Style::default().fg(Color::Gray))
        .alignment(Alignment::Center)
        .block(Block::default().borders(Borders::ALL));
    f.render_widget(footer, chunks[2]);
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
