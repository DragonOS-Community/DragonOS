use clap::Parser;
use logmonitor::app::{App, AppResult};
use logmonitor::command;
use logmonitor::event::{Event, EventHandler};
use logmonitor::handler::handle_key_events;
use logmonitor::tui::Tui;
use std::io;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

fn main() -> AppResult<()> {
    let command_line_args = command::CommandLineArgs::parse();
    println!("{:?}", command_line_args);
    // Create an application.
    let mut app = App::new("DragonOS Log Monitor");

    // Initialize the terminal user interface.
    let backend = CrosstermBackend::new(io::stderr());
    let terminal = Terminal::new(backend)?;
    let events = EventHandler::new(250);
    let mut tui = Tui::new(terminal, events);
    tui.init()?;

    // Start the main loop.
    while app.running {
        // Render the user interface.
        tui.draw(&mut app)?;
        // Handle events.
        match tui.events.next()? {
            Event::Tick => app.tick(),
            Event::Key(key_event) => handle_key_events(key_event, &mut app)?,
            Event::Mouse(_) => {}
            Event::Resize(_, _) => {}
        }
    }

    // Exit the user interface.
    tui.exit()?;
    Ok(())
}
