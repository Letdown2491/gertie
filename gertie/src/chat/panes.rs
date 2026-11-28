//! The two panes (issue #54): paper for words, ink for work, and
//! Gertie's face on the seam between them. This module owns the work
//! pane, the folded strip, the seam (the face, its progress ring, the
//! thread of numbered marks), the words header, and the small shared
//! pieces (marks, density bars). The words pane's rows stay in
//! transcript.rs; they read the thread from here.

use super::shape::{self, Kind, Shape, Stage};
use super::transcript::diff_element;
use super::*;
use crate::ink::{self, Palette};
use gpui_kit::{
    Div, Hsla, LineCap, PathBuilder, PathStyle, Pixels, StrokeOptions, canvas, point, relative, rgb,
};

/// The header band both panes share; the face sits centered on it.
pub(crate) const HEADER_HEIGHT: f32 = 150.;
/// The face's disc on the seam.
const FACE_DISC: f32 = 132.;
/// The face's disc when the work side is folded to its strip.
const FACE_DISC_SMALL: f32 = 100.;
/// The folded work side.
pub(crate) const STRIP_WIDTH: f32 = 64.;
/// The words pane at full width, and the least it yields to.
const WORDS_WIDTH: f32 = 620.;
const WORDS_MIN: f32 = 480.;
/// Under this window width the work side folds to its strip.
const FOLD_BELOW: f32 = 1100.;

/// Gertie's expression on the seam: the face says what the turn is
/// doing, the way GERTY's screen does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mood {
    /// Nothing has happened yet.
    Ready,
    /// The model is thinking or writing, no tool yet.
    Thinking,
    /// Reading and searching.
    Curious,
    /// Changing files.
    Focused,
    /// A check is running.
    Checking,
    /// An ask or a permission waits on the human.
    Concerned,
    /// The turn settled and nothing failed.
    Happy,
    /// The turn settled on a failed check or an error.
    Sad,
}

/// What the seam shows: the face, the ring, and the caption.
#[derive(Clone, Debug, PartialEq)]
pub struct SeamState {
    pub mood: Mood,
    /// The ring's fill, 0 to 1.
    pub fill: f32,
    /// The vermilion head on the ring: the turn is live.
    pub live: bool,
    pub verb: &'static str,
    pub detail: String,
}

/// A memory proposal seal's three answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProposalAnswer {
    Keep,
    Edit,
    LetGo,
}

/// Where the work pane's content comes from, and how it shows.
pub(crate) struct WorkView {
    /// The span of transcript rows the pane describes.
    pub start: usize,
    pub end: usize,
    pub shape: Shape,
    /// The span is the running turn.
    pub running: bool,
}

/// A numbered mark: outlined when done, filled vermilion while live.
pub(crate) fn mark(number: usize, live: bool, on_ink: bool, pal: Palette) -> Div {
    let text = if on_ink { pal.ink_text } else { pal.text };
    let base = div()
        .flex_shrink_0()
        .size(px(20.))
        .rounded_full()
        .flex()
        .items_center()
        .justify_center()
        .font_family(ink::SANS)
        .text_size(px(11.))
        .font_weight(gpui_kit::FontWeight::SEMIBOLD)
        .child(number.to_string());
    if live {
        base.bg(pal.seal).text_color(pal.seal_text)
    } else {
        base.border_1().border_color(text).text_color(text)
    }
}

/// The +/- density bar: blue blocks added, vermilion-light removed.
pub(crate) fn density_bar(adds: usize, removes: usize, pal: Palette) -> Div {
    let (add_blocks, remove_blocks) = shape::density(adds, removes);
    let block = |color: Hsla| div().w(px(4.)).h(px(12.)).bg(color);
    h_flex()
        .gap(px(2.))
        .children((0..add_blocks).map(move |_| block(pal.added)))
        .children((0..remove_blocks).map(move |_| block(pal.removed)))
}

/// Whether a tool result reads as a failure: the cards' own rule.
pub(crate) fn result_failed(result: &str) -> bool {
    result.starts_with("error:")
        || result.starts_with("exit ")
        || result.starts_with("blocked by policy")
        || result.ends_with("denied by user")
}

/// A settled memory proposal's outcome, kept on its row so the seal
/// and the work header can say which way it went.
pub(crate) const PROPOSAL_KEPT: &str = "kept";
pub(crate) const PROPOSAL_LET_GO: &str = "let go";

/// One proposed memory change, as the seal shows it: what kind of
/// change, and its words.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProposedLine {
    pub kind: String,
    pub text: String,
}

/// A proposal card's lines, from its arguments (one JSON field holding
/// the summary, a change per line). The evidence tail is the panel's
/// detail; the seal shows the rule.
pub(crate) fn proposal_lines(arguments: &str) -> Vec<ProposedLine> {
    let value: serde_json::Value = serde_json::from_str(arguments).unwrap_or_default();
    let summary = value
        .as_object()
        .and_then(|object| object.values().next())
        .and_then(|value| value.as_str())
        .unwrap_or_default();
    summary
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| match line.split_once(" :: ") {
            Some((head, text)) => {
                let text = match text.rfind(" (evidence: ") {
                    Some(at) if text.ends_with(')') => &text[..at],
                    _ => text,
                };
                let kind = if head.starts_with("add correction") {
                    "a correction"
                } else if head.starts_with("update") {
                    "an update"
                } else {
                    "new"
                };
                ProposedLine {
                    kind: kind.to_string(),
                    text: text.to_string(),
                }
            }
            None => ProposedLine {
                kind: "tidy".to_string(),
                text: line.to_string(),
            },
        })
        .collect()
}

/// The test counts a check printed, summed over its result lines
/// ("test result: ok. 98 passed; 0 failed", "12 passed, 1 failed").
pub(crate) fn test_counts(result: &str) -> Option<(usize, usize)> {
    let mut passed = 0;
    let mut failed = 0;
    let mut seen = false;
    for line in result.lines() {
        let words: Vec<&str> = line
            .split(|c: char| c.is_whitespace() || c == ';' || c == ',')
            .filter(|word| !word.is_empty())
            .collect();
        for pair in words.windows(2) {
            let Ok(count) = pair[0].parse::<usize>() else {
                continue;
            };
            match pair[1] {
                "passed" => {
                    passed += count;
                    seen = true;
                }
                "failed" => {
                    failed += count;
                    seen = true;
                }
                _ => {}
            }
        }
    }
    seen.then_some((passed, failed))
}

/// A shell command without its `cd <workspace> && ` lead: every call
/// runs there already, the prefix is noise on a card.
pub(crate) fn strip_cd(command: &str, root: &str) -> String {
    let trimmed = command.trim();
    for prefix in [
        format!("cd {root} && "),
        format!("cd {root}; "),
        format!("cd \"{root}\" && "),
    ] {
        if let Some(rest) = trimmed.strip_prefix(&prefix) {
            return rest.to_string();
        }
    }
    trimmed.to_string()
}

/// Points around a pill (a rounded rect whose short side is round),
/// for a filled eye.
fn pill(center_x: f32, center_y: f32, width: f32, height: f32) -> Vec<gpui_kit::Point<Pixels>> {
    let radius = width.min(height) / 2.;
    let half_w = width / 2. - radius;
    let half_h = height / 2. - radius;
    let mut points = Vec::new();
    let arc = |points: &mut Vec<_>, ox: f32, oy: f32, from: f32| {
        for step in 0..=8 {
            let angle = (from + step as f32 * 90. / 8.).to_radians();
            points.push(point(
                px(ox + radius * angle.cos()),
                px(oy + radius * angle.sin()),
            ));
        }
    };
    arc(&mut points, center_x + half_w, center_y + half_h, 0.);
    arc(&mut points, center_x - half_w, center_y + half_h, 90.);
    arc(&mut points, center_x - half_w, center_y - half_h, 180.);
    arc(&mut points, center_x + half_w, center_y - half_h, 270.);
    points
}

fn round_stroke(width: f32) -> PathBuilder {
    PathBuilder::stroke(px(width)).with_style(PathStyle::Stroke(StrokeOptions {
        line_width: width,
        line_cap: LineCap::Round,
        ..Default::default()
    }))
}

/// Where the ring's head sits for a fill, relative to the face's box.
fn ring_head(size: f32, fill: f32) -> (f32, f32) {
    let ring_radius = size / 2. - 9.;
    let angle = (fill.clamp(0., 0.985) * 360. - 90.).to_radians();
    (
        size / 2. + ring_radius * angle.cos(),
        size / 2. + ring_radius * angle.sin(),
    )
}

/// An element that pulses while motion is on: the breathing of a live
/// thing. gpui renders it still when reduce-motion is set.
pub(crate) fn pulsing<E>(
    element: E,
    id: impl Into<gpui_kit::ElementId>,
    period_ms: u64,
) -> AnyElement
where
    E: IntoElement + Styled + 'static,
{
    use gpui_kit::{Animation, AnimationExt as _, pulsating_between};
    element
        .with_animation(
            id,
            Animation::new(std::time::Duration::from_millis(period_ms))
                .repeat()
                .with_easing(pulsating_between(0.35, 1.0))
                .with_max_fps(30.),
            |element, delta| element.opacity(delta),
        )
        .into_any_element()
}

/// A thread mark that blinks while live (#54).
pub(crate) fn live_mark(
    number: usize,
    live: bool,
    on_ink: bool,
    pal: Palette,
    scope: &str,
) -> AnyElement {
    let base = mark(number, live, on_ink, pal);
    if live {
        pulsing(
            base,
            SharedString::from(format!("mark-{scope}-{number}")),
            1100,
        )
    } else {
        base.into_any_element()
    }
}

/// The face plate's colors, the launcher icon's own.
const PLATE: u32 = 0x11163F;
const MINT: u32 = 0x7CF5E0;

/// Gertie's face with its progress ring, drawn on one canvas: the ring
/// around the disc, the icon's face plate, eyes and mouth by mood.
fn face(size: f32, seam: &SeamState, pal: Palette) -> impl IntoElement {
    let seam = seam.clone();
    canvas(
        |_, _, _| {},
        move |bounds, _, window, _| {
            let ox: f32 = bounds.origin.x.into();
            let oy: f32 = bounds.origin.y.into();
            let center = (ox + size / 2., oy + size / 2.);
            let ring_radius = size / 2. - 9.;
            let at = |fraction: f32| {
                let angle = (fraction * 360. - 90.).to_radians();
                (
                    center.0 + ring_radius * angle.cos(),
                    center.1 + ring_radius * angle.sin(),
                )
            };
            // The track: the whole circle, faint, so the filled part
            // reads as how far.
            let mut track = round_stroke(1.5);
            for step in 0..=72 {
                let (x, y) = at(step as f32 / 72.);
                if step == 0 {
                    track.move_to(point(px(x), px(y)));
                } else {
                    track.line_to(point(px(x), px(y)));
                }
            }
            if let Ok(path) = track.build() {
                window.paint_path(path, pal.line);
            }
            // The ring: from the top, clockwise, as far as the turn got.
            let fill = seam.fill.clamp(0., 0.985);
            if fill > 0.005 {
                let mut ring = round_stroke(6.);
                let steps = ((fill * 120.).ceil() as usize).max(2);
                for step in 0..=steps {
                    let (x, y) = at(fill * step as f32 / steps as f32);
                    if step == 0 {
                        ring.move_to(point(px(x), px(y)));
                    } else {
                        ring.line_to(point(px(x), px(y)));
                    }
                }
                if let Ok(path) = ring.build() {
                    window.paint_path(path, pal.text);
                }
            }
            // The stage notches: small cuts in the ring at the
            // boundaries between understand, change and verify. The
            // live head is its own element over the canvas, so it can
            // pulse (see `ring_head`).
            for boundary in [0.25, 0.5, 0.75] {
                let angle = (boundary * 360. - 90.0f32).to_radians();
                let (cos, sin) = (angle.cos(), angle.sin());
                let mut notch = round_stroke(2.);
                notch.move_to(point(
                    px(center.0 + (ring_radius - 5.) * cos),
                    px(center.1 + (ring_radius - 5.) * sin),
                ));
                notch.line_to(point(
                    px(center.0 + (ring_radius + 5.) * cos),
                    px(center.1 + (ring_radius + 5.) * sin),
                ));
                if let Ok(path) = notch.build() {
                    window.paint_path(path, pal.disc);
                }
            }
            // The face plate: the icon's dark screen, a little above
            // center so the caption fits under it.
            let plate_w = size * 0.5;
            let plate_h = size * 0.32;
            let plate_center = (center.0, center.1 - size * 0.12);
            let mut plate = PathBuilder::fill();
            plate.add_polygon(
                &rounded_rect(plate_center.0, plate_center.1, plate_w, plate_h, size * 0.1),
                true,
            );
            if let Ok(path) = plate.build() {
                window.paint_path(path, rgb(PLATE));
            }
            let mint: Hsla = rgb(MINT).into();
            let eye_dx = plate_w * 0.2;
            let eye_y = plate_center.1 - plate_h * 0.08;
            let eye_w = plate_w * 0.17;
            let eye_h = plate_h * 0.42;
            let mouth_y = plate_center.1 + plate_h * 0.28;
            let mouth_w = plate_w * 0.22;
            // Eyes, by mood: open pills, shifted when looking, short
            // when focused, arcs when happy.
            let eyes: Option<(f32, f32, f32)> = match seam.mood {
                Mood::Ready | Mood::Concerned | Mood::Sad => Some((0., 0., eye_h)),
                Mood::Thinking => Some((eye_w * 0.35, -eye_h * 0.22, eye_h)),
                Mood::Curious => Some((eye_w * 0.4, 0., eye_h * 1.05)),
                Mood::Focused => Some((0., eye_h * 0.12, eye_h * 0.55)),
                Mood::Checking => Some((0., eye_h * 0.18, eye_h * 0.8)),
                Mood::Happy => None,
            };
            match eyes {
                Some((dx, dy, height)) => {
                    for side in [-1., 1.] {
                        let mut eye = PathBuilder::fill();
                        // A concerned face: one eye a little larger.
                        let height = if seam.mood == Mood::Concerned && side > 0. {
                            height * 1.15
                        } else {
                            height
                        };
                        eye.add_polygon(
                            &pill(
                                plate_center.0 + side * eye_dx + dx,
                                eye_y + dy,
                                eye_w,
                                height,
                            ),
                            true,
                        );
                        if let Ok(path) = eye.build() {
                            window.paint_path(path, mint);
                        }
                    }
                }
                None => {
                    for side in [-1., 1.] {
                        let x = plate_center.0 + side * eye_dx;
                        let mut arc = round_stroke(2.6);
                        arc.move_to(point(px(x - eye_w * 0.7), px(eye_y + eye_h * 0.12)));
                        arc.curve_to(
                            point(px(x + eye_w * 0.7), px(eye_y + eye_h * 0.12)),
                            point(px(x), px(eye_y - eye_h * 0.45)),
                        );
                        if let Ok(path) = arc.build() {
                            window.paint_path(path, mint);
                        }
                    }
                }
            }
            // The mouth: a smile at rest and when happy, a flat line
            // at work, a small curve up for a question, a frown when sad.
            let lift = match seam.mood {
                Mood::Ready | Mood::Curious => plate_h * 0.1,
                Mood::Happy => plate_h * 0.16,
                Mood::Thinking | Mood::Focused | Mood::Checking => 0.,
                Mood::Concerned => -plate_h * 0.04,
                Mood::Sad => -plate_h * 0.12,
            };
            let mut mouth = round_stroke(2.4);
            mouth.move_to(point(px(plate_center.0 - mouth_w), px(mouth_y)));
            mouth.curve_to(
                point(px(plate_center.0 + mouth_w), px(mouth_y)),
                point(px(plate_center.0), px(mouth_y + lift * 2.)),
            );
            if let Ok(path) = mouth.build() {
                window.paint_path(path, mint);
            }
        },
    )
    .size(px(size))
}

fn rounded_rect(
    center_x: f32,
    center_y: f32,
    width: f32,
    height: f32,
    radius: f32,
) -> Vec<gpui_kit::Point<Pixels>> {
    let radius = radius.min(width / 2.).min(height / 2.);
    let half_w = width / 2. - radius;
    let half_h = height / 2. - radius;
    let mut points = Vec::new();
    for (ox, oy, from) in [
        (center_x + half_w, center_y + half_h, 0.),
        (center_x - half_w, center_y + half_h, 90.),
        (center_x - half_w, center_y - half_h, 180.),
        (center_x + half_w, center_y - half_h, 270.),
    ] {
        for step in 0..=6 {
            let angle = (from + step as f32 * 15.0f32).to_radians();
            points.push(point(
                px(ox + radius * angle.cos()),
                px(oy + radius * angle.sin()),
            ));
        }
    }
    points
}

impl ChatView {
    /// A call's kind, memoized by call id: the arguments never change
    /// once a card exists, and the panes ask every frame.
    pub(crate) fn kind_of_call(&self, call: &ToolCall) -> Kind {
        if let Some(kind) = self.call_kinds.borrow().get(&call.id) {
            return *kind;
        }
        let kind = shape::kind_of(&call.name, &call.arguments, self.verify.as_deref());
        self.call_kinds.borrow_mut().insert(call.id.clone(), kind);
        kind
    }

    /// The span of rows a turn covers: after the user message at or
    /// before `index`, up to the next user message.
    pub(crate) fn span_around(&self, index: usize) -> (usize, usize) {
        let is_user = |turn: &Turn| {
            matches!(
                turn,
                Turn::Message {
                    role: Role::User,
                    ..
                }
            )
        };
        let start = self.turns[..index.min(self.turns.len())]
            .iter()
            .rposition(is_user)
            .map(|at| at + 1)
            .unwrap_or(0);
        let end = self.turns[start..]
            .iter()
            .position(is_user)
            .map(|at| start + at)
            .unwrap_or(self.turns.len());
        (start, end)
    }

    pub(crate) fn shape_of(&self, start: usize, end: usize) -> Shape {
        // A call waiting on the human is work whatever its kind (a
        // fetch's permission, a worker's question): it needs a card
        // and a mark, or the decision has nowhere to live.
        // Asks and memory proposals are answered in the words.
        let mut numbering = shape::Numbering::default();
        let items = self.turns[start..end]
            .iter()
            .enumerate()
            .filter_map(|(offset, turn)| match turn {
                Turn::Tool { call, status, .. } => {
                    let kind = self.kind_of_call(call);
                    let pending = matches!(status, ToolStatus::Pending(_))
                        && !matches!(kind, Kind::Ask | Kind::Memory);
                    let number = (kind.numbered() || pending)
                        .then(|| numbering.next(kind, shape::path_of(&call.arguments).as_deref()));
                    Some(shape::Item {
                        index: start + offset,
                        kind,
                        number,
                    })
                }
                _ => None,
            })
            .collect();
        Shape { items }
    }

    /// The thread marks an assistant paragraph carries: the numbered
    /// calls between it and the next message, each with whether it is
    /// still live (running, or waiting on the human).
    pub(crate) fn marks_after(&self, index: usize) -> Vec<(usize, bool)> {
        let (start, end) = self.span_around(index);
        let next_message = self.turns[index + 1..end.max(index + 1)]
            .iter()
            .position(|turn| matches!(turn, Turn::Message { .. }))
            .map(|at| index + 1 + at)
            .unwrap_or(end);
        let shape = self.shape_of(start, end);
        self.merge_marks(
            shape
                .items
                .iter()
                .filter(|item| (index + 1..next_message).contains(&item.index)),
        )
    }

    /// Calls folded to their marks, in first-seen order: a mark is live
    /// while any of its calls runs or waits on the human.
    fn merge_marks<'a>(&self, items: impl Iterator<Item = &'a shape::Item>) -> Vec<(usize, bool)> {
        let mut marks: Vec<(usize, bool)> = Vec::new();
        for item in items {
            let Some(number) = item.number else { continue };
            let live = matches!(
                &self.turns[item.index],
                Turn::Tool {
                    status: ToolStatus::Running | ToolStatus::Pending(_),
                    ..
                }
            );
            match marks.iter_mut().find(|(seen, _)| *seen == number) {
                Some((_, was_live)) => *was_live |= live,
                None => marks.push((number, live)),
            }
        }
        marks
    }

    /// The thread mark of the call at `index`, within its turn.
    pub(crate) fn mark_of(&self, index: usize) -> Option<usize> {
        let (start, end) = self.span_around(index);
        self.shape_of(start, end).number_at(index)
    }

    /// The words side of an approval: one vermilion line pointing at
    /// the card on the work side, where its diff and buttons are.
    pub(crate) fn render_waiting_pointer(&self, index: usize, cx: &Context<Self>) -> AnyElement {
        let pal = ink::palette(cx);
        let what = match &self.turns[index] {
            Turn::Tool { call, .. } if call.name == "bash" => "wants to run a command",
            _ => "wants to change a file",
        };
        h_flex()
            .gap_2()
            .items_center()
            .children(
                self.mark_of(index)
                    .map(|number| live_mark(number, true, false, pal, "pointer")),
            )
            .child(
                div()
                    .font_family(ink::SERIF)
                    .text_size(px(16.))
                    .text_color(pal.vermilion)
                    .child(format!("Gertie {what}. Your OK is on the work side.")),
            )
            .into_any_element()
    }

    /// The seal notes for a memory consult (#54): each entry the call
    /// returned that the session relies on, first time it shows up,
    /// with the evidence flag and its two answers when a cited file
    /// moved.
    pub(crate) fn render_memory_seals(
        &self,
        index: usize,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let pal = ink::palette(cx);
        let ids_of = |turn: &Turn| match turn {
            Turn::Tool {
                status: ToolStatus::Done(result),
                ..
            } => status::memory_hit_ids(result),
            _ => Vec::new(),
        };
        let ids = ids_of(&self.turns[index]);
        if ids.is_empty() {
            return None;
        }
        let seen_before: std::collections::HashSet<String> = self.turns[..index]
            .iter()
            .filter(|turn| match turn {
                Turn::Tool { call, .. } => self.kind_of_call(call) == Kind::Memory,
                _ => false,
            })
            .flat_map(ids_of)
            .collect();
        let rows = self.ground_rows();
        let fresh: Vec<status::GroundRow> = ids
            .iter()
            .filter(|id| !seen_before.contains(*id))
            .filter_map(|id| rows.iter().find(|row| &row.id == id).cloned())
            .collect();
        if fresh.is_empty() {
            return None;
        }
        const SHOWN: usize = 3;
        let more = fresh.len().saturating_sub(SHOWN);
        Some(
        v_flex()
            .gap_4()
            .pb_1()
            .children(fresh.into_iter().take(SHOWN).map(|row| {
                let id = row.id.clone();
                let flagged = !row.changed.is_empty();
                let revise_id = id.clone();
                h_flex()
                    .items_start()
                    .gap_3p5()
                    .child(
                        div()
                            .flex_shrink_0()
                            .min_w(px(34.))
                            .h(px(34.))
                            .px_1()
                            .rounded(px(6.))
                            .bg(pal.seal)
                            .text_color(pal.seal_text)
                            .font_family(ink::MONO)
                            .text_size(px(11.5))
                            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(id.clone()),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_1()
                            .child(
                                div()
                                    .font_family(ink::SERIF)
                                    .text_size(px(13.5))
                                    .text_color(pal.text_muted)
                                    .child("remembered, and relied on"),
                            )
                            .child(
                                div()
                                    .font_family(ink::SERIF)
                                    .text_size(px(14.5))
                                    .line_height(relative(1.55))
                                    .text_color(pal.text)
                                    .child(row.text.clone()),
                            )
                            .when(flagged, |note| {
                                note.child(
                                    h_flex()
                                        .flex_wrap()
                                        .items_center()
                                        .gap_2()
                                        .pt_1()
                                        .child(
                                            div()
                                                .font_family(ink::SANS)
                                                .text_size(px(12.))
                                                .text_color(pal.vermilion)
                                                .child("A file it cites changed. Still true?"),
                                        )
                                        .child(
                                            Button::new(SharedString::from(format!("seal-true-{index}-{id}")))
                                                .label("still true")
                                                .xsmall()
                                                .outline()
                                                .tooltip("Re-record its evidence: the entry holds as written")
                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                    if let Some(store) = this.memory_store.as_ref() {
                                                        let _ = store.reverify_evidence(&id);
                                                    }
                                                    this.invalidate_grounding();
                                                    cx.notify();
                                                })),
                                        )
                                        .child(
                                            Button::new(SharedString::from(format!(
                                                "seal-revise-{index}-{revise_id}"
                                            )))
                                            .label("revise")
                                            .xsmall()
                                            .outline()
                                            .tooltip("Open it in the memory panel")
                                            .on_click(cx.listener(|this, _, _, cx| {
                                                if this.panel != Panel::Memory {
                                                    this.toggle_memory(cx);
                                                }
                                            })),
                                        ),
                                )
                            }),
                    )
            }))
            .when(more > 0, |seals| {
                seals.child(
                    div()
                        .pl(px(48.))
                        .font_family(ink::SANS)
                        .text_size(px(12.))
                        .text_color(pal.text_muted)
                        .child(format!("and {more} more remembered")),
                )
            })
            .into_any_element(),
        )
    }

    /// The settle's memory proposal as a seal in the words (#54): the
    /// outlined "new" seal, each proposed rule in serif, and keep / edit
    /// / let it go (the inbox's accept, accept then open the panel to
    /// edit, reject). The ▲/▼ votes stay the panel's; these never
    /// touch them.
    pub(crate) fn render_proposal_seal(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let pal = ink::palette(cx);
        let Some(Turn::Tool { call, status, .. }) = self.turns.get(index) else {
            return div().into_any_element();
        };
        let lines = proposal_lines(&call.arguments);
        let pending = matches!(status, ToolStatus::Pending(_));
        let outcome = match status {
            ToolStatus::Done(result) if result == PROPOSAL_KEPT => Some("kept"),
            ToolStatus::Done(result) if result == PROPOSAL_LET_GO => Some("let go"),
            _ => None,
        };
        let flagged = call.arguments.contains("untrusted web content");
        let caption = match (pending, outcome) {
            (true, _) if flagged => {
                "worth remembering? (this session read the web; read closely)".to_string()
            }
            (true, _) => "worth remembering?".to_string(),
            (false, Some(outcome)) => format!("worth remembering, {outcome}"),
            (false, None) => "worth remembering".to_string(),
        };
        let faded = !pending;
        h_flex()
            .id(SharedString::from(format!("proposal-{index}")))
            .test_support()
            .items_start()
            .gap_3p5()
            .when(faded, |row| row.opacity(0.7))
            .child(
                div()
                    .flex_shrink_0()
                    .min_w(px(34.))
                    .h(px(34.))
                    .px_1()
                    .rounded(px(6.))
                    .border_1()
                    .border_color(if pending { pal.seal } else { pal.text_muted })
                    .text_color(if pending { pal.vermilion } else { pal.text_muted })
                    .font_family(ink::SERIF)
                    .text_size(px(12.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child("new"),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_1p5()
                    .child(
                        div()
                            .font_family(ink::SERIF)
                            .text_size(px(13.5))
                            .text_color(pal.text_muted)
                            .child(caption),
                    )
                    .children(lines.into_iter().take(6).map(|line| {
                        h_flex()
                            .items_baseline()
                            .gap_2()
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .font_family(ink::SANS)
                                    .text_size(px(11.5))
                                    .text_color(pal.text_muted)
                                    .child(line.kind),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .font_family(ink::SERIF)
                                    .text_size(px(14.5))
                                    .line_height(relative(1.55))
                                    .text_color(pal.text)
                                    .child(line.text),
                            )
                    }))
                    .when(pending, |note| {
                        note.child(
                            h_flex()
                                .gap_2()
                                .pt_1()
                                .child(
                                    Button::new(SharedString::from(format!("keep-{index}")))
                                        .label("keep")
                                        .xsmall()
                                        .primary()
                                        .tooltip("Remember these")
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.decide_proposal(index, ProposalAnswer::Keep, cx)
                                        })),
                                )
                                .child(
                                    Button::new(SharedString::from(format!("edit-memory-{index}")))
                                        .label("edit")
                                        .xsmall()
                                        .outline()
                                        .tooltip("Keep them and open the memory panel to change the words")
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.decide_proposal(index, ProposalAnswer::Edit, cx)
                                        })),
                                )
                                .child(
                                    Button::new(SharedString::from(format!("let-go-{index}")))
                                        .label("let it go")
                                        .xsmall()
                                        .ghost()
                                        .tooltip("Remember none of it")
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.decide_proposal(index, ProposalAnswer::LetGo, cx)
                                        })),
                                ),
                        )
                    }),
            )
            .into_any_element()
    }

    /// A proposal seal's answer, through the card's own decision path.
    pub(crate) fn decide_proposal(
        &mut self,
        index: usize,
        answer: ProposalAnswer,
        cx: &mut Context<Self>,
    ) {
        let decision = match answer {
            ProposalAnswer::Keep | ProposalAnswer::Edit => PermissionDecision::Approve,
            ProposalAnswer::LetGo => PermissionDecision::Deny { feedback: None },
        };
        self.decide_tool(index, decision, cx);
        if answer == ProposalAnswer::Edit && self.panel != Panel::Memory {
            self.toggle_memory(cx);
        }
        cx.notify();
    }

    /// The span the panes describe: the live call's turn while one is
    /// running or waiting (a message queued mid-turn adds a user row
    /// after it, which must not orphan the work), else everything
    /// after the last user message.
    fn latest_span(&self) -> (usize, usize) {
        let live = self.turns.iter().rposition(|turn| {
            matches!(
                turn,
                Turn::Tool {
                    status: ToolStatus::Running | ToolStatus::Pending(_),
                    ..
                }
            )
        });
        match live {
            Some(index) if self.streaming => self.span_around(index),
            _ => self.span_around(self.turns.len()),
        }
    }

    /// What the work pane describes, if it shows at all: the running
    /// turn; else the latest turn when it did work; else, opened by
    /// hand from the strip, the most recent turn that did.
    pub(crate) fn work_view(&self) -> Option<WorkView> {
        let (start, end) = self.latest_span();
        let shape = self.shape_of(start, end);
        if self.streaming || !shape.chat_only() {
            return Some(WorkView {
                start,
                end,
                running: self.streaming,
                shape,
            });
        }
        if !self.work_open {
            return None;
        }
        let mut cursor = start;
        while cursor > 0 {
            let (earlier_start, earlier_end) = self.span_around(cursor.saturating_sub(2));
            let earlier = self.shape_of(earlier_start, earlier_end);
            if !earlier.chat_only() {
                return Some(WorkView {
                    start: earlier_start,
                    end: earlier_end,
                    running: false,
                    shape: earlier,
                });
            }
            if earlier_start == 0 {
                break;
            }
            cursor = earlier_start;
        }
        None
    }

    /// How wide the words pane is, given the window: the full 620 on a
    /// wide window, yielding toward 480 as it narrows; `None` when the
    /// work side is folded and the words take the room.
    pub(crate) fn words_width(&self, window: &Window, work_shown: bool) -> Option<f32> {
        if !work_shown {
            return None;
        }
        let viewport: f32 = window.viewport_size().width.into();
        let sidebar = if self.sidebar_collapsed {
            super::sidebar::RAIL_WIDTH
        } else {
            240.
        };
        let avail = viewport - sidebar;
        Some((avail * 0.46).clamp(WORDS_MIN, WORDS_WIDTH))
    }

    /// The work overlay's width in a narrow window: most of the room
    /// beside the strip, never wider than a full words pane.
    pub(crate) fn overlay_width(&self, window: &Window) -> f32 {
        let viewport: f32 = window.viewport_size().width.into();
        let sidebar = if self.sidebar_collapsed {
            super::sidebar::RAIL_WIDTH
        } else {
            240.
        };
        (viewport - sidebar - STRIP_WIDTH - 24.).clamp(320., WORDS_WIDTH)
    }

    /// Whether the window is too narrow for both panes.
    pub(crate) fn narrow(&self, window: &Window) -> bool {
        let viewport: f32 = window.viewport_size().width.into();
        viewport < FOLD_BELOW
    }

    /// The seam's face, ring and caption, from the latest turn.
    pub(crate) fn seam_state(&self) -> SeamState {
        let (start, end) = self.latest_span();
        let shape = self.shape_of(start, end);
        let steps = shape
            .items
            .iter()
            .filter(|item| item.kind != Kind::Quiet)
            .count();
        let clock_words = {
            let elapsed = self.run_elapsed_total();
            let clock = clock(elapsed);
            let clock = if elapsed.is_zero() {
                String::new()
            } else {
                clock
            };
            match (clock.is_empty(), steps) {
                (true, 0) => String::new(),
                (true, steps) => format!("{steps} step{}", if steps == 1 { "" } else { "s" }),
                (false, 0) => clock,
                (false, steps) => format!(
                    "{clock} · {steps} step{}",
                    if steps == 1 { "" } else { "s" }
                ),
            }
        };
        if self.turns.is_empty() {
            return SeamState {
                mood: Mood::Ready,
                fill: 0.,
                live: false,
                verb: "ready",
                detail: String::new(),
            };
        }
        if self.streaming {
            if self.waiting_on_human() {
                return SeamState {
                    mood: Mood::Concerned,
                    fill: shape.stage().fill().max(0.1),
                    live: true,
                    verb: "waiting on you",
                    detail: clock_words,
                };
            }
            let stage = shape.stage();
            let running_kind =
                shape
                    .items
                    .iter()
                    .rev()
                    .find_map(|item| match &self.turns[item.index] {
                        Turn::Tool {
                            status: ToolStatus::Running,
                            ..
                        } => Some(item.kind),
                        _ => None,
                    });
            let mood = match running_kind.map(Kind::stage).unwrap_or(stage) {
                Stage::None => Mood::Thinking,
                Stage::Understand => Mood::Curious,
                Stage::Change => Mood::Focused,
                Stage::Verify => Mood::Checking,
            };
            // A turn with no tools yet: the ring grows with the words.
            let (fill, verb) = if shape.items.is_empty() {
                let written = self
                    .streaming_assistant
                    .and_then(|index| match self.turns.get(index) {
                        Some(Turn::Message { text, .. }) => Some(text.len()),
                        _ => None,
                    })
                    .unwrap_or(0);
                if written > 0 {
                    ((0.08 + written as f32 / 3000.).min(0.85), "answering")
                } else {
                    (0.04, "thinking")
                }
            } else {
                (stage.fill(), stage.verb())
            };
            return SeamState {
                mood,
                fill,
                live: true,
                verb,
                detail: clock_words,
            };
        }
        // Settled: happy, unless the turn ended on a failed check or
        // an error.
        let failed_check = shape
            .items
            .iter()
            .rev()
            .find(|item| item.kind == Kind::Check)
            .is_some_and(|item| {
                matches!(
                    &self.turns[item.index],
                    Turn::Tool { status: ToolStatus::Done(result), .. } if result_failed(result)
                )
            });
        let errored = self.turns[start..end].iter().rev().any(|turn| {
            matches!(turn, Turn::Message { role: Role::Assistant, text, .. } if text.contains("[error]"))
        });
        let sad = failed_check || errored || self.turn_checked == Some(false);
        SeamState {
            mood: if sad { Mood::Sad } else { Mood::Happy },
            fill: 1.,
            live: false,
            verb: if sad { "stuck" } else { "settled" },
            detail: clock_words,
        }
    }

    /// The seam's verb, for tests: the face's caption.
    pub fn seam_verb_for_test(&self) -> &'static str {
        self.seam_state().verb
    }

    /// The face on the seam, with its caption under the plate.
    fn render_face(&self, size: f32, pal: Palette) -> AnyElement {
        let seam = self.seam_state();
        let verb_color = if seam.mood == Mood::Concerned {
            pal.vermilion
        } else {
            pal.text
        };
        div()
            .id("gertie-face")
            .test_support()
            .relative()
            .size(px(size))
            .rounded_full()
            .bg(pal.disc)
            .border_4()
            .border_color(pal.ink)
            .shadow_lg()
            .child(
                div()
                    .absolute()
                    .top(px(-4.))
                    .left(px(-4.))
                    .size(px(size))
                    .child(face(size, &seam, pal))
                    .when(seam.live && seam.fill > 0.005, |layer| {
                        let (x, y) = ring_head(size, seam.fill);
                        let dot = div()
                            .absolute()
                            .left(px(x - 5.5))
                            .top(px(y - 5.5))
                            .size(px(11.))
                            .rounded_full()
                            .bg(pal.vermilion);
                        // Waiting on you pulses faster: it is asking.
                        let period = if seam.mood == Mood::Concerned {
                            900
                        } else {
                            1800
                        };
                        layer.child(pulsing(dot, "ring-head", period))
                    }),
            )
            .child(
                v_flex()
                    .absolute()
                    .left_0()
                    .right_0()
                    .top(px(size * 0.56 - 4.))
                    .items_center()
                    .child(
                        div()
                            .font_family(ink::SERIF)
                            .text_size(px(if size > 110. { 13.5 } else { 11.5 }))
                            .text_color(verb_color)
                            .child(seam.verb),
                    )
                    .when(!seam.detail.is_empty() && size > 110., |caption| {
                        caption.child(
                            div()
                                .font_family(ink::SANS)
                                .text_size(px(9.5))
                                .text_color(pal.text_muted)
                                .child(seam.detail.clone()),
                        )
                    }),
            )
            .into_any_element()
    }

    /// The seam overlay: the face centered on the panes' boundary in
    /// the header band, and under it the thread of the turn's marks.
    /// `at_left` is the boundary's offset from the panes' left edge;
    /// `None` means the work side is folded and the face rides the
    /// strip's edge instead.
    pub(crate) fn render_seam(&self, at_left: Option<f32>, cx: &Context<Self>) -> AnyElement {
        let pal = ink::palette(cx);
        let size = if at_left.is_some() {
            FACE_DISC
        } else {
            FACE_DISC_SMALL
        };
        let top = HEADER_HEIGHT / 2. - size / 2.;
        let face = div().absolute().top(px(top));
        let face = match at_left {
            Some(left) => face.left(px(left - size / 2.)),
            None => face.right(px(STRIP_WIDTH - size / 2.)),
        }
        .child(self.render_face(size, pal));
        let thread = at_left.zip(self.work_view()).and_then(|(left, view)| {
            let marks = self.merge_marks(view.shape.work());
            if marks.is_empty() {
                return None;
            }
            let color = if view.running {
                pal.vermilion
            } else {
                pal.text_past
            };
            let height = 36. + marks.len() as f32 * 32. + if view.running { 28. } else { 0. };
            Some(
                div()
                    .absolute()
                    .left(px(left - 11.))
                    .top(px(HEADER_HEIGHT + 4.))
                    .w(px(22.))
                    .h(px(height))
                    .child(
                        div()
                            .absolute()
                            .left(px(10.))
                            .top_0()
                            .w(px(2.))
                            .h_full()
                            .bg(color.opacity(0.5)),
                    )
                    .child(
                        v_flex()
                            .absolute()
                            .top(px(22.))
                            .left_0()
                            .w(px(22.))
                            .items_center()
                            .gap(px(12.))
                            .children(marks.into_iter().map(|(number, live)| {
                                if live {
                                    live_mark(number, true, false, pal, "seam")
                                } else {
                                    mark(number, false, false, pal)
                                        .bg(pal.paper)
                                        .into_any_element()
                                }
                            }))
                            .when(view.running, |thread| {
                                thread.child(
                                    div()
                                        .size(px(12.))
                                        .rounded_full()
                                        .border_1()
                                        .border_dashed()
                                        .border_color(pal.text_past)
                                        .bg(pal.paper),
                                )
                            }),
                    ),
            )
        });
        div()
            .absolute()
            .inset_0()
            .children(thread)
            .child(face)
            .into_any_element()
    }

    /// The words pane's header: where this is, what it is called, and
    /// what is happening now and next, in a sentence.
    pub(crate) fn render_words_header(&self, cx: &Context<Self>) -> AnyElement {
        let pal = ink::palette(cx);
        let title = match self.session_id {
            Some(_) => self.tab_title(),
            None => SharedString::from("New chat"),
        };
        let title = div()
            .id("conversation-title")
            .min_w_0()
            .truncate()
            .font_family(ink::SERIF)
            .text_size(px(26.))
            .line_height(relative(1.2))
            .font_weight(gpui_kit::FontWeight::SEMIBOLD)
            .text_color(pal.text)
            .child(title)
            .when_some(self.session_id, |title, session_id| {
                title
                    .cursor_pointer()
                    .tooltip(|window, cx| {
                        Tooltip::new("Rename this conversation").build(window, cx)
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.start_rename(session_id, window, cx)
                    }))
            });
        let project = self
            .project_id()
            .and_then(|id| self.projects.iter().find(|project| project.id == id))
            .map(|project| project.name.clone())
            .unwrap_or_else(|| "Other".to_string());
        let worktree = self.branch_naming.is_some();
        let folder = h_flex()
            .id("workspace-word")
            .gap_1()
            .rounded_sm()
            .when(!worktree, |folder| {
                folder
                    .cursor_pointer()
                    .hover(|style| style.text_color(pal.text))
                    .tooltip(|window, cx| {
                        Tooltip::new("Workspace, click to move the agent to another folder")
                            .build(window, cx)
                    })
                    .on_click(cx.listener(|this, _, _, cx| this.switch_workspace(cx)))
            })
            .child(
                Icon::new(if worktree {
                    IconName::GitBranch
                } else {
                    IconName::Folder
                })
                .with_size(px(12.)),
            )
            .child(tildify(&self.workspace_root))
            .test_support();
        let breadcrumb = h_flex()
            .gap_2()
            .font_family(ink::SANS)
            .text_size(px(12.))
            .text_color(pal.text_muted)
            .child(project)
            .child("·")
            .child(folder);
        // Now and next: the model's own words when it gave them (the
        // status tool), else what the turn is doing.
        let (now, then) = self.now_and_next();
        let now = now.unwrap_or_default();
        let bright = 0..now.len();
        let joined = match then {
            Some(then) if !now.is_empty() => format!("{now} {then}"),
            Some(then) => then,
            None => now,
        };
        let sentence = div()
            .w_full()
            .font_family(ink::SERIF)
            .text_size(px(15.))
            .line_height(relative(1.5))
            .text_color(pal.text_muted)
            .line_clamp(2)
            .child(gpui_kit::StyledText::new(joined).with_highlights([(
                bright,
                gpui_kit::HighlightStyle {
                    color: Some(pal.text),
                    ..Default::default()
                },
            )]));
        v_flex()
            .w_full()
            .flex_shrink_0()
            .h(px(HEADER_HEIGHT))
            .justify_center()
            .gap_2()
            .pl(px(48.))
            .pr(px(FACE_DISC / 2. + 24.))
            .border_b_1()
            .border_color(pal.line)
            .child(breadcrumb)
            .child(title)
            .child(sentence)
            .into_any_element()
    }

    /// The header's sentence halves.
    fn now_and_next(&self) -> (Option<String>, Option<String>) {
        let clip = |text: &str| {
            let line = text.lines().next().unwrap_or_default().trim();
            if line.chars().count() > 90 {
                format!("{}…", line.chars().take(90).collect::<String>().trim_end())
            } else {
                line.to_string()
            }
        };
        if self.turns.is_empty() {
            return (None, None);
        }
        let then = self
            .status_facts
            .next
            .as_deref()
            .map(|next| format!("Then: {}", clip(next)));
        if self.streaming {
            let now = if self.waiting_on_human() {
                "Now: waiting on you.".to_string()
            } else if let Some(goal) = self.status_facts.goal.as_deref() {
                format!("Now: {}", clip(goal))
            } else {
                format!("Now: {}.", self.seam_state().verb)
            };
            (Some(now), then)
        } else {
            let done = if self.seam_state().mood == Mood::Sad {
                "Stopped short."
            } else {
                "Done."
            };
            (
                Some(done.to_string()),
                self.status_facts
                    .next
                    .as_deref()
                    .map(|next| format!("Next, if you want it: {}", clip(next))),
            )
        }
    }

    /// The work pane: what this turn changed and ran, numbered to
    /// match the words, with the checkpoints underneath.
    pub(crate) fn render_work_pane(
        &mut self,
        view: WorkView,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let pal = ink::palette(cx);
        // The touched files, folded the column's way over this span.
        let mut facts = status::StatusFacts::default();
        for turn in &self.turns[view.start..view.end] {
            if let Turn::Tool {
                call,
                status: ToolStatus::Done(result),
                ..
            } = turn
            {
                facts.fold_tool(&call.name, &call.arguments, result, &self.workspace_root);
            }
        }
        let changed = facts
            .touched
            .iter()
            .filter(|(_, touch)| *touch == status::Touch::Edited)
            .count();
        let read = facts.touched.len() - changed;
        let check = view
            .shape
            .items
            .iter()
            .rev()
            .find(|item| item.kind == Kind::Check)
            .and_then(|item| match &self.turns[item.index] {
                Turn::Tool {
                    status: ToolStatus::Done(result),
                    ..
                } => Some(!result_failed(result)),
                _ => None,
            });
        let plural = |count: usize, one: &str, many: &str| {
            if count == 1 {
                format!("1 {one}")
            } else {
                format!("{count} {many}")
            }
        };
        let header_line = if view.running {
            div()
                .font_family(ink::SERIF)
                .text_size(px(16.))
                .text_color(pal.ink_muted)
                .child("the work")
                .into_any_element()
        } else {
            let mut parts = Vec::new();
            if changed > 0 {
                parts.push(format!("{} changed", plural(changed, "file", "files")));
            }
            match check {
                Some(true) => parts.push("the check passed".to_string()),
                Some(false) => parts.push("the check failed".to_string()),
                None => {}
            }
            if parts.is_empty() {
                parts.push(format!(
                    "{} ran",
                    plural(view.shape.work().count(), "step", "steps")
                ));
            }
            let mut sentence = parts.join(", ");
            if let Some(first) = sentence.get(..1) {
                sentence = format!("{}{}.", first.to_uppercase(), &sentence[1..]);
            }
            v_flex()
                .gap_1()
                .child(
                    div()
                        .font_family(ink::SERIF)
                        .text_size(px(14.))
                        .text_color(pal.ink_muted)
                        .child("what this turn left behind"),
                )
                .child(
                    div()
                        .font_family(ink::SERIF)
                        .text_size(px(19.))
                        .line_height(relative(1.35))
                        .text_color(pal.ink_text)
                        .child(sentence),
                )
                .into_any_element()
        };
        let counts = format!("{} changed · {} read", changed, read);
        let learned = self.learned_line(view.start, view.end, pal, cx);
        let chips = h_flex()
            .flex_wrap()
            .gap_2()
            .children(
                facts
                    .touched
                    .iter()
                    .filter(|(path, _)| path.chars().any(char::is_alphanumeric))
                    .take(10)
                    .map(|(path, touch)| {
                        let edited = *touch == status::Touch::Edited;
                        let short = path.rsplit('/').take(2).collect::<Vec<_>>();
                        let short = short.into_iter().rev().collect::<Vec<_>>().join("/");
                        h_flex()
                            .gap_1()
                            .px_2p5()
                            .py(px(4.))
                            .rounded_full()
                            .font_family(ink::MONO)
                            .text_size(px(11.5))
                            .when(edited, |chip| {
                                chip.bg(pal.ink_raised).text_color(pal.ink_text)
                            })
                            .when(!edited, |chip| {
                                chip.border_1()
                                    .border_color(pal.ink_line)
                                    .text_color(pal.ink_muted)
                            })
                            .child(short)
                            .when(edited, |chip| {
                                chip.child(div().text_color(pal.added).child("changed"))
                            })
                    }),
            )
            .when(facts.touched.len() > 10, |chips| {
                chips.child(
                    div()
                        .font_family(ink::SANS)
                        .text_size(px(11.5))
                        .text_color(pal.ink_muted)
                        .child(format!("and {} more", facts.touched.len() - 10)),
                )
            });
        let header = v_flex()
            .flex_shrink_0()
            .h(px(HEADER_HEIGHT))
            .justify_center()
            .gap_3()
            .pl(px(FACE_DISC / 2. + 28.))
            .pr(px(36.))
            .border_b_1()
            .border_color(pal.ink_line)
            .child(
                h_flex()
                    .items_end()
                    .gap_3()
                    .child(div().flex_1().min_w_0().child(header_line))
                    .when(view.running, |row| {
                        row.child(
                            div()
                                .font_family(ink::SANS)
                                .text_size(px(12.))
                                .text_color(pal.ink_muted)
                                .child(counts.clone()),
                        )
                    })
                    .children(learned),
            )
            .child(chips);

        // The cards, in order. Looking (reads, searches) lives in the
        // chips above; asks live in the words.
        let mut cards: Vec<AnyElement> = Vec::new();
        let last_numbered = view.shape.work().last().map(|item| item.index);
        // What waits on the human comes first: the decision is never
        // below the scroll.
        let mut ordered = view.shape.items.clone();
        ordered.sort_by_key(|item| {
            !matches!(
                self.turns.get(item.index),
                Some(Turn::Tool {
                    status: ToolStatus::Pending(_),
                    ..
                })
            )
        });
        let mut carded: Vec<usize> = Vec::new();
        for item in ordered {
            let Some(number) = item.number else { continue };
            let pending_item = matches!(
                self.turns.get(item.index),
                Some(Turn::Tool {
                    status: ToolStatus::Pending(_),
                    ..
                })
            );
            // One card per mark: a file's later edits join its card.
            if !pending_item {
                if carded.contains(&number) {
                    continue;
                }
                carded.push(number);
            }
            let Some(Turn::Tool {
                call,
                status,
                worker,
                auto_approved,
                ..
            }) = self.turns.get(item.index)
            else {
                continue;
            };
            let call = call.clone();

            let (worker, auto_approved) = (*worker, *auto_approved);
            let card = match (&status, item.kind) {
                (ToolStatus::Pending(_), _) => {
                    // Waiting on the human: the full card, its diff and
                    // its buttons, edged in vermilion.
                    let face = self.render_tool(
                        item.index,
                        &call,
                        status,
                        auto_approved,
                        worker,
                        window,
                        cx,
                    );
                    v_flex()
                        .flex_shrink_0()
                        .gap_2()
                        .p_4()
                        .rounded(px(14.))
                        .border_1()
                        .border_color(pal.vermilion)
                        .bg(pal.ink_raised)
                        .child(
                            h_flex()
                                .gap_2()
                                .items_center()
                                .child(live_mark(number, true, true, pal, "pending"))
                                .child(
                                    div()
                                        .font_family(ink::SANS)
                                        .text_size(px(12.5))
                                        .text_color(pal.vermilion)
                                        .child("waiting on you"),
                                ),
                        )
                        .child(face)
                        .into_any_element()
                }
                (_, Kind::Edit) => {
                    let group: Vec<usize> = view
                        .shape
                        .group(number)
                        .iter()
                        .map(|item| item.index)
                        .filter(|index| {
                            !matches!(
                                self.turns.get(*index),
                                Some(Turn::Tool {
                                    status: ToolStatus::Pending(_),
                                    ..
                                })
                            )
                        })
                        .collect();
                    let open =
                        view.running && last_numbered.is_some_and(|last| group.contains(&last));
                    self.render_edit_card(&group, number, open, pal, cx)
                }
                (_, Kind::Check | Kind::Run) => {
                    self.render_run_card(item.index, number, item.kind, &call, status, pal)
                }
                _ => self.render_step_card(number, &call, status, pal),
            };
            cards.push(card);
        }
        let actions = (!view.running).then(|| {
            let review_index = view.shape.work().last().map(|item| item.index);
            let has_commit = self
                .templates
                .iter()
                .any(|template| template.name == "commit");
            h_flex()
                .gap_2()
                .flex_wrap()
                .mt_2()
                .when(has_commit && changed > 0, |row| {
                    row.child(
                        Button::new("work-commit")
                            .label("Commit these changes")
                            .small()
                            .primary()
                            .tooltip("Put the commit command in the composer, ready to send")
                            .on_click(cx.listener(|this, _, window, cx| {
                                if let Some(template) = this
                                    .templates
                                    .iter()
                                    .find(|template| template.name == "commit")
                                {
                                    let text = template.render("");
                                    this.accept_slash_text(false, text, window, cx);
                                }
                            })),
                    )
                })
                .when_some(
                    review_index.filter(|_| self.snapshots.is_some()),
                    |row, index| {
                        row.child(
                            Button::new("work-review")
                                .label("Review the whole diff")
                                .small()
                                .outline()
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.open_turn_diff(index, window, cx)
                                })),
                        )
                    },
                )
        });
        let body = v_flex()
            .id("work-body")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.work_scroll)
            .pt_5()
            .pb_4()
            .pl(px(FACE_DISC / 2. + 28.))
            .pr(px(36.))
            .gap_4()
            .children(cards)
            .children(actions);
        v_flex()
            .id("work-pane")
            .test_support()
            .flex_1()
            .min_w_0()
            .h_full()
            .bg(pal.ink)
            .text_color(pal.ink_text)
            .child(header)
            .child(body)
            .child(self.render_checkpoints(pal, cx))
            .into_any_element()
    }

    /// An edit card, one per file: the path, its density bar and
    /// counts over every edit the turn made to it, a sentence of why,
    /// an overview strip of where in the file the changes sit, and the
    /// diffs (open while it is the live item, or when the reader
    /// opened it).
    fn render_edit_card(
        &self,
        indices: &[usize],
        number: usize,
        open_by_default: bool,
        pal: Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(&first) = indices.first() else {
            return div().into_any_element();
        };
        let calls: Vec<(usize, ToolCall, Option<String>, bool)> = indices
            .iter()
            .filter_map(|index| match self.turns.get(*index) {
                Some(Turn::Tool { call, status, .. }) => Some((
                    *index,
                    call.clone(),
                    match status {
                        ToolStatus::Done(result) => Some(result.clone()),
                        _ => None,
                    },
                    matches!(status, ToolStatus::Running),
                )),
                _ => None,
            })
            .collect();
        let Some((_, first_call, _, _)) = calls.first() else {
            return div().into_any_element();
        };
        let path = shape::path_of(&first_call.arguments).unwrap_or_else(|| first_call.name.clone());
        let path = status::normalize_path(&self.workspace_root, &path).unwrap_or(path);
        let patches: Vec<crate::diff::Patch> = calls
            .iter()
            .filter(|(_, _, result, _)| !result.as_deref().is_some_and(result_failed))
            .filter_map(|(index, call, _, _)| self.patch_for(*index, call))
            .collect();
        let adds: usize = patches.iter().map(|patch| patch.adds).sum();
        let removes: usize = patches.iter().map(|patch| patch.removes).sum();
        let failed = calls
            .iter()
            .all(|(_, _, result, _)| result.as_deref().is_some_and(result_failed));
        let live = calls.iter().any(|(_, _, _, running)| *running);
        let open = open_by_default != self.expanded_tools.contains(&first);
        let why = self.why_for(first);
        let spans: Vec<(f32, f32)> = calls
            .iter()
            .flat_map(|(index, call, _, _)| self.change_spans(*index, call))
            .collect();
        let edits = calls.len();
        let head = h_flex()
            .gap_3()
            .items_center()
            .child(live_mark(number, live, true, pal, "edit"))
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .font_family(ink::MONO)
                    .text_size(px(13.))
                    .child(path),
            )
            .when(!patches.is_empty(), |row| {
                row.child(density_bar(adds, removes, pal)).child(
                    div()
                        .font_family(ink::SANS)
                        .text_size(px(12.))
                        .text_color(pal.ink_muted)
                        .child(format!("+{adds} \u{2212}{removes}")),
                )
            })
            .when(edits > 1, |row| {
                row.child(
                    div()
                        .font_family(ink::SANS)
                        .text_size(px(12.))
                        .text_color(pal.ink_muted)
                        .child(format!("{edits} edits")),
                )
            })
            .child(div().flex_1())
            .when(failed, |row| {
                row.child(
                    div()
                        .font_family(ink::SANS)
                        .text_size(px(12.))
                        .text_color(pal.vermilion)
                        .child("did not apply"),
                )
            })
            .when(!patches.is_empty() && !failed, |row| {
                row.child(
                    div()
                        .id(SharedString::from(format!("edit-toggle-{first}")))
                        .cursor_pointer()
                        .font_family(ink::SANS)
                        .text_size(px(12.5))
                        .text_color(pal.added)
                        .hover(|style| style.text_color(pal.ink_text))
                        .child(if open { "hide" } else { "see the change" })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if !this.expanded_tools.remove(&first) {
                                this.expanded_tools.insert(first);
                            }
                            cx.notify();
                        })),
                )
            });
        // Where in the file: a thin track the file's length, a block
        // per change at its place.
        let overview = (!spans.is_empty()).then(|| {
            div()
                .id(SharedString::from(format!("edit-overview-{first}")))
                .test_support()
                .relative()
                .w_full()
                .h(px(4.))
                .rounded_full()
                .bg(pal.ink_line)
                .children(spans.iter().map(|(from, to)| {
                    div()
                        .absolute()
                        .top_0()
                        .h_full()
                        .rounded_full()
                        .left(relative(from.clamp(0., 0.99)))
                        .w(relative((to - from).max(0.012)))
                        .bg(pal.added)
                }))
        });
        let diffs = patches;
        v_flex()
            .id(SharedString::from(format!("edit-card-{first}")))
            .flex_shrink_0()
            .test_support()
            .rounded(px(14.))
            .border_1()
            .border_color(if live {
                pal.seal.opacity(0.7)
            } else {
                pal.ink_line
            })
            .bg(pal.ink_raised)
            .overflow_hidden()
            .child(
                v_flex()
                    .gap_2()
                    .px_4()
                    .py_3p5()
                    .child(head)
                    .children(why.map(|why| {
                        div()
                            .font_family(ink::SERIF)
                            .text_size(px(15.))
                            .line_height(relative(1.5))
                            .text_color(pal.ink_body)
                            .child(why)
                    }))
                    .children(overview),
            )
            .when(open && !failed, |card| {
                card.children(diffs.iter().map(|patch| {
                    div()
                        .border_t_1()
                        .border_color(pal.ink_line)
                        .px_3()
                        .py_2()
                        .child(diff_element(patch, cx))
                }))
            })
            .into_any_element()
    }

    /// Where a change sits in its file, as fractions of the file's
    /// length. A write's diff carries the whole file; an edit is found
    /// by its new text in the file as it is now. Memoized per row: the
    /// file read happens once.
    pub(crate) fn change_spans(&self, index: usize, call: &ToolCall) -> Vec<(f32, f32)> {
        if let Some(spans) = self.change_spans.borrow().get(&index) {
            return spans.clone();
        }
        let spans = self.compute_change_spans(index, call);
        self.change_spans.borrow_mut().insert(index, spans.clone());
        spans
    }

    fn compute_change_spans(&self, index: usize, call: &ToolCall) -> Vec<(f32, f32)> {
        if call.name == "write_file" {
            let Some(patch) = self.patch_for(index, call) else {
                return Vec::new();
            };
            let total = patch.lines.len().max(1) as f32;
            let mut spans: Vec<(f32, f32)> = Vec::new();
            for (at, line) in patch.lines.iter().enumerate() {
                if matches!(line, crate::diff::Line::Same(_)) {
                    continue;
                }
                let (from, to) = (at as f32 / total, (at + 1) as f32 / total);
                match spans.last_mut() {
                    Some(last) if from - last.1 < 0.01 => last.1 = to,
                    _ => spans.push((from, to)),
                }
            }
            return spans;
        }
        let value: serde_json::Value = match serde_json::from_str(&call.arguments) {
            Ok(value) => value,
            Err(_) => return Vec::new(),
        };
        let (Some(path), Some(new)) = (
            value.get("path").and_then(|path| path.as_str()),
            value.get("new_text").and_then(|text| text.as_str()),
        ) else {
            return Vec::new();
        };
        let full = if std::path::Path::new(path).is_absolute() {
            std::path::PathBuf::from(path)
        } else {
            self.workspace_root.join(path)
        };
        let Ok(content) = std::fs::read_to_string(full) else {
            return Vec::new();
        };
        let Some(at) = content.find(new).filter(|_| !new.is_empty()) else {
            return Vec::new();
        };
        let total = content.lines().count().max(1) as f32;
        let start = content[..at].lines().count() as f32;
        let span = new.lines().count().max(1) as f32;
        vec![(start / total, ((start + span) / total).min(1.))]
    }

    /// The work header's memory line: what the settle proposed for this
    /// turn, waiting in the words or kept (a link to the panel).
    fn learned_line(
        &self,
        start: usize,
        end: usize,
        pal: Palette,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let (count, status) = self.turns[start..end]
            .iter()
            .rev()
            .find_map(|turn| match turn {
                Turn::Tool { call, status, .. } if call.name == "memory_proposal" => {
                    Some((proposal_lines(&call.arguments).len(), status))
                }
                _ => None,
            })?;
        let word = |kept: &str| {
            if count == 1 {
                format!("1 {kept}")
            } else {
                format!("{count} {kept}")
            }
        };
        let base = |row: Div| {
            row.font_family(ink::SANS)
                .text_size(px(12.))
                .flex_shrink_0()
        };
        match status {
            ToolStatus::Pending(_) => Some(
                base(div())
                    .text_color(pal.vermilion)
                    .child(word("worth remembering, in the words"))
                    .into_any_element(),
            ),
            ToolStatus::Done(result) if result == PROPOSAL_KEPT => Some(
                base(div())
                    .id("work-learned")
                    .cursor_pointer()
                    .text_color(pal.added)
                    .hover(|style| style.text_color(pal.ink_text))
                    .child(format!("{} · review", word("learned")))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if this.panel != Panel::Memory {
                            this.toggle_memory(cx);
                        }
                    }))
                    .into_any_element(),
            ),
            _ => None,
        }
    }

    /// A shell card: the command, its clock, the last lines of its
    /// output, and for a check, whether it passed.
    fn render_run_card(
        &self,
        index: usize,
        number: usize,
        kind: Kind,
        call: &ToolCall,
        status: &ToolStatus,
        pal: Palette,
    ) -> AnyElement {
        let command = shape::command_of(&call.arguments).unwrap_or_default();
        let first_line = strip_cd(
            command.lines().next().unwrap_or_default(),
            &self.workspace_root.to_string_lossy(),
        );
        let running = matches!(status, ToolStatus::Running);
        let (verdict, tail): (Option<(bool, String)>, Vec<String>) = match status {
            ToolStatus::Done(result) => {
                let failed = result_failed(result);
                let lines: Vec<String> = result
                    .lines()
                    .filter(|line| !line.trim().is_empty())
                    .map(str::to_string)
                    .collect();
                let tail = lines[lines.len().saturating_sub(3)..].to_vec();
                let summary = crate::tools_ui::result_summary(&call.name, result);
                (Some((failed, summary)), tail)
            }
            _ => (None, Vec::new()),
        };
        let clock = self
            .tool_starts
            .get(&call.id)
            .filter(|_| running)
            .map(|started| clock(started.elapsed()))
            .unwrap_or_default();
        let edge = match (&verdict, running) {
            (_, true) => pal.seal.opacity(0.7),
            (Some((true, _)), _) => pal.vermilion.opacity(0.6),
            _ => pal.ink_line,
        };
        v_flex()
            .id(SharedString::from(format!("run-card-{index}")))
            .flex_shrink_0()
            .test_support()
            .gap_2()
            .px_4()
            .py_3p5()
            .rounded(px(14.))
            .border_1()
            .border_color(edge)
            .bg(pal.ink_raised)
            .child(
                h_flex()
                    .gap_3()
                    .items_center()
                    .child(live_mark(number, running, true, pal, "run"))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_family(ink::MONO)
                            .text_size(px(13.))
                            .child(first_line),
                    )
                    .when(!clock.is_empty(), |row| {
                        row.child(
                            div()
                                .font_family(ink::SANS)
                                .text_size(px(12.))
                                .text_color(pal.ink_muted)
                                .child(clock),
                        )
                    })
                    .when_some(verdict.clone(), |row, (failed, _)| {
                        let counts = match status {
                            ToolStatus::Done(result) if kind == Kind::Check => test_counts(result),
                            _ => None,
                        };
                        let word = match (kind, failed, counts) {
                            (Kind::Check, false, Some((passed, _))) => format!("{passed} passed"),
                            (Kind::Check, true, Some((_, failed))) if failed > 0 => {
                                format!("{failed} failed")
                            }
                            (Kind::Check, false, _) => "passed".to_string(),
                            (_, false, _) => "done".to_string(),
                            (_, true, _) => "failed".to_string(),
                        };
                        let color = if failed { pal.vermilion } else { pal.added };
                        row.child(
                            h_flex()
                                .gap_1p5()
                                .items_center()
                                .font_family(ink::SANS)
                                .text_size(px(12.5))
                                .text_color(color)
                                .when(kind == Kind::Check && !failed, |row| {
                                    row.child(
                                        div()
                                            .size(px(16.))
                                            .rounded_full()
                                            .border_1()
                                            .border_color(color)
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .child(Icon::new(IconName::Check).with_size(px(10.))),
                                    )
                                })
                                .child(word),
                        )
                    }),
            )
            .when(!tail.is_empty(), |card| {
                card.child(
                    v_flex()
                        .font_family(ink::MONO)
                        .text_size(px(12.))
                        .line_height(relative(1.6))
                        .text_color(pal.ink_body)
                        .children(tail.into_iter().map(|line| div().truncate().child(line))),
                )
            })
            .into_any_element()
    }

    /// Any other numbered step (a worker run): its label and outcome.
    fn render_step_card(
        &self,
        number: usize,
        call: &ToolCall,
        status: &ToolStatus,
        pal: Palette,
    ) -> AnyElement {
        let running = matches!(status, ToolStatus::Running);
        let label =
            crate::tools_ui::action_label(&call.name, &call.arguments, running).replace('`', "");
        let outcome = match status {
            ToolStatus::Done(result) => Some(crate::tools_ui::result_summary(&call.name, result)),
            _ => None,
        };
        v_flex()
            .flex_shrink_0()
            .gap_1()
            .px_4()
            .py_3()
            .rounded(px(14.))
            .border_1()
            .border_color(pal.ink_line)
            .bg(pal.ink_raised)
            .child(
                h_flex()
                    .gap_3()
                    .items_center()
                    .child(live_mark(number, running, true, pal, "step"))
                    .child(div().font_family(ink::SANS).text_size(px(13.)).child(label)),
            )
            .children(outcome.map(|outcome| {
                div()
                    .pl(px(32.))
                    .font_family(ink::SANS)
                    .text_size(px(12.))
                    .text_color(pal.ink_muted)
                    .child(outcome)
            }))
            .into_any_element()
    }

    /// The checkpoint timeline: one dot per question asked (each is a
    /// place the workspace can return to, through the rewind and its
    /// confirm), the present at the end.
    fn render_checkpoints(&self, pal: Palette, cx: &mut Context<Self>) -> AnyElement {
        let points = self.checkpoint_points();
        let shown = &points[points.len().saturating_sub(10)..];
        let mut row = h_flex().items_center();
        for (at, (index, question)) in shown.iter().enumerate() {
            if at > 0 {
                row = row.child(div().flex_1().h(px(2.)).bg(pal.ink_line));
            }
            let index = *index;
            let tip = format!("Return to before: {}", display_text(question));
            row = row.child(
                div()
                    .id(SharedString::from(format!("checkpoint-{index}")))
                    .size(px(28.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .tooltip(move |window, cx| Tooltip::new(tip.clone()).build(window, cx))
                    .on_click(cx.listener(move |this, _, _, cx| this.begin_rewind(index, cx)))
                    .child(
                        div()
                            .size(px(11.))
                            .rounded_full()
                            .border_2()
                            .border_color(pal.ink_text)
                            .hover(|style| style.bg(pal.ink_text)),
                    ),
            );
        }
        let now = if self.streaming {
            pal.vermilion
        } else {
            pal.ink_text
        };
        if !shown.is_empty() {
            row = row.child(div().flex_1().h(px(2.)).bg(pal.ink_line));
        }
        row = row.child(
            div()
                .size(px(28.))
                .flex()
                .items_center()
                .justify_center()
                .child(div().size(px(13.)).rounded_full().bg(now)),
        );
        v_flex()
            .flex_shrink_0()
            .gap_1()
            .pt_2()
            .pb_4()
            .pl(px(FACE_DISC / 2. + 28.))
            .pr(px(36.))
            .child(
                h_flex()
                    .gap_3()
                    .items_baseline()
                    .child(
                        div()
                            .font_family(ink::SERIF)
                            .text_size(px(14.))
                            .text_color(pal.ink_muted)
                            .child("checkpoints"),
                    )
                    .child(
                        div()
                            .font_family(ink::SANS)
                            .text_size(px(11.5))
                            .text_color(pal.ink_muted.opacity(0.8))
                            .child("click one to return the workspace to it"),
                    ),
            )
            .child(row)
            .into_any_element()
    }

    /// The places the workspace can return to: each persisted question,
    /// with its first line for the tooltip.
    fn checkpoint_points(&self) -> Vec<(usize, String)> {
        self.turns
            .iter()
            .enumerate()
            .filter_map(|(index, turn)| match turn {
                Turn::Message {
                    role: Role::User,
                    db_id: Some(_),
                    text,
                } => Some((index, text.lines().next().unwrap_or_default().to_string())),
                _ => None,
            })
            .collect()
    }

    /// The folded work side: a strip of ink with the way back open.
    pub(crate) fn render_work_strip(&self, cx: &mut Context<Self>) -> AnyElement {
        let pal = ink::palette(cx);
        let has_work = self.turns.iter().any(|turn| match turn {
            Turn::Tool { call, .. } => self.kind_of_call(call).numbered(),
            _ => false,
        });
        v_flex()
            .id("work-strip")
            .test_support()
            .w(px(STRIP_WIDTH))
            .flex_shrink_0()
            .h_full()
            .bg(pal.ink)
            .items_center()
            .pt(px(HEADER_HEIGHT - 10.))
            .gap_3()
            .child(
                div()
                    .font_family(ink::SERIF)
                    .text_size(px(12.5))
                    .text_color(pal.ink_muted)
                    .text_center()
                    .child(if has_work { "last" } else { "no" })
                    .child(div().child("work")),
            )
            .when(has_work || self.work_open, |strip| {
                strip.child(
                    Button::new("work-open")
                        .icon(if self.work_open {
                            IconName::ChevronRight
                        } else {
                            IconName::ChevronLeft
                        })
                        .small()
                        .ghost()
                        .tooltip(if self.work_open {
                            "Fold the work side"
                        } else {
                            "Open the work side on the last turn that did work"
                        })
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.work_open = !this.work_open;
                            cx.notify();
                        })),
                )
            })
            // The checkpoints, folded too: a short vertical run of the
            // latest, the present at the foot.
            .child({
                let points = self.checkpoint_points();
                let shown = points[points.len().saturating_sub(8)..].to_vec();
                let now = if self.streaming {
                    pal.vermilion
                } else {
                    pal.ink_text
                };
                v_flex()
                    .mt_4()
                    .items_center()
                    .children(shown.into_iter().map(|(index, question)| {
                        let tip = format!("Return to before: {}", display_text(&question));
                        v_flex()
                            .items_center()
                            .child(div().w(px(2.)).h(px(10.)).bg(pal.ink_line))
                            .child(
                                div()
                                    .id(SharedString::from(format!("strip-checkpoint-{index}")))
                                    .size(px(22.))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .cursor_pointer()
                                    .tooltip(move |window, cx| {
                                        Tooltip::new(tip.clone()).build(window, cx)
                                    })
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.begin_rewind(index, cx)
                                    }))
                                    .child(
                                        div()
                                            .size(px(9.))
                                            .rounded_full()
                                            .border_2()
                                            .border_color(pal.ink_muted)
                                            .hover(|style| style.bg(pal.ink_text)),
                                    ),
                            )
                    }))
                    .child(div().w(px(2.)).h(px(10.)).bg(pal.ink_line))
                    .child(div().mt_1().size(px(11.)).rounded_full().bg(now))
            })
            .into_any_element()
    }

    /// A diff patch for an edit or write call, from the card cache.
    pub(crate) fn patch_for(&self, index: usize, call: &ToolCall) -> Option<crate::diff::Patch> {
        self.tool_face_for(index, call).2
    }

    /// The sentence that announced a call: the first sentence of the
    /// nearest assistant paragraph before it in its turn.
    pub(crate) fn why_for(&self, index: usize) -> Option<String> {
        let (start, _) = self.span_around(index);
        self.turns[start..index]
            .iter()
            .rev()
            .find_map(|turn| match turn {
                Turn::Message {
                    role: Role::Assistant,
                    text,
                    ..
                } if !text.trim().is_empty() && !text.contains("[error]") => {
                    let sentence = shape::first_sentence(text);
                    (!sentence.is_empty()).then_some(sentence)
                }
                _ => None,
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_proposal_reads_as_rules_without_their_evidence_tail() {
        let arguments = serde_json::json!({
            "memory proposal": "add gotchas.md :: build in the container (evidence: scripts/build.sh)\n\
                                add correction feedback.md :: no em dashes\n\
                                update p4 :: the check is build.sh test\n\
                                supersede p2 (replaced by p9)"
        })
        .to_string();
        let lines = proposal_lines(&arguments);
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0].kind, "new");
        assert_eq!(lines[0].text, "build in the container");
        assert_eq!(lines[1].kind, "a correction");
        assert_eq!(lines[2].kind, "an update");
        assert_eq!(lines[3].kind, "tidy");
        assert!(proposal_lines("not json").is_empty());
    }

    #[test]
    fn test_counts_sum_over_the_result() {
        let result = "running 3 tests\n\
                      test result: ok. 98 passed; 0 failed; 0 ignored\n\
                      test result: ok. 12 passed; 1 failed; 0 ignored";
        assert_eq!(test_counts(result), Some((110, 1)));
        assert_eq!(test_counts("Tests: 4 failed, 20 passed"), Some((20, 4)));
        assert_eq!(test_counts("all clean"), None);
    }

    #[test]
    fn a_command_loses_its_cd_to_the_workspace() {
        assert_eq!(strip_cd("cd /w && cargo test", "/w"), "cargo test");
        assert_eq!(strip_cd("cd /other && ls", "/w"), "cd /other && ls");
        assert_eq!(strip_cd("  ls  ", "/w"), "ls");
    }

    #[test]
    fn the_ring_head_starts_at_the_top() {
        let (x, y) = ring_head(100., 0.);
        assert!((x - 50.).abs() < 0.01 && (y - 9.).abs() < 0.01);
        let (x, _) = ring_head(100., 0.25);
        assert!((x - 91.).abs() < 0.01, "a quarter is three o'clock");
    }
}
