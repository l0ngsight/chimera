//! A single-line text input with a cursor (pasted newlines are kept as spaces).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Default, Clone)]
pub struct LineInput {
    chars: Vec<char>,
    cursor: usize,
}

impl LineInput {
    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    pub fn clear(&mut self) {
        self.chars.clear();
        self.cursor = 0;
    }

    pub fn insert_str(&mut self, s: &str) {
        for ch in s.chars() {
            self.insert(if ch == '\n' || ch == '\r' || ch == '\t' { ' ' } else { ch });
        }
    }

    fn insert(&mut self, ch: char) {
        self.chars.insert(self.cursor, ch);
        self.cursor += 1;
    }

    pub fn handle_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('a') if ctrl => self.cursor = 0,
            KeyCode::Char('e') if ctrl => self.cursor = self.chars.len(),
            KeyCode::Char('u') if ctrl => {
                self.chars.drain(..self.cursor);
                self.cursor = 0;
            }
            KeyCode::Char('w') if ctrl => {
                // Delete the word before the cursor.
                let mut start = self.cursor;
                while start > 0 && self.chars[start - 1] == ' ' {
                    start -= 1;
                }
                while start > 0 && self.chars[start - 1] != ' ' {
                    start -= 1;
                }
                self.chars.drain(start..self.cursor);
                self.cursor = start;
            }
            KeyCode::Char(ch) if !ctrl => self.insert(ch),
            KeyCode::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                self.chars.remove(self.cursor);
            }
            KeyCode::Delete if self.cursor < self.chars.len() => {
                self.chars.remove(self.cursor);
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.chars.len()),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.chars.len(),
            _ => {}
        }
    }

    /// The visible slice for a field `width` characters wide, keeping the cursor in view, and the
    /// cursor's column within it.
    pub fn view(&self, width: usize) -> (String, usize) {
        let width = width.max(1);
        let start = self.cursor.saturating_sub(width - 1);
        let visible: String = self.chars[start..].iter().take(width).collect();
        (visible, self.cursor - start)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::from(code)
    }

    #[test]
    fn edits_and_scrolls() {
        let mut i = LineInput::default();
        i.insert_str("hello world");
        i.handle_key(key(KeyCode::Left));
        i.handle_key(key(KeyCode::Backspace));
        assert_eq!(i.text(), "hello word");
        i.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(i.text(), "hello d");
        i.insert_str("line1\nline2");
        assert!(!i.text().contains('\n'));
        let (visible, col) = i.view(5);
        assert_eq!(visible.chars().count(), 5);
        assert!(col <= 4);
    }
}
