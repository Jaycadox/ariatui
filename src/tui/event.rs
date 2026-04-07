use std::time::Duration;

use crossterm::event::{self, Event, KeyEvent};

#[derive(Debug)]
pub enum UiEvent {
    Tick,
    Key(KeyEvent),
}

pub fn next_event(timeout: Duration) -> std::io::Result<UiEvent> {
    if event::poll(timeout)? {
        match event::read()? {
            Event::Key(key) => Ok(UiEvent::Key(key)),
            _ => Ok(UiEvent::Tick),
        }
    } else {
        Ok(UiEvent::Tick)
    }
}
