pub mod app;
pub mod draw;
pub mod event;
pub mod focus;
pub mod forms;
pub mod input;
pub mod widgets;

use std::sync::Arc;

use color_eyre::eyre::Result;
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};
use tracing::debug;

use crate::{
    daemon::{AppContext, Snapshot},
    tui::app::UiApp,
};

pub async fn run(app: Arc<AppContext>, initial_snapshot: Option<Snapshot>) -> Result<()> {
    debug!(target: "ariatui::startup", "tui startup begin");
    enable_raw_mode()?;
    debug!(target: "ariatui::startup", "raw mode enabled");
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    debug!(target: "ariatui::startup", "alternate screen entered");
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    debug!(target: "ariatui::startup", "terminal created");

    let result = UiApp::new(app, initial_snapshot).run(&mut terminal).await;

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    result
}
