use clap::Parser;
use logmonitor::app::{App, AppResult};
use logmonitor::command::{self, CommandLineArgs};
use logmonitor::event::{Event, EventHandler};
use logmonitor::handler::{handle_backend_events, handle_key_events};
use logmonitor::logging::LoggingInitResult;
use logmonitor::tui::Tui;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io;

extern crate log;

fn main() -> AppResult<()> {
    let command_line_args = command::CommandLineArgs::parse();
    println!("{:?}", command_line_args);
   

    let logging_init_result = logmonitor::logging::init(&command_line_args);
    if command_line_args.headless {
        return start_headless_app(command_line_args, logging_init_result);
    } else {
        return start_tui_app(command_line_args, logging_init_result);
    }
}

/// 启动无界面应用
fn start_headless_app(cmdargs: CommandLineArgs, logging_init_result: LoggingInitResult )-> AppResult<()>{
    let mut app = App::new("DragonOS Log Monitor");
    let events = EventHandler::new(250);
    let app_backend =
        logmonitor::backend::AppBackend::new(cmdargs.clone(), events.sender());
    
    while app.running {
        match events.next()? {
            Event::Tick => app.tick(),
            Event::Key(key_event) => handle_key_events(key_event, &mut app)?,
            Event::Mouse(_) => {}
            Event::Resize(_, _) => {}
            Event::Backend(e) => {
                handle_backend_events(e, &mut app)?;
            }
        }
    }
    println!("Headless mode not implemented yet.");
    Ok(())
}

/// 启动TUI应用
fn start_tui_app(cmdargs: CommandLineArgs, logging_init_result: LoggingInitResult)-> AppResult<()>{
     // Create an application.
     let mut app = App::new("DragonOS Log Monitor");
    if let Some(receiver) = logging_init_result.tui_receiver {
        app.set_backend_log_receiver(receiver);
    }
    
    // Initialize the terminal user interface.
    let backend = CrosstermBackend::new(io::stderr());
    let terminal = Terminal::new(backend)?;
    let events = EventHandler::new(250);
    let mut tui = Tui::new(terminal, events);
    tui.init()?;
    let app_backend =
        logmonitor::backend::AppBackend::new(cmdargs.clone(), tui.events.sender());

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
            Event::Backend(e) => {
                handle_backend_events(e, &mut app)?;
            }
        }
    }

    // Exit the user interface.
    tui.exit()?;
    Ok(())
}