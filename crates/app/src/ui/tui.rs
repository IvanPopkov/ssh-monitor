//! Keys come from a blocking crossterm reader thread (SPEC F6).

use crate::app::{self, Frontend, Key, Session, View};
use crate::ui::render::render;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use std::time::Duration;
use tokio::sync::mpsc;

struct Tui {
    terminal: DefaultTerminal,
}

impl Frontend for Tui {
    fn draw(&mut self, view: &View<'_>) -> anyhow::Result<()> {
        self.terminal.draw(|frame| render(frame, view))?;
        Ok(())
    }
}

pub fn map_event(ev: &Event) -> Option<Key> {
    match ev {
        Event::Key(k) if k.kind != KeyEventKind::Release => match k.code {
            KeyCode::Char('q') | KeyCode::Esc => Some(Key::Quit),
            KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => Some(Key::Quit),
            KeyCode::Char('p') => Some(Key::Pause),
            KeyCode::Char('c') => Some(Key::Clear),
            _ => None,
        },
        Event::Resize(_, _) => Some(Key::Redraw),
        _ => None,
    }
}

/// The thread stops at the next event or poll timeout after the channel closes.
fn spawn_input_thread(tx: mpsc::Sender<Key>) {
    std::thread::spawn(move || {
        loop {
            match event::poll(Duration::from_millis(250)) {
                Ok(true) => {
                    let Ok(ev) = event::read() else { break };
                    if let Some(key) = map_event(&ev)
                        && tx.blocking_send(key).is_err()
                    {
                        break;
                    }
                }
                Ok(false) => {
                    if tx.is_closed() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
}

pub async fn run(session: Session) -> anyhow::Result<u8> {
    let terminal = ratatui::try_init()?;
    let (tx, rx) = mpsc::channel(16);
    spawn_input_thread(tx);
    let mut tui = Tui { terminal };
    let result = app::run(session, &mut tui, Some(rx), None).await;
    drop(tui);
    ratatui::restore();
    result?;
    // A clean `q` is success; the 0/1/2 state codes belong to `--once` (SPEC F7).
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyEvent;

    #[test]
    fn keys_map_to_quit_pause_clear_and_redraw() {
        let key = |c: char| Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        assert_eq!(map_event(&key('q')), Some(Key::Quit));
        assert_eq!(map_event(&key('p')), Some(Key::Pause));
        assert_eq!(map_event(&key('c')), Some(Key::Clear));
        assert_eq!(map_event(&key('x')), None);
        let ctrl_c = Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert_eq!(map_event(&ctrl_c), Some(Key::Quit));
        assert_eq!(map_event(&Event::Resize(80, 24)), Some(Key::Redraw));
    }
}
