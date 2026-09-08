use std::io::{self, Write};

use ratatui::backend::{Backend, ClearType, CrosstermBackend, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};

/// Do not let a terminal's Unicode width table determine where the next cell
/// lands. In particular, ambiguous-width box drawing and punctuation must not
/// shift a subsequent border. Keep Crossterm's color/modifier handling intact.
pub(super) struct PositionedBackend<W: Write>(CrosstermBackend<W>);

impl<W: Write> PositionedBackend<W> {
    pub(super) fn new(writer: W) -> Self {
        Self(CrosstermBackend::new(writer))
    }
}

impl<W: Write> Backend for PositionedBackend<W> {
    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        // Each call starts with an absolute cursor position. draw only queues
        // bytes; flushing stays with Terminal and its cursor operations.
        for cell in content {
            self.0.draw(std::iter::once(cell))?;
        }
        Ok(())
    }

    fn append_lines(&mut self, n: u16) -> io::Result<()> {
        self.0.append_lines(n)
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        self.0.hide_cursor()
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        self.0.show_cursor()
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        self.0.get_cursor_position()
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        self.0.set_cursor_position(position)
    }

    fn clear(&mut self) -> io::Result<()> {
        self.0.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        self.0.clear_region(clear_type)
    }

    fn size(&self) -> io::Result<Size> {
        self.0.size()
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        self.0.window_size()
    }

    fn flush(&mut self) -> io::Result<()> {
        Backend::flush(&mut self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::{Color, Modifier};

    // Replay the actual emitted CUP/SGR stream with a terminal that disagrees
    // about ambiguous-width glyphs. A TestBackend would miss this regression.
    fn replay(output: &[u8], ambiguous_width: u16) -> Vec<(u16, u16, char)> {
        let text = std::str::from_utf8(output).unwrap();
        let mut chars = text.chars();
        let (mut x, mut y) = (0, 0);
        let mut painted = Vec::new();
        while let Some(ch) = chars.next() {
            if ch == '\x1b' {
                assert_eq!(chars.next(), Some('['));
                let mut parameters = String::new();
                let command = loop {
                    let next = chars.next().expect("complete CSI");
                    if ('@'..='~').contains(&next) {
                        break next;
                    }
                    parameters.push(next);
                };
                match command {
                    'H' => {
                        let (row, col) = parameters.split_once(';').unwrap();
                        y = row.parse::<u16>().unwrap() - 1;
                        x = col.parse::<u16>().unwrap() - 1;
                    }
                    'm' => {} // Color/modifier commands do not advance the cursor.
                    other => panic!("unexpected CSI {other}"),
                }
            } else {
                painted.push((x, y, ch));
                x += match ch {
                    '·' | '─' | '│' | '↑' => ambiguous_width,
                    '中' | '文' => 2,
                    _ => 1,
                };
            }
        }
        painted
    }

    #[test]
    fn actual_ansi_positions_survive_terminal_width_disagreements() {
        let symbols = ["│", "·", "A", "─", "中", " ", "↑", "│"];
        let cells = symbols.map(|symbol| Cell::new(symbol));
        let positions = [0, 1, 2, 3, 4, 6, 7, 8];
        let content = || positions.iter().zip(&cells).map(|(&x, cell)| (x, 2, cell));
        let mut old = Vec::new();
        CrosstermBackend::new(&mut old).draw(content()).unwrap();
        let mut fixed = Vec::new();
        PositionedBackend::new(&mut fixed).draw(content()).unwrap();
        let expected: Vec<_> = positions
            .iter()
            .zip(symbols)
            .map(|(&x, symbol)| (x, 2, symbol.chars().next().unwrap()))
            .collect();
        assert_ne!(
            replay(&old, 2),
            expected,
            "fixture must expose cursor drift"
        );
        for width in [1, 2] {
            assert_eq!(replay(&fixed, width), expected);
        }
    }

    #[test]
    fn compatible_frame_survives_wide_glyph_overwrites_in_actual_ansi() {
        use ratatui::{
            buffer::Buffer,
            layout::Rect,
            style::Style,
            widgets::{Borders, Paragraph, Widget},
        };
        let area = Rect::new(0, 0, 32, 6);
        let mut buffer = Buffer::empty(area);
        Paragraph::new("中文 · ASCII\n↑ next row\ntext near edge ·")
            .style(Style::default().fg(Color::Cyan))
            .block(crate::tui::panel_block().borders(Borders::ALL))
            .render(area, &mut buffer);
        let mut output = Vec::new();
        PositionedBackend::new(&mut output)
            .draw(Buffer::empty(area).diff(&buffer).into_iter())
            .unwrap();
        for ambiguous_width in [1, 2] {
            // Track occupied cells, including the real-terminal rule that
            // overwriting either half of a wide glyph clears the whole glyph.
            let mut screen = vec![vec![None; 32]; 6];
            for (x, y, ch) in replay(&output, ambiguous_width) {
                let width = match ch {
                    '·' | '↑' => ambiguous_width,
                    '中' | '文' => 2,
                    _ => 1,
                };
                for col in x..(x + width).min(32) {
                    if let Some((start, old_width, _)) = screen[y as usize][col as usize] {
                        for old_col in start..start + old_width {
                            screen[y as usize][old_col as usize] = None;
                        }
                    }
                }
                for col in x..(x + width).min(32) {
                    screen[y as usize][col as usize] = Some((x, width, ch));
                }
            }
            for y in 0..6 {
                for x in 0..32 {
                    let expected = match (x, y) {
                        (0 | 31, 0 | 5) => '+',
                        (_, 0 | 5) => '-',
                        (0 | 31, _) => '|',
                        _ => continue,
                    };
                    assert_eq!(screen[y][x].map(|(_, _, ch)| ch), Some(expected));
                }
            }
        }
    }

    #[test]
    fn preserves_styles_and_batches_writes_until_frame_flush() {
        #[derive(Default)]
        struct Writer {
            bytes: Vec<u8>,
            flushes: usize,
        }
        impl Write for Writer {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.bytes.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                self.flushes += 1;
                Ok(())
            }
        }
        let mut writer = Writer::default();
        let mut cell = Cell::new(" ");
        cell.set_fg(Color::Rgb(0, 0, 0))
            .set_bg(Color::Rgb(255, 255, 255))
            .set_style(Modifier::BOLD);
        {
            let mut backend = PositionedBackend::new(&mut writer);
            backend.draw((0..100).map(|x| (x, 0, &cell))).unwrap();
        }
        assert_eq!(writer.flushes, 0);
        let mut expected = Vec::new();
        CrosstermBackend::new(&mut expected)
            .draw(std::iter::once((0, 0, &cell)))
            .unwrap();
        // Match Crossterm's exact style sequence, including when the caller
        // has disabled color with NO_COLOR. Do not mutate that global policy.
        assert!(writer.bytes.starts_with(&expected));
        assert_eq!(replay(&writer.bytes, 1).len(), 100);
        PositionedBackend::new(&mut writer).flush().unwrap();
        assert_eq!(writer.flushes, 1);
    }
}
