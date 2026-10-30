//! Single-line text fields.

use feos_gfx::Rect;

use crate::geometry::RectExt;
use crate::input::{Event, Key, Modifiers};
use crate::response::{Response, Sense};
use crate::text::TextBuffer;
use crate::ui::Ui;

impl<'a, 'b, 's, const N: usize> Ui<'a, 'b, 's, N> {
    /// A single-line text field.
    pub fn text_input<const M: usize>(&mut self, buffer: &mut TextBuffer<M>) -> Response {
        self.text_input_with_placeholder(buffer, "")
    }

    /// A single-line text field with muted placeholder text.
    pub fn text_input_with_placeholder<const M: usize>(
        &mut self,
        buffer: &mut TextBuffer<M>,
        placeholder: &str,
    ) -> Response {
        let metrics = self.theme.metrics;
        let palette = self.theme.palette;
        let rect = self.allocate(220, metrics.control_height);
        let id = self.next_id();
        let mut response = self.interact(id, rect, Sense::focus());
        response.changed = false;

        if response.focused {
            let ctrl = self.ctrl();
            let mut content_changed = false;
            let mut submitted = false;
            for event in self.input.events() {
                match event {
                    Event::Text(ch) => {
                        if ctrl {
                            if ch == 'a' || ch == 'A' {
                                buffer.select_all();
                            }
                        } else if !ch.is_control() && buffer.insert(ch) {
                            content_changed = true;
                        }
                    }
                    Event::KeyDown(Key::Character(ch), modifiers)
                        if modifiers.contains(Modifiers::CTRL) =>
                    {
                        if ch == 'a' || ch == 'A' {
                            buffer.select_all();
                        }
                    }
                    Event::KeyDown(Key::Backspace, _) => {
                        content_changed |= buffer.backspace();
                    }
                    Event::KeyDown(Key::Delete, _) => {
                        content_changed |= buffer.delete();
                    }
                    Event::KeyDown(Key::Left, modifiers) => {
                        buffer.move_left(modifiers.contains(Modifiers::SHIFT));
                    }
                    Event::KeyDown(Key::Right, modifiers) => {
                        buffer.move_right(modifiers.contains(Modifiers::SHIFT));
                    }
                    Event::KeyDown(Key::Home, modifiers) => {
                        buffer.move_home(modifiers.contains(Modifiers::SHIFT));
                    }
                    Event::KeyDown(Key::End, modifiers) => {
                        buffer.move_end(modifiers.contains(Modifiers::SHIFT));
                    }
                    Event::KeyDown(Key::Enter, _) => {
                        submitted = true;
                    }
                    _ => {}
                }
            }
            response.changed = content_changed;
            if submitted {
                response.submitted = true;
                self.clear_focus();
            }
        }

        let fill = if response.focused {
            palette.surface
        } else {
            palette.surface_alt
        };
        self.painter.fill_rounded_rect(rect, metrics.radius, fill);
        self.painter.stroke_rounded_rect(
            rect,
            metrics.radius,
            if response.focused {
                2
            } else {
                metrics.border_width
            },
            if response.focused {
                palette.focus
            } else if response.hovered {
                palette.border_strong
            } else {
                palette.border
            },
        );

        let inner = Rect::new(
            rect.x + metrics.padding_x,
            rect.y + 2,
            rect.width.saturating_sub((metrics.padding_x * 2) as u32),
            rect.height.saturating_sub(4),
        );
        let scale = metrics.font_scale;
        let text_height = self.painter.line_height(scale);
        let baseline = inner.y + (inner.height as i32 - text_height) / 2;
        let text = buffer.as_str();

        let cursor = buffer.cursor().min(text.len());
        let caret_x = self.painter.text_width(&text[..cursor], scale);
        let text_width = self.painter.text_width(text, scale);
        let max_scroll = (text_width - inner.width as i32 + 4).max(0);
        let mut scroll = self.state.scroll_offset(id);
        if caret_x - scroll > inner.width as i32 - 4 {
            scroll = caret_x - inner.width as i32 + 4;
        }
        if caret_x - scroll < 0 {
            scroll = caret_x;
        }
        scroll = scroll.clamp(0, max_scroll);
        self.state.set_scroll(id, scroll, max_scroll);

        let old_clip = self.painter.push_clip(inner);
        if let Some((start, end)) = buffer.selection() {
            let x0 = self.painter.text_width(&text[..start], scale) - scroll;
            let x1 = self.painter.text_width(&text[..end], scale) - scroll;
            self.painter.fill_rect(
                Rect::new(
                    inner.x + x0,
                    baseline - 2,
                    (x1 - x0).max(0) as u32,
                    (text_height + 4) as u32,
                ),
                palette.selection,
            );
        }

        let content = if text.is_empty() { placeholder } else { text };
        let color = if text.is_empty() {
            palette.text_disabled
        } else {
            palette.text
        };
        self.painter
            .draw_text(inner.x - scroll, baseline, content, color, scale);

        if response.focused {
            let caret = inner.x + caret_x - scroll;
            if caret >= inner.x && caret <= inner.right_i32() {
                self.painter.fill_rect(
                    Rect::new(caret, baseline, 2, text_height as u32),
                    palette.focus,
                );
            }
        }
        self.painter.restore_clip(old_clip);

        response
    }
}
