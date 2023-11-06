use ratatui::{
    prelude::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    symbols,
    text::{self, Span, Text},
    widgets::{Block, Borders, List, ListItem, Sparkline, Tabs},
    Frame,
};

use crate::app::App;

/// Renders the user interface widgets.
pub fn render(app: &mut App, frame: &mut Frame) {
    // This is where you add new widgets.
    // See the following resources:
    // - https://docs.rs/ratatui/latest/ratatui/widgets/index.html
    // - https://github.com/ratatui-org/ratatui/tree/master/examples
    // frame.render_widget(
    //     Paragraph::new(format!(
    //         "This is a tui template.\n\
    //             Press `Esc`, `Ctrl-C` or `q` to stop running.\n\
    //             Press left and right to increment and decrement the counter respectively.\n\
    //             Counter: {}",
    //         app.counter
    //     ))
    //     .block(
    //         Block::default()
    //             .title("Template")
    //             .title_alignment(Alignment::Center)
    //             .borders(Borders::ALL)
    //             .border_type(BorderType::Rounded),
    //     )
    //     .style(Style::default().fg(Color::Cyan).bg(Color::Black))
    //     .alignment(Alignment::Center),
    //     frame.size(),
    // )

    let chunks = Layout::default()
        .constraints([Constraint::Length(3), Constraint::Min(0)])
        .split(frame.size());
    let titles = app
        .tabs
        .titles
        .iter()
        .map(|t| text::Line::from(Span::styled(*t, Style::default().fg(Color::Green))))
        .collect();
    let tabs = Tabs::new(titles)
        .block(Block::default().borders(Borders::ALL).title(app.title))
        .highlight_style(Style::default().fg(Color::Yellow))
        .select(app.tabs.index);
    frame.render_widget(tabs, chunks[0]);

    match app.tabs.index {
        0 => draw_first_tab(frame, app, chunks[1]),
        _ => {}
    }
}

fn draw_first_tab(f: &mut Frame, app: &mut App, area: Rect) {
    let chunks = Layout::default()
        .constraints([
            Constraint::Min(1),
            Constraint::Min(3),
            Constraint::Length(7),
        ])
        .split(area);
    draw_memory_logging_speed_gauges(f, app, chunks[0]);
    // draw_charts(f, app, chunks[1]);
    draw_footer(f, app, chunks[2]);
}

/// 绘制内存日志产生数量的图表
fn draw_memory_logging_speed_gauges(f: &mut Frame, app: &mut App, area: Rect) {
    let chunks = Layout::default()
        .constraints([Constraint::Length(3)])
        .margin(1)
        .split(area);
    let block = Block::default().borders(Borders::ALL).title("Speed:");
    f.render_widget(block, area);

    let sparkline = Sparkline::default()
        .block(Block::default().title("Memory Log Speed:"))
        .style(Style::default().fg(Color::Green))
        .data(&app.memory_log_sparkline.points)
        .bar_set(if app.enhanced_graphics {
            symbols::bar::NINE_LEVELS
        } else {
            symbols::bar::THREE_LEVELS
        });
    f.render_widget(sparkline, chunks[0]);
}

fn draw_footer(f: &mut Frame, app: &mut App, area: Rect) {
    let _block = Block::default().borders(Borders::ALL).title(Span::styled(
        "Logs",
        Style::default()
            .fg(Color::Magenta)
            .add_modifier(Modifier::BOLD),
    ));

    let info_style = Style::default().fg(Color::Blue);
    let warning_style = Style::default().fg(Color::Yellow);
    let error_style = Style::default().fg(Color::Magenta);
    let critical_style = Style::default().fg(Color::Red);

    let binding = app.logs().clone();
    let log_list = binding
        .iter()
        .map(|log_str| {
            let _style = match log_str {
                log if log.contains("INFO") => info_style,
                log if log.contains("WARNING") => warning_style,
                log if log.contains("ERROR") => error_style,
                log if log.contains("CRITICAL") => critical_style,
                _ => Style::default().fg(Color::White),
            };

            // println!("log_str: {}", log_str);

            ListItem::new(Text::from(log_str.clone()))
        })
        .collect::<Vec<ListItem>>();

    let items_num = 5;
    let list_to_show = log_list.split_at(if log_list.len() > items_num {
        log_list.len() - items_num
    } else {
        0
    });

    let logs =
        List::new(list_to_show.1).block(Block::default().borders(Borders::ALL).title("List"));
    f.render_stateful_widget(logs, area, &mut app.stateful_logs.state);
}
