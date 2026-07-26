//! Rendering: paint terminal grids, manager overlays, and the name
//! prompt as escape-sequence streams into any `Write` (a client socket
//! buffer on the server, stdout in tests).

use std::io::Write;

use crossterm::{
    cursor::{Hide, MoveTo, Show},
    queue,
    style::{Attribute, Color, Print, SetAttribute, SetBackgroundColor, SetForegroundColor},
    terminal::{BeginSynchronizedUpdate, Clear, ClearType, EndSynchronizedUpdate},
};

use libghostty_vt::{
    Terminal,
    render::{CellIterator, Dirty, RenderState, RowIterator},
    screen::CellWide,
    style::{RgbColor, Underline},
};

use std::collections::HashMap;

use crate::Result;
use crate::model::{Layout, Rect, Session, SplitDir, split_rect};

/// The pane area of a client screen: everything except the bottom tab
/// bar row. Sessions are laid out and shells sized to this, so it must
/// be used for split/navigation geometry too.
pub fn content_size(size: (u16, u16)) -> (u16, u16) {
    if size.1 >= 2 {
        (size.0, size.1 - 1)
    } else {
        size
    }
}

/// Truncate to `max` display characters, ellipsized.
fn fit(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{cut}…")
}

/// Draw one frame of the viewed session: the active tab's panes at their
/// rectangles, dim divider lines between them, the bottom tab bar, and
/// the focused pane's cursor.
pub fn draw_session(
    renderer: &mut Renderer<'static>,
    session: &Session,
    out: &mut impl Write,
    size: (u16, u16),
    accent: Color,
) -> Result<()> {
    let content = content_size(size);
    let full = Rect {
        x: 0,
        y: 0,
        w: content.0,
        h: content.1,
    };
    let tab = &session.tabs[session.active_tab];
    queue!(out, BeginSynchronizedUpdate, Hide)?;

    let mut cursor = None;
    let mut focus_rect = None;
    let focused = tab.focused;
    if tab.zoomed && let Some(pane) = tab.layout.pane(focused) {
        // Fullscreen: only the focused pane, no dividers.
        cursor = renderer.draw_at(&pane.term, out, full)?;
    } else {
        tab.layout.for_each(full, &mut |pane, rect| {
            let pane_cursor = renderer.draw_at(&pane.term, out, rect)?;
            if pane.id == focused {
                cursor = pane_cursor;
                focus_rect = Some(rect);
            }
            Ok(())
        })?;
        draw_dividers(out, &tab.layout, full, focus_rect, accent)?;
    }

    if size.1 >= 2 {
        draw_tab_bar(out, session, size, accent)?;
    }

    if let Some((x, y)) = cursor {
        queue!(out, MoveTo(x, y), Show)?;
    }
    queue!(out, EndSynchronizedUpdate)?;
    out.flush()?;
    Ok(())
}

/// The bottom bar: the session name as an accent chip, then the tabs —
/// the open tab in accent, the rest dim. Segments past the right edge
/// are dropped.
fn draw_tab_bar(out: &mut impl Write, session: &Session, size: (u16, u16), accent: Color) -> Result<()> {
    queue!(out, MoveTo(0, size.1 - 1), SetAttribute(Attribute::Reset))?;
    let cols = size.0 as usize;
    let mut used = 0usize;

    // Session name as a chip: accent background, terminal-background
    // text (accent foreground + reverse adapts to any theme).
    let chip = format!(" {} ", session.name);
    let chip_width = chip.chars().count();
    if chip_width <= cols {
        queue!(
            out,
            SetForegroundColor(accent),
            SetAttribute(Attribute::Reverse),
            Print(&chip),
            SetAttribute(Attribute::Reset),
        )?;
        used += chip_width;
    }

    for (i, tab) in session.tabs.iter().enumerate() {
        // A fullscreened tab advertises it in its label.
        let label = if tab.zoomed {
            format!(" {} [F] ", tab.name)
        } else {
            format!(" {} ", tab.name)
        };
        let width = label.chars().count();
        if used + width > cols {
            break;
        }
        if i == session.active_tab {
            // Accent background chip: accent foreground + reverse gives
            // accent-colored background with terminal-background text.
            queue!(
                out,
                SetForegroundColor(accent),
                SetAttribute(Attribute::Reverse),
            )?;
        } else {
            queue!(out, SetAttribute(Attribute::Dim))?;
        }
        queue!(out, Print(&label), SetAttribute(Attribute::Reset))?;
        used += width;
    }
    queue!(out, Clear(ClearType::UntilNewLine))?;
    Ok(())
}

// Line-component bits for box-drawing junction resolution.
pub(crate) const B_UP: u8 = 1;
pub(crate) const B_DOWN: u8 = 2;
pub(crate) const B_LEFT: u8 = 4;
pub(crate) const B_RIGHT: u8 = 8;

/// Draw the divider lines of every split, resolving crossings and tees
/// (`┬ ┴ ├ ┤ ┼`) where dividers meet instead of overdrawing. Divider
/// cells that border the focused pane are drawn in the accent color so
/// the active terminal reads as framed.
fn draw_dividers(
    out: &mut impl Write,
    layout: &Layout,
    rect: Rect,
    focused: Option<Rect>,
    accent: Color,
) -> Result<()> {
    // (bits, real): `real` cells are on a divider line; hint-only cells
    // exist so a neighboring divider knows a line abuts it.
    let mut cells: HashMap<(u16, u16), (u8, bool)> = HashMap::new();
    collect_dividers(layout, rect, &mut cells);
    if cells.is_empty() {
        return Ok(());
    }

    // Dim pass for dividers away from the focused pane...
    queue!(out, SetAttribute(Attribute::Reset), SetAttribute(Attribute::Dim))?;
    for (&(x, y), &(bits, real)) in &cells {
        if !real || focused.is_some_and(|f| touches(f, x, y)) {
            continue;
        }
        queue!(out, MoveTo(x, y), Print(box_char(bits)))?;
    }
    // ...then an accent pass for the ones framing it.
    queue!(
        out,
        SetAttribute(Attribute::Reset),
        SetForegroundColor(accent),
    )?;
    for (&(x, y), &(bits, real)) in &cells {
        if !real || !focused.is_some_and(|f| touches(f, x, y)) {
            continue;
        }
        queue!(out, MoveTo(x, y), Print(box_char(bits)))?;
    }
    queue!(out, SetAttribute(Attribute::Reset), SetForegroundColor(Color::Reset))?;
    Ok(())
}

/// Whether a divider cell lies on the one-cell ring around `f` — the
/// dividers that visually frame that pane (corners included).
fn touches(f: Rect, x: u16, y: u16) -> bool {
    let x_in = x + 1 >= f.x && x <= f.x + f.w;
    let y_in = y + 1 >= f.y && y <= f.y + f.h;
    let on_vertical = (x + 1 == f.x || x == f.x + f.w) && y_in;
    let on_horizontal = (y + 1 == f.y || y == f.y + f.h) && x_in;
    on_vertical || on_horizontal
}

pub(crate) fn collect_dividers(layout: &Layout, rect: Rect, cells: &mut HashMap<(u16, u16), (u8, bool)>) {
    let Layout::Split { dir, a, b } = layout else {
        return;
    };
    let (ra, rb) = split_rect(*dir, rect);
    match dir {
        SplitDir::Horizontal => {
            let y = rect.y + ra.h;
            for x in rect.x..rect.x + rect.w {
                let cell = cells.entry((x, y)).or_insert((0, false));
                cell.0 |= B_LEFT | B_RIGHT;
                cell.1 = true;
            }
            // Tell abutting vertical dividers a line arrives from the side.
            if rect.x > 0 {
                cells.entry((rect.x - 1, y)).or_insert((0, false)).0 |= B_RIGHT;
            }
            cells.entry((rect.x + rect.w, y)).or_insert((0, false)).0 |= B_LEFT;
        }
        SplitDir::Vertical => {
            let x = rect.x + ra.w;
            for y in rect.y..rect.y + rect.h {
                let cell = cells.entry((x, y)).or_insert((0, false));
                cell.0 |= B_UP | B_DOWN;
                cell.1 = true;
            }
            if rect.y > 0 {
                cells.entry((x, rect.y - 1)).or_insert((0, false)).0 |= B_DOWN;
            }
            cells.entry((x, rect.y + rect.h)).or_insert((0, false)).0 |= B_UP;
        }
    }
    collect_dividers(a, ra, cells);
    collect_dividers(b, rb, cells);
}

pub(crate) fn box_char(bits: u8) -> char {
    let (u, d, l, r) = (
        bits & B_UP != 0,
        bits & B_DOWN != 0,
        bits & B_LEFT != 0,
        bits & B_RIGHT != 0,
    );
    match (u, d, l, r) {
        (true, true, true, true) => '┼',
        (true, true, true, false) => '┤',
        (true, true, false, true) => '├',
        (true, false, true, true) => '┴',
        (false, true, true, true) => '┬',
        (true, true, _, _) => '│',
        _ => '─',
    }
}

/// One row of a manager panel.
pub struct ListItem {
    pub label: String,
    /// The currently open session/tab — marked with an accent dot.
    pub active: bool,
    /// Rendered dim (e.g. a pinned session that isn't running).
    pub dim: bool,
}

/// A centered, rounded panel geometry for the overlays.
struct Panel {
    x: u16,
    y: u16,
    /// Interior width (inside the borders, minus 1-cell side padding).
    iw: usize,
}

/// Draw the panel frame (rounded corners, dim border, bold inline title)
/// and `body_rows` blank interior rows, returning the geometry.
fn draw_panel(
    out: &mut impl Write,
    title: &str,
    body_rows: u16,
    min_interior: usize,
    size: (u16, u16),
) -> Result<Panel> {
    let need = (min_interior.max(title.chars().count() + 2) + 4) as u16;
    let w = need.clamp(24, size.0.saturating_sub(2).max(10));
    let h = body_rows + 2;
    let x = size.0.saturating_sub(w) / 2;
    let y = size.1.saturating_sub(h) / 2;
    let iw = w.saturating_sub(4) as usize;

    // Top border with the title inline: ╭─ title ────╮
    let title = fit(title, iw);
    let dash_count = (w as usize).saturating_sub(title.chars().count() + 5);
    queue!(
        out,
        MoveTo(x, y),
        SetAttribute(Attribute::Reset),
        SetAttribute(Attribute::Dim),
        Print("╭─ "),
        SetAttribute(Attribute::Reset),
        SetAttribute(Attribute::Bold),
        Print(&title),
        SetAttribute(Attribute::Reset),
        SetAttribute(Attribute::Dim),
        Print(format!(" {}╮", "─".repeat(dash_count))),
    )?;
    for row in 1..=body_rows {
        queue!(
            out,
            MoveTo(x, y + row),
            Print(format!("│{}│", " ".repeat(w as usize - 2))),
        )?;
    }
    queue!(
        out,
        MoveTo(x, y + body_rows + 1),
        Print(format!("╰{}╯", "─".repeat(w as usize - 2))),
        SetAttribute(Attribute::Reset),
    )?;
    Ok(Panel { x, y, iw })
}

/// Draw a manager overlay: a centered panel listing sessions or tabs,
/// with a `❯` selector, an accent dot on the open entry, and stopped
/// entries dimmed.
pub fn draw_manager(
    out: &mut impl Write,
    title: &str,
    items: &[ListItem],
    selected: usize,
    size: (u16, u16),
    accent: Color,
    footer: &str,
) -> Result<()> {
    queue!(
        out,
        BeginSynchronizedUpdate,
        Hide,
        SetAttribute(Attribute::Reset),
        Clear(ClearType::All),
    )?;
    // Window the list if the screen is short.
    let max_shown = (size.1.saturating_sub(6) as usize).max(1);
    let offset = (selected + 1).saturating_sub(max_shown);
    let shown = &items[offset.min(items.len())..(offset + max_shown).min(items.len())];

    let min_interior = items
        .iter()
        .map(|i| i.label.chars().count() + 4)
        .chain([footer.chars().count()])
        .max()
        .unwrap_or(0);
    let body_rows = shown.len() as u16 + 3;
    let panel = draw_panel(out, title, body_rows, min_interior, size)?;

    for (row, item) in shown.iter().enumerate() {
        let is_selected = offset + row == selected;
        queue!(out, MoveTo(panel.x + 2, panel.y + 2 + row as u16))?;
        if is_selected {
            queue!(
                out,
                SetForegroundColor(accent),
                Print("❯ "),
                SetAttribute(Attribute::Bold),
            )?;
        } else {
            queue!(out, Print("  "))?;
        }
        if item.active {
            queue!(out, SetForegroundColor(accent), Print("● "))?;
            if !is_selected {
                queue!(out, SetForegroundColor(Color::Reset))?;
            }
        } else {
            queue!(out, Print("  "))?;
        }
        if item.dim && !is_selected {
            queue!(out, SetAttribute(Attribute::Dim))?;
        }
        queue!(
            out,
            Print(fit(&item.label, panel.iw.saturating_sub(4))),
            SetAttribute(Attribute::Reset),
            SetForegroundColor(Color::Reset),
        )?;
    }

    queue!(
        out,
        MoveTo(panel.x + 2, panel.y + body_rows),
        SetAttribute(Attribute::Dim),
        Print(fit(footer, panel.iw)),
        SetAttribute(Attribute::Reset),
        EndSynchronizedUpdate,
    )?;
    out.flush()?;
    Ok(())
}

/// Draw the name prompt for a new session/tab as a centered panel.
pub fn draw_naming(
    out: &mut impl Write,
    title: &str,
    name: &str,
    size: (u16, u16),
    accent: Color,
    footer: &str,
) -> Result<()> {
    queue!(
        out,
        BeginSynchronizedUpdate,
        Hide,
        SetAttribute(Attribute::Reset),
        Clear(ClearType::All),
    )?;
    let min_interior = (name.chars().count() + 6).max(footer.chars().count());
    let panel = draw_panel(out, title, 4, min_interior, size)?;

    queue!(
        out,
        MoveTo(panel.x + 2, panel.y + 2),
        SetForegroundColor(accent),
        Print("❯ "),
        SetForegroundColor(Color::Reset),
        Print(fit(name, panel.iw.saturating_sub(2))),
        MoveTo(panel.x + 2, panel.y + 4),
        SetAttribute(Attribute::Dim),
        Print(fit(footer, panel.iw)),
        SetAttribute(Attribute::Reset),
        // Park the visible cursor at the end of the typed name.
        MoveTo(panel.x + 4 + name.chars().count().min(panel.iw) as u16, panel.y + 2),
        Show,
        EndSynchronizedUpdate,
    )?;
    out.flush()?;
    Ok(())
}

pub struct Renderer<'alloc> {
    render_state: RenderState<'alloc>,
    row_it: RowIterator<'alloc>,
    cell_it: CellIterator<'alloc>,
}

/// The SGR state we last emitted, so we only send color/attribute
/// sequences when a cell actually differs from the previous one.
#[derive(PartialEq, Clone, Copy)]
struct Pen {
    fg: Color,
    bg: Color,
    bold: bool,
    italic: bool,
    underline: bool,
}

impl<'alloc> Renderer<'alloc> {
    pub fn new() -> Result<Self> {
        Ok(Self {
            render_state: RenderState::new()?,
            row_it: RowIterator::new()?,
            cell_it: CellIterator::new()?,
        })
    }

    /// Draw one terminal's grid with its top-left at `rect`'s origin
    /// (the terminal is kept sized to the rect by `Tab::apply_layout`).
    /// Returns the coordinates of the terminal's cursor if visible.
    /// The caller wraps the frame in a synchronized update and flushes.
    fn draw_at(
        &mut self,
        term: &Terminal<'alloc, '_>,
        out: &mut impl Write,
        rect: Rect,
    ) -> Result<Option<(u16, u16)>> {
        // Snapshot the terminal state; everything below reads the snapshot.
        let snapshot = self.render_state.update(term)?;
        let colors = snapshot.colors()?;

        let default = Pen {
            fg: color(colors.foreground),
            bg: color(colors.background),
            bold: false,
            italic: false,
            underline: false,
        };
        let mut pen = default;

        queue!(
            out,
            SetAttribute(Attribute::Reset),
            SetForegroundColor(pen.fg),
            SetBackgroundColor(pen.bg),
        )?;

        let mut row_it = self.row_it.update(&snapshot)?;
        let mut y: u16 = 0;
        let mut text = String::with_capacity(16);

        while let Some(row) = row_it.next() {
            queue!(out, MoveTo(rect.x, rect.y + y))?;
            let mut cell_it = self.cell_it.update(row)?;

            while let Some(cell) = cell_it.next() {
                // A wide character already advanced the cursor two
                // columns; printing anything for its spacer cell would
                // clobber the glyph's right half.
                match cell.raw_cell()?.wide()? {
                    CellWide::SpacerTail | CellWide::SpacerHead => continue,
                    CellWide::Narrow | CellWide::Wide => {}
                }

                let mut next = Pen {
                    fg: cell.fg_color()?.map_or(default.fg, color),
                    bg: cell.bg_color()?.map_or(default.bg, color),
                    ..default
                };

                if cell.has_styling()? {
                    let style = cell.style()?;
                    next.bold = style.bold;
                    next.italic = style.italic;
                    next.underline = style.underline != Underline::None;
                    if style.inverse {
                        std::mem::swap(&mut next.fg, &mut next.bg);
                    }
                }

                Self::apply_pen(out, &mut pen, next)?;

                if cell.graphemes_len()? == 0 {
                    queue!(out, Print(' '))?;
                } else {
                    cell.graphemes_utf8(&mut text)?;
                    queue!(out, Print(&text))?;
                }
            }

            row.set_dirty(false)?;
            y += 1;
        }

        // Report where the cursor should sit for this terminal.
        let cursor = if snapshot.cursor_visible()? {
            snapshot
                .cursor_viewport()?
                .map(|vp| (rect.x + vp.x, rect.y + vp.y as u16))
        } else {
            None
        };

        snapshot.set_dirty(Dirty::Clean)?;
        Ok(cursor)
    }

    /// Emit the escape sequences needed to go from `pen` to `next`.
    fn apply_pen(out: &mut impl Write, pen: &mut Pen, next: Pen) -> Result<()> {
        if *pen == next {
            return Ok(());
        }

        // Attributes can only be cleared by a full reset, which also
        // clears colors, so re-emit everything in that case.
        let attrs_changed = (pen.bold, pen.italic, pen.underline)
            != (next.bold, next.italic, next.underline);

        if attrs_changed {
            queue!(out, SetAttribute(Attribute::Reset))?;
            if next.bold {
                queue!(out, SetAttribute(Attribute::Bold))?;
            }
            if next.italic {
                queue!(out, SetAttribute(Attribute::Italic))?;
            }
            if next.underline {
                queue!(out, SetAttribute(Attribute::Underlined))?;
            }
        }
        if attrs_changed || pen.fg != next.fg {
            queue!(out, SetForegroundColor(next.fg))?;
        }
        if attrs_changed || pen.bg != next.bg {
            queue!(out, SetBackgroundColor(next.bg))?;
        }

        *pen = next;
        Ok(())
    }
}

fn color(rgb: RgbColor) -> Color {
    Color::Rgb {
        r: rgb.r,
        g: rgb.g,
        b: rgb.b,
    }
}
