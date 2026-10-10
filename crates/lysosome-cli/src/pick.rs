//! `lysosome clean --interactive`: a terminal multi-select over a plan.
//!
//! Split in two so the logic is testable without a terminal:
//! * [`Picker`] is a pure model: rows, cursor, ticks, key handling and
//!   rendering to plain lines.
//! * [`run`] drives it on stderr with raw key reads (the `console` crate,
//!   which indicatif already depends on).
//!
//! The picker only *narrows* a plan (see [`lysosome_core::clean::Plan::subset`]);
//! REVIEW items are shown dimmed with their reason and can't be ticked.

use std::collections::BTreeSet;
use std::io;

use lysosome_core::clean::{self, Plan, PlanItem, Selection};
use lysosome_core::model::{Category, Finding, Status, Tier};
use owo_colors::Style;

use crate::clean::{size_cell, target_of, tier_badge};
use crate::ui::{human_bytes, pad_left, pad_right, plural, Painter, PathShortener};

/// A key, as far as the picker cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input {
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
    Toggle,
    All,
    Confirm,
    Cancel,
    /// Ctrl-Z: suspend like any other program.
    Suspend,
    Other,
}

impl Input {
    fn from_key(k: console::Key) -> Input {
        use console::Key;
        match k {
            Key::ArrowUp | Key::Char('k') | Key::BackTab => Input::Up,
            Key::ArrowDown | Key::Char('j') | Key::Tab => Input::Down,
            Key::PageUp => Input::PageUp,
            Key::PageDown => Input::PageDown,
            Key::Home | Key::Char('g') => Input::Home,
            Key::End | Key::Char('G') => Input::End,
            Key::Char(' ') | Key::Char('x') => Input::Toggle,
            Key::Char('a') | Key::Char('A') => Input::All,
            Key::Enter => Input::Confirm,
            // Not a bare Escape: over a slow link an arrow key's escape
            // sequence can arrive split and read as Escape.
            Key::Char('q') | Key::Char('Q') | Key::CtrlC => Input::Cancel,
            // Ctrl-C / Ctrl-D as raw bytes on terminals that don't map them.
            Key::Char('\u{3}') | Key::Char('\u{4}') => Input::Cancel,
            Key::Char('\u{1a}') => Input::Suspend,
            _ => Input::Other,
        }
    }
}

/// What the caller should do after a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    Continue,
    Confirm,
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Row {
    Header {
        title: String,
        items: u64,
        bytes: u64,
    },
    /// A plan item (`idx` into `Plan::items`).
    Item {
        idx: usize,
        tier: Tier,
        size: String,
        bytes: u64,
        label: String,
        /// Extra context, e.g. "inside a REVIEW worktree".
        note: Option<String>,
        /// Listed items whose targets contain this one's: ticking any of
        /// them takes this one along.
        within: Vec<usize>,
    },
    /// A REVIEW finding: the cursor can rest on it, it can't be ticked.
    Held {
        size: String,
        label: String,
        reason: String,
    },
}

impl Row {
    fn focusable(&self) -> bool {
        !matches!(self, Row::Header { .. })
    }
}

/// The multi-select model.
#[derive(Debug, Clone)]
pub struct Picker {
    rows: Vec<Row>,
    ticked: BTreeSet<usize>,
    /// Index into `rows`; never a header when anything else exists.
    cursor: usize,
    /// First row shown (scrolling).
    top: usize,
}

impl Picker {
    /// Rows grouped by category (plan order), each group's REVIEW findings
    /// last. Items start ticked when their tier is one `sel` asked for
    /// (`--tier`, so `regenerable` by default); costlier ones are listed
    /// unticked. With `sel.strip_artifacts` the worktree group holds
    /// artifact folders inside worktrees (which are kept), titled like the
    /// plan.
    pub fn new(plan: &Plan, held: &[&Finding], paths: &PathShortener, sel: &Selection) -> Picker {
        let strip_artifacts = sel.strip_artifacts;
        let mut rows = Vec::new();
        let mut ticked = BTreeSet::new();
        for cat in Category::ALL {
            let items: Vec<(usize, &PlanItem)> = plan
                .items
                .iter()
                .enumerate()
                .filter(|(_, i)| i.category == cat)
                .collect();
            let held_here: Vec<&&Finding> = held.iter().filter(|f| f.category == cat).collect();
            if items.is_empty() && held_here.is_empty() {
                continue;
            }
            rows.push(Row::Header {
                title: if cat == Category::Worktrees && strip_artifacts {
                    "Artifacts inside dormant worktrees (worktrees kept)".to_string()
                } else {
                    cat.display_name().to_string()
                },
                items: items.len() as u64,
                bytes: items.iter().map(|(_, i)| i.size_bytes).sum(),
            });
            for (idx, i) in items {
                let mine = clean::targets(i);
                let within: Vec<usize> = plan
                    .items
                    .iter()
                    .enumerate()
                    .filter(|(n, other)| {
                        *n != idx
                            && !mine.is_empty()
                            && mine.iter().all(|t| {
                                clean::targets(other)
                                    .iter()
                                    .any(|o| t != o && t.starts_with(o))
                            })
                    })
                    .map(|(n, _)| n)
                    .collect();
                // An item holding a REVIEW finding never starts ticked.
                let holds_review = held
                    .iter()
                    .find(|f| mine.iter().any(|t| f.path != **t && f.path.starts_with(t)));
                if sel.tiers.contains(&i.tier) && i.tier != Tier::Review && holds_review.is_none() {
                    ticked.insert(idx);
                }
                let note = if let Some(f) = holds_review {
                    Some(format!(
                        "holds {} (REVIEW: {}); left unticked",
                        paths.short(&f.path),
                        f.reason
                    ))
                } else {
                    (i.status == Status::Review)
                        .then(|| "inside a REVIEW worktree; only this ignored folder goes".into())
                };
                rows.push(Row::Item {
                    idx,
                    tier: i.tier,
                    size: size_cell(i),
                    bytes: i.size_bytes,
                    label: target_of(i, paths),
                    note,
                    within,
                });
            }
            for f in held_here {
                // A REVIEW folder inside a listed item goes if that is ticked.
                let inside = plan.items.iter().any(|i| {
                    clean::targets(i)
                        .iter()
                        .any(|t| f.path != **t && f.path.starts_with(t))
                });
                rows.push(Row::Held {
                    size: human_bytes(f.size_bytes),
                    label: paths.short(&f.path),
                    reason: if inside {
                        format!(
                            "{}; inside a listed item, which would take it along",
                            f.reason
                        )
                    } else {
                        f.reason.clone()
                    },
                });
            }
        }
        let cursor = rows
            .iter()
            .position(|r| matches!(r, Row::Item { .. }))
            .or_else(|| rows.iter().position(Row::focusable))
            .unwrap_or(0);
        Picker {
            rows,
            ticked,
            cursor,
            top: 0,
        }
    }

    /// Plan indices currently ticked.
    pub fn selected(&self) -> &BTreeSet<usize> {
        &self.ticked
    }

    fn item_rows(&self) -> impl Iterator<Item = (usize, usize, u64)> + '_ {
        self.rows
            .iter()
            .enumerate()
            .filter_map(|(r, row)| match row {
                Row::Item { idx, bytes, .. } => Some((r, *idx, *bytes)),
                _ => None,
            })
    }

    /// Taken along by a ticked item it sits inside.
    fn covered(&self, row: &Row) -> bool {
        matches!(row, Row::Item { within, .. } if within.iter().any(|w| self.ticked.contains(w)))
    }

    /// Items that would be acted on (ticked or covered) and their bytes,
    /// counting covered items' bytes once (inside their parent's).
    fn effective(&self) -> (usize, u64) {
        let mut n = 0;
        let mut bytes = 0;
        for row in &self.rows {
            if let Row::Item { idx, bytes: b, .. } = row {
                let covered = self.covered(row);
                if covered || self.ticked.contains(idx) {
                    n += 1;
                }
                if !covered && self.ticked.contains(idx) {
                    bytes += b;
                }
            }
        }
        (n, bytes)
    }

    fn selectable(&self) -> usize {
        self.item_rows().count()
    }

    /// Move the cursor `delta` focusable rows (items and REVIEW rows, never
    /// headers), clamped to the first/last one.
    fn move_by(&mut self, delta: isize) {
        let stops: Vec<usize> = (0..self.rows.len())
            .filter(|r| self.rows[*r].focusable())
            .collect();
        if stops.is_empty() {
            return;
        }
        let pos = stops.iter().position(|r| *r == self.cursor).unwrap_or(0) as isize;
        let next = (pos + delta).clamp(0, stops.len() as isize - 1) as usize;
        self.cursor = stops[next];
    }

    pub fn handle(&mut self, input: Input) -> Step {
        match input {
            Input::Up => self.move_by(-1),
            Input::Down => self.move_by(1),
            Input::PageUp => self.move_by(-10),
            Input::PageDown => self.move_by(10),
            Input::Home => self.move_by(isize::MIN / 2),
            Input::End => self.move_by(isize::MAX / 2),
            Input::Toggle => {
                if let Some(row @ Row::Item { idx, .. }) = self.rows.get(self.cursor) {
                    // Covered by a ticked parent: it goes with it either way.
                    if !self.covered(row) && !self.ticked.remove(idx) {
                        self.ticked.insert(*idx);
                    }
                }
            }
            Input::All => {
                let all: BTreeSet<usize> = self.item_rows().map(|(_, i, _)| i).collect();
                self.ticked = if self.ticked == all {
                    BTreeSet::new()
                } else {
                    all
                };
            }
            Input::Confirm => return Step::Confirm,
            Input::Cancel => return Step::Cancel,
            Input::Suspend | Input::Other => {}
        }
        Step::Continue
    }

    /// The screen as lines (no trailing newlines). Never more than
    /// `height - 1` lines (one spare for the cursor) and, after the caller
    /// truncates to `width`, never wraps, so a redraw can erase exactly what
    /// it drew. Long paths are shortened from the left so sizes and reasons
    /// stay visible.
    pub fn render(&mut self, p: &Painter, height: usize, width: usize) -> Vec<String> {
        // Title + status + help around the list; on a tiny terminal, the list
        // and status only.
        let chrome = match height {
            0..=2 => 0,
            3..=7 => 1,
            _ => 3,
        };
        let room = height.saturating_sub(chrome + 1).max(1);
        let n = self.rows.len();
        if self.cursor < self.top {
            self.top = self.cursor;
        }
        if self.cursor >= self.top + room {
            self.top = self.cursor + 1 - room;
        }
        // Show a group's header when its first row is at the top.
        if self.top > 0
            && self.top == self.cursor
            && room > 1
            && matches!(self.rows[self.top - 1], Row::Header { .. })
        {
            self.top -= 1;
        }
        self.top = self.top.min(n.saturating_sub(room));
        let end = (self.top + room).min(n);
        let ws = self
            .rows
            .iter()
            .filter_map(|r| match r {
                Row::Item { size, .. } | Row::Held { size, .. } => Some(size.chars().count()),
                _ => None,
            })
            .max()
            .unwrap_or(6);
        let mut out = Vec::with_capacity(room + chrome);
        if chrome == 3 {
            out.push(format!(
                "  {} {}",
                p.bold("Choose what to clean"),
                p.dim("· every item is re-checked before anything happens")
            ));
        }
        for r in self.top..end {
            let at = r == self.cursor;
            let covered = self.covered(&self.rows[r]);
            let pointer = if at {
                p.paint("❯", Style::new().cyan().bold())
            } else {
                " ".into()
            };
            let line = match &self.rows[r] {
                Row::Header {
                    title,
                    items,
                    bytes,
                } => format!(
                    "  {} {}  {}",
                    p.dim("──"),
                    p.paint(title, Style::new().bold().underline()),
                    p.dim(format!(
                        "{} · {}",
                        plural(*items, "item", "items"),
                        human_bytes(*bytes)
                    ))
                ),
                Row::Item {
                    idx,
                    tier,
                    size,
                    label,
                    note,
                    ..
                } => {
                    let mark = if covered {
                        p.dim("[x]")
                    } else if self.ticked.contains(idx) {
                        p.paint("[x]", Style::new().green().bold())
                    } else {
                        "[ ]".to_string()
                    };
                    let note = if covered {
                        "  (inside a ticked item above; goes with it)".to_string()
                    } else {
                        note.as_deref()
                            .map(|n| format!("  ({n})"))
                            .unwrap_or_default()
                    };
                    let room = width.saturating_sub(18 + ws + note.chars().count() + 1);
                    let text = format!(
                        "{}  {}  {}",
                        tier_badge(*tier, p),
                        pad_left(size, ws),
                        fit_tail(label, room)
                    );
                    format!(
                        "  {pointer} {mark} {}{}",
                        if at { p.bold(text) } else { text },
                        p.dim(note)
                    )
                }
                Row::Held {
                    size,
                    label,
                    reason,
                } => {
                    let reason = crate::ui::truncate(reason, 60);
                    let room = width.saturating_sub(18 + ws + 4 + reason.chars().count() + 1);
                    format!(
                        "  {pointer} {}",
                        p.dim(format!(
                            "[-] {}  {}  {}  ({reason})",
                            pad_right("review", 6),
                            pad_left(size, ws),
                            fit_tail(label, room)
                        ))
                    )
                }
            };
            out.push(line);
        }
        let more_above = self.top;
        let more_below = n - end;
        let (picked_n, picked_bytes) = self.effective();
        let mut status = format!(
            "  {} {}",
            p.bold(format!("Selected {} of {}", picked_n, self.selectable())),
            p.dim(format!("· {}", human_bytes(picked_bytes)))
        );
        if more_above + more_below > 0 {
            status.push_str(&p.dim(format!(
                "  ({} more {})",
                more_above + more_below,
                match (more_above > 0, more_below > 0) {
                    (true, true) => "above and below",
                    (true, false) => "above",
                    _ => "below",
                }
            )));
        }
        if matches!(self.rows.get(self.cursor), Some(Row::Held { .. })) {
            status.push_str(&p.dim("  · REVIEW items can't be selected"));
        }
        if chrome > 0 {
            out.push(status);
        }
        if chrome == 3 {
            out.push(format!(
                "  {}",
                p.dim("↑/↓ move · space toggle · a all/none · enter continue · q cancel")
            ));
        }
        out
    }
}

/// REVIEW findings as static lines (when there is nothing to pick).
pub fn held_lines(held: &[&Finding], p: &Painter, paths: &PathShortener) -> String {
    if held.is_empty() {
        return String::new();
    }
    let mut o = format!("\n  {}\n", p.bold("Kept for review (never cleaned):"));
    let ws = held
        .iter()
        .map(|f| human_bytes(f.size_bytes).chars().count())
        .max()
        .unwrap_or(6);
    for f in held {
        o.push_str(&format!(
            "    {}  {}  {}\n",
            pad_left(&human_bytes(f.size_bytes), ws),
            paths.short(&f.path),
            p.dim(format!("({})", f.reason))
        ));
    }
    o
}

/// Keep the end of `s` (the folder name matters most), at least 16 columns.
fn fit_tail(s: &str, max: usize) -> String {
    let max = max.max(16);
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let tail: String = s.chars().skip(n - (max - 1)).collect();
    format!("…{tail}")
}

/// The terminal state for one picker session: keys unechoed and unbuffered,
/// Ctrl-C delivered as a key (no SIGINT), cursor hidden. Restored on drop,
/// even on an error or panic, with any typed-ahead input discarded so it
/// can't answer the confirmation prompt that follows.
struct Session<'a> {
    term: &'a console::Term,
    saved: libc::termios,
    active: bool,
}

impl<'a> Session<'a> {
    fn start(term: &'a console::Term) -> io::Result<Session<'a>> {
        // SAFETY: tcgetattr fully initializes `t` on success.
        let saved = unsafe {
            let mut t = std::mem::zeroed::<libc::termios>();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut t) != 0 {
                return Err(io::Error::last_os_error());
            }
            t
        };
        let mut s = Session {
            term,
            saved,
            active: false,
        };
        s.enter()?;
        Ok(s)
    }

    fn enter(&mut self) -> io::Result<()> {
        let mut t = self.saved;
        t.c_lflag &= !(libc::ECHO | libc::ICANON | libc::ISIG | libc::IEXTEN);
        t.c_cc[libc::VMIN] = 1;
        t.c_cc[libc::VTIME] = 0;
        // SAFETY: plain termios call on stdin (checked to be a terminal).
        if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &t) } != 0 {
            return Err(io::Error::last_os_error());
        }
        self.active = true;
        self.term.hide_cursor()
    }

    fn leave(&mut self) {
        if !self.active {
            return;
        }
        self.active = false;
        let _ = self.term.show_cursor();
        // SAFETY: restoring the attributes read in `start`.
        unsafe {
            libc::tcflush(libc::STDIN_FILENO, libc::TCIFLUSH);
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.saved);
        }
    }

    /// Ctrl-Z: give the terminal back, stop, and take it again on `fg`.
    fn suspend(&mut self) -> io::Result<()> {
        self.leave();
        // SAFETY: plain syscall; the default SIGTSTP action stops us here.
        unsafe {
            libc::raise(libc::SIGTSTP);
        }
        self.enter()
    }
}

impl Drop for Session<'_> {
    fn drop(&mut self) {
        self.leave();
    }
}

/// Terminal rows the lines we drew occupy at `width` columns (a narrower
/// terminal re-wraps them).
fn rows_at(widths: &[usize], width: usize) -> usize {
    let w = width.max(1);
    widths.iter().map(|n| n.max(&1).div_ceil(w)).sum()
}

/// Run the picker on stderr. `Ok(Some(picked))` on enter, `Ok(None)` when
/// cancelled. The picker's lines are erased afterwards, also on an error.
pub fn run(mut picker: Picker, p: &Painter) -> io::Result<Option<BTreeSet<usize>>> {
    let term = console::Term::stderr();
    let mut session = Session::start(&term)?;
    // Visible widths of the lines on screen.
    let mut drawn: Vec<usize> = Vec::new();
    let result = (|| loop {
        let (h, w) = term.size();
        let lines = picker.render(p, h as usize, w as usize);
        term.clear_last_lines(rows_at(&drawn, w as usize))?;
        drawn.clear();
        for l in &lines {
            // One terminal line per row, or clearing would miscount.
            let fit = console::truncate_str(l, (w as usize).saturating_sub(1).max(1), "…");
            term.write_line(&fit)?;
            drawn.push(console::measure_text_width(&fit));
        }
        let input = Input::from_key(term.read_key_raw()?);
        if input == Input::Suspend {
            session.suspend()?;
            continue;
        }
        match picker.handle(input) {
            Step::Continue => {}
            Step::Confirm => return Ok(Some(picker.selected().clone())),
            Step::Cancel => return Ok(None),
        }
    })();
    let _ = term.clear_last_lines(rows_at(&drawn, term.size().1 as usize));
    drop(session);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use lysosome_core::clean::PlanOptions;
    use lysosome_core::model::{Cleanup, Status};
    use serde_json::json;
    use std::path::PathBuf;

    fn finding(kind: &str, cat: Category, status: Status, tier: Tier, size: u64) -> Finding {
        Finding {
            id: format!("{kind}:/h/{kind}"),
            category: cat,
            kind: kind.into(),
            path: PathBuf::from(format!("/h/{kind}")),
            size_bytes: size,
            tier,
            status,
            reason: format!("{kind} reason"),
            last_activity: None,
            details: json!({}),
            cleanup: Some(Cleanup::command(vec![kind.into(), "clean".into()])),
        }
    }

    fn fixture() -> (Plan, Vec<Finding>) {
        let findings = vec![
            finding(
                "npm",
                Category::PackageCaches,
                Status::Removable,
                Tier::Regenerable,
                300,
            ),
            finding(
                "brew",
                Category::PackageCaches,
                Status::Removable,
                Tier::Regenerable,
                200,
            ),
            finding(
                "ollama",
                Category::AiModels,
                Status::Removable,
                Tier::RegenerableExpensive,
                900,
            ),
            finding(
                "vol",
                Category::Containers,
                Status::Review,
                Tier::Review,
                50,
            ),
        ];
        let mut sel = Selection::default();
        sel.tiers.insert(Tier::RegenerableExpensive);
        let plan = clean::plan(&findings, &sel, PlanOptions::default());
        (plan, findings)
    }

    fn kinds(plan: &Plan, picked: &BTreeSet<usize>) -> Vec<String> {
        plan.subset(picked, &Selection::default().tiers)
            .items
            .into_iter()
            .map(|i| i.kind)
            .collect()
    }

    #[test]
    fn regenerable_items_start_ticked_and_review_rows_are_not_selectable() {
        let (plan, findings) = fixture();
        let held = clean::held_for_review(
            &findings,
            &Selection::default(),
            std::time::SystemTime::now(),
        );
        let mut pk = Picker::new(
            &plan,
            &held,
            &PathShortener::default(),
            &Selection::default(),
        );
        assert_eq!(kinds(&plan, pk.selected()), ["npm", "brew"]);
        assert_eq!(pk.selectable(), 3);
        // The cursor can rest on the REVIEW row (so it scrolls into view),
        // but it can't be ticked.
        for _ in 0..10 {
            pk.handle(Input::Down);
        }
        assert!(matches!(pk.rows[pk.cursor], Row::Held { .. }));
        let before = pk.selected().clone();
        pk.handle(Input::Toggle);
        assert_eq!(pk.selected(), &before);
        assert!(pk
            .render(&Painter::new(false), 40, 120)
            .join("\n")
            .contains("REVIEW items can't be selected"));
        pk.handle(Input::Home);
        pk.handle(Input::Toggle);
        assert_eq!(kinds(&plan, pk.selected()), ["ollama", "npm", "brew"]);
        let text = pk.render(&Painter::new(false), 40, 120).join("\n");
        assert!(text.contains("[-] review"), "{text}");
        assert!(text.contains("vol reason"), "{text}");
        assert!(text.contains("Selected 3 of 3"), "{text}");
    }

    #[test]
    fn an_explicit_tier_preselects_costly_items_too() {
        let (plan, _) = fixture();
        let mut sel = Selection::default();
        sel.tiers.insert(Tier::RegenerableExpensive);
        let pk = Picker::new(&plan, &[], &PathShortener::default(), &sel);
        assert_eq!(kinds(&plan, pk.selected()), ["ollama", "npm", "brew"]);
    }

    #[test]
    fn toggle_all_none_and_confirm_cancel() {
        let (plan, _) = fixture();
        let mut pk = Picker::new(&plan, &[], &PathShortener::default(), &Selection::default());
        assert_eq!(pk.handle(Input::All), Step::Continue);
        assert_eq!(pk.selected().len(), 3, "not all ticked: `a` ticks all");
        pk.handle(Input::All);
        assert!(pk.selected().is_empty(), "all ticked: `a` clears");
        pk.handle(Input::Home);
        pk.handle(Input::Toggle);
        // First row in plan order is the AI model (category order).
        assert_eq!(kinds(&plan, pk.selected()), ["ollama"]);
        pk.handle(Input::Toggle);
        assert!(pk.selected().is_empty());
        assert_eq!(pk.handle(Input::Confirm), Step::Confirm);
        assert_eq!(pk.handle(Input::Cancel), Step::Cancel);
        assert_eq!(Input::from_key(console::Key::Char(' ')), Input::Toggle);
        assert_eq!(Input::from_key(console::Key::Char('a')), Input::All);
        assert_eq!(Input::from_key(console::Key::Enter), Input::Confirm);
        assert_eq!(Input::from_key(console::Key::CtrlC), Input::Cancel);
    }

    #[test]
    fn long_lists_scroll_and_keep_the_cursor_visible() {
        let findings: Vec<Finding> = (0..40)
            .map(|n| {
                finding(
                    &format!("c{n:02}"),
                    Category::PackageCaches,
                    Status::Removable,
                    Tier::Regenerable,
                    1000 - n,
                )
            })
            .collect();
        let plan = clean::plan(&findings, &Selection::default(), PlanOptions::default());
        let mut pk = Picker::new(&plan, &[], &PathShortener::default(), &Selection::default());
        let p = Painter::new(false);
        let first = pk.render(&p, 12, 120);
        assert_eq!(first.len(), 11, "one spare line for the cursor");
        assert!(first[1].contains("Package caches"), "{first:?}");
        assert!(first.join("\n").contains("more below"));
        pk.handle(Input::End);
        let last = pk.render(&p, 12, 120);
        assert_eq!(last.len(), 11);
        assert!(
            last.iter().any(|l| l.contains("❯") && l.contains("c39")),
            "{last:?}"
        );
        assert!(last.join("\n").contains("more above"));
    }

    #[test]
    fn review_rows_at_either_end_scroll_into_view_and_frames_fit() {
        let mut findings: Vec<Finding> = (0..30)
            .map(|n| {
                finding(
                    &format!("c{n:02}"),
                    Category::PackageCaches,
                    Status::Removable,
                    Tier::Regenerable,
                    1000 - n,
                )
            })
            .collect();
        // A REVIEW-only group first (worktrees) and REVIEW rows last.
        findings.push(finding(
            "wt",
            Category::Worktrees,
            Status::Review,
            Tier::Review,
            5,
        ));
        findings.push(finding(
            "vol",
            Category::Containers,
            Status::Review,
            Tier::Review,
            5,
        ));
        let plan = clean::plan(&findings, &Selection::default(), PlanOptions::default());
        let held = clean::held_for_review(
            &findings,
            &Selection::default(),
            std::time::SystemTime::now(),
        );
        let mut pk = Picker::new(
            &plan,
            &held,
            &PathShortener::default(),
            &Selection::default(),
        );
        let p = Painter::new(false);
        pk.handle(Input::End);
        let text = pk.render(&p, 12, 120).join("\n");
        assert!(text.contains("vol reason"), "{text}");
        pk.handle(Input::Home);
        let text = pk.render(&p, 12, 120).join("\n");
        assert!(
            text.contains("Git worktrees") && text.contains("wt reason"),
            "{text}"
        );
        for h in [1, 2, 3, 5, 8, 12] {
            assert!(pk.render(&p, h, 120).len() <= (h - 1).max(1), "height {h}");
        }
    }

    #[test]
    fn strip_artifacts_rows_say_the_worktree_is_kept() {
        let mut f = finding("x", Category::Worktrees, Status::Review, Tier::Review, 5);
        f.path = PathBuf::from("/c/wt");
        f.details = json!({
            "gitdir": "/c/app/.git/worktrees/wt",
            "artifacts": [{"path": "/c/wt/target", "size_bytes": 10}],
        });
        f.cleanup = None;
        let sel = Selection {
            strip_artifacts: true,
            ..Selection::default()
        };
        let plan = clean::plan(&[f], &sel, PlanOptions::default());
        assert_eq!(plan.items.len(), 1);
        let mut pk = Picker::new(&plan, &[], &PathShortener::default(), &sel);
        let text = pk.render(&Painter::new(false), 20, 160).join("\n");
        assert!(text.contains("worktrees kept"), "{text}");
        assert!(text.contains("inside a REVIEW worktree"), "{text}");
        assert!(text.contains("/c/wt/target"), "{text}");
    }

    #[test]
    fn items_inside_a_ticked_item_go_with_it_and_count_once() {
        let mk = |path: &str, size| {
            let mut f = finding(
                "dd",
                Category::Xcode,
                Status::Removable,
                Tier::Regenerable,
                size,
            );
            f.id = format!("dd:{path}");
            f.path = PathBuf::from(path);
            f.cleanup = Some(Cleanup::trash(vec![PathBuf::from(path)]));
            f
        };
        let findings = vec![mk("/h/big", 100), mk("/h/big/small", 10)];
        let opts = PlanOptions {
            keep_nested: true,
            ..PlanOptions::default()
        };
        let plan = clean::plan(&findings, &Selection::default(), opts);
        assert_eq!(plan.items.len(), 2);
        let mut pk = Picker::new(&plan, &[], &PathShortener::default(), &Selection::default());
        let p = Painter::new(false);
        let text = pk.render(&p, 20, 160).join("\n");
        assert!(
            text.contains("Selected 2 of 2 · 100 B"),
            "counted once: {text}"
        );
        assert!(text.contains("goes with it"), "{text}");
        // The covered child can't be unticked while its parent is ticked.
        let small_row = pk
            .rows
            .iter()
            .position(|r| matches!(r, Row::Item { label, .. } if label.ends_with("small")))
            .unwrap();
        pk.cursor = small_row;
        let before = pk.selected().clone();
        pk.handle(Input::Toggle);
        assert_eq!(pk.selected(), &before);
        // Untick the parent: the child is free, still ticked, and stays planned.
        pk.handle(Input::Home);
        pk.handle(Input::Toggle);
        let sub = plan.subset(pk.selected(), &Selection::default().tiers);
        assert_eq!(sub.items.len(), 1);
        assert!(sub.items[0].path.ends_with("small"));
        assert_eq!(sub.excluded.deselected.bytes, 100);
    }

    #[test]
    fn items_holding_a_review_finding_start_unticked() {
        let mut outer = finding(
            "dd",
            Category::Xcode,
            Status::Removable,
            Tier::Regenerable,
            9,
        );
        outer.path = PathBuf::from("/h/dd");
        outer.cleanup = Some(Cleanup::trash(vec![PathBuf::from("/h/dd")]));
        let mut inner = finding("x", Category::Xcode, Status::Review, Tier::Review, 1);
        inner.path = PathBuf::from("/h/dd/keep");
        let findings = vec![outer, inner];
        let plan = clean::plan(&findings, &Selection::default(), PlanOptions::default());
        let held = clean::held_for_review(
            &findings,
            &Selection::default(),
            std::time::SystemTime::now(),
        );
        let mut pk = Picker::new(
            &plan,
            &held,
            &PathShortener::default(),
            &Selection::default(),
        );
        assert!(pk.selected().is_empty());
        let text = pk.render(&Painter::new(false), 20, 200).join("\n");
        assert!(
            text.contains("holds /h/dd/keep (REVIEW: x reason); left unticked"),
            "{text}"
        );
        assert!(text.contains("which would take it along"), "{text}");
    }

    #[test]
    fn redraw_accounts_for_rewrapped_lines() {
        assert_eq!(rows_at(&[10, 10], 80), 2);
        assert_eq!(rows_at(&[130, 0], 80), 3);
        assert_eq!(Input::from_key(console::Key::Escape), Input::Other);
        assert_eq!(
            Input::from_key(console::Key::Char('\u{1a}')),
            Input::Suspend
        );
    }

    #[test]
    fn long_paths_are_shortened_from_the_left() {
        assert_eq!(fit_tail("/a/b", 30), "/a/b");
        let long = format!("/very{}/project/app-feature-x", "/deep".repeat(20));
        let fit = fit_tail(&long, 30);
        assert_eq!(fit.chars().count(), 30);
        assert!(fit.starts_with('…') && fit.ends_with("/project/app-feature-x"));
        assert_eq!(fit_tail(&long, 3).chars().count(), 16, "never below 16");
    }
}
