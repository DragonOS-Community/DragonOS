use std::error;

use rand::{distributions::Uniform, prelude::Distribution, rngs::ThreadRng};
use ratatui::widgets::ListState;

/// Application result type.
pub type AppResult<T> = std::result::Result<T, Box<dyn error::Error>>;

/// Application.
#[derive(Debug)]
pub struct App<'a> {
    /// APP的标题
    pub title: &'a str,
    /// Is the application running?
    pub running: bool,

    pub enhanced_graphics: bool,

    /// counter
    pub counter: u8,

    pub tabs: TabsState<'a>,

    pub memory_log_sparkline: Signal<RandomSignal>,

    logs: Vec<String>,
    pub stateful_logs: StatefulList<(&'a str, &'a str)>,

    backend_log_receiver: Option<std::sync::mpsc::Receiver<String>>,
}

impl<'a> App<'a> {
    /// Constructs a new instance of [`App`].
    pub fn new(title: &'a str) -> Self {
        let mut rand_signal = RandomSignal::new(0, 100);
        let sparkline_points = rand_signal.by_ref().take(300).collect();
        let sparkline = Signal {
            source: rand_signal,
            points: sparkline_points,
            tick_rate: 1,
        };

        Self {
            title,
            running: true,
            enhanced_graphics: true,
            counter: 0,
            tabs: TabsState::new(vec!["Tab0", "Tab1", "Tab2"]),
            memory_log_sparkline: sparkline,
            logs: Vec::new(),
            stateful_logs: StatefulList::with_items(vec![]),
            backend_log_receiver: None,
        }
    }

    pub fn set_backend_log_receiver(&mut self, receiver: std::sync::mpsc::Receiver<String>) {
        self.backend_log_receiver = Some(receiver);
    }

    /// Handles the tick event of the terminal.
    pub fn tick(&mut self) {
        self.memory_log_sparkline.on_tick();
        self.handle_logs_on_tick();
    }

    /// 当到达tick时，处理日志
    fn handle_logs_on_tick(&mut self) {
        let logs_to_push = self
            .backend_log_receiver
            .as_ref()
            .map(|rv| rv.try_iter().collect::<Vec<String>>());

        if let Some(logs) = logs_to_push {
            for log in logs {
                self.push_log(log);
            }
        }
    }

    /// Set running to false to quit the application.
    pub fn quit(&mut self) {
        self.running = false;
    }

    pub fn increment_counter(&mut self) {
        if let Some(res) = self.counter.checked_add(1) {
            self.counter = res;
        }
    }

    pub fn decrement_counter(&mut self) {
        if let Some(res) = self.counter.checked_sub(1) {
            self.counter = res;
        }
    }

    pub fn push_log(&mut self, log: String) {
        self.logs.push(log);
    }

    pub fn logs(&self) -> &Vec<String> {
        &self.logs
    }
}

#[derive(Debug)]
pub struct TabsState<'a> {
    pub titles: Vec<&'a str>,
    pub index: usize,
}

impl<'a> TabsState<'a> {
    pub fn new(titles: Vec<&'a str>) -> TabsState {
        TabsState { titles, index: 0 }
    }
    pub fn next(&mut self) {
        self.index = (self.index + 1) % self.titles.len();
    }

    pub fn previous(&mut self) {
        if self.index > 0 {
            self.index -= 1;
        } else {
            self.index = self.titles.len() - 1;
        }
    }
}

#[derive(Clone, Debug)]
pub struct Signal<S: Iterator> {
    source: S,
    pub points: Vec<S::Item>,
    tick_rate: usize,
}

impl<S> Signal<S>
where
    S: Iterator,
{
    fn on_tick(&mut self) {
        for _ in 0..self.tick_rate {
            self.points.remove(0);
        }
        self.points
            .extend(self.source.by_ref().take(self.tick_rate));
    }
}

#[derive(Clone, Debug)]
pub struct RandomSignal {
    distribution: Uniform<u64>,
    rng: ThreadRng,
}

impl RandomSignal {
    pub fn new(lower: u64, upper: u64) -> RandomSignal {
        RandomSignal {
            distribution: Uniform::new(lower, upper),
            rng: rand::thread_rng(),
        }
    }
}

impl Iterator for RandomSignal {
    type Item = u64;
    fn next(&mut self) -> Option<u64> {
        Some(self.distribution.sample(&mut self.rng))
    }
}

#[derive(Debug)]
pub struct StatefulList<T> {
    pub state: ListState,
    pub items: Vec<T>,
}

impl<T> StatefulList<T> {
    pub fn with_items(items: Vec<T>) -> StatefulList<T> {
        StatefulList {
            state: ListState::default(),
            items,
        }
    }

    pub fn next(&mut self) {
        let i = match self.state.selected() {
            Some(i) => {
                if i >= self.items.len() - 1 {
                    0
                } else {
                    i + 1
                }
            }
            None => 0,
        };
        self.state.select(Some(i));
    }

    pub fn previous(&mut self) {
        let i = match self.state.selected() {
            Some(i) => {
                if i == 0 {
                    self.items.len() - 1
                } else {
                    i - 1
                }
            }
            None => 0,
        };
        self.state.select(Some(i));
    }
}
