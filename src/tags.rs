//! How tags, ratings and colour labels look and are changed: stars, label
//! swatches, the tag bar, Edit tags and the folder filter. The data lives in XMP
//! sidecars (see `sidecar`); `app` owns loading and saving it.

use crate::app::{Icon, focus_ring, hand, icon_button, paint_icon};
use crate::sidecar::{self, Meta};
use crate::theme::{Palette, lerp};
use eframe::egui::{self, Color32, Mesh, Pos2, Sense, Shape, Stroke, pos2, vec2};
use std::collections::BTreeSet;
use std::f32::consts::{FRAC_PI_2, PI};
use std::path::{Path, PathBuf};

/// The colour labels Lightroom, Bridge and digiKam share, in their order.
pub const LABELS: [(&str, Color32); 5] = [
    ("Red", Color32::from_rgb(0xe0, 0x55, 0x61)),
    ("Yellow", Color32::from_rgb(0xe5, 0xc0, 0x7b)),
    ("Green", Color32::from_rgb(0x98, 0xc3, 0x79)),
    ("Blue", Color32::from_rgb(0x61, 0xaf, 0xef)),
    ("Purple", Color32::from_rgb(0xc6, 0x78, 0xdd)),
];

/// The swatch colour of a label as written in a sidecar (any case).
pub fn label_color(label: &str) -> Option<Color32> {
    LABELS
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(label))
        .map(|(_, color)| *color)
}

/// A tag as typed, tidied: `/` between levels, no empty or padded levels.
pub fn normalize(tag: &str) -> String {
    tag.split('/')
        .map(str::trim)
        .filter(|level| !level.is_empty())
        .collect::<Vec<_>>()
        .join("/")
}

/// How well `tag` matches the typed `query`, higher is better; `None` when
/// the query's letters don't all appear in the tag, in order. Matches at the
/// start of a level (`Shot/Action`) or a word, and runs of letters, count
/// more, so `sa` finds `Shot/Action` before `Seasonal`.
pub fn fuzzy_score(query: &str, tag: &str) -> Option<u32> {
    let query: Vec<char> = query.trim().to_lowercase().chars().collect();
    if query.is_empty() {
        return Some(0);
    }
    let tag_lower: Vec<char> = tag.to_lowercase().chars().collect();
    let mut score = 0;
    let mut next = 0;
    let mut previous: Option<usize> = None;
    for wanted in &query {
        let found = (next..tag_lower.len()).find(|&index| tag_lower[index] == *wanted)?;
        let starts_word = found == 0 || matches!(tag_lower[found - 1], '/' | ' ' | '-' | '_');
        score += 1;
        if starts_word {
            score += 8;
        }
        if previous == Some(found.wrapping_sub(1)) {
            score += 5;
        }
        previous = Some(found);
        next = found + 1;
    }
    let text: String = query.iter().collect();
    let lower: String = tag_lower.iter().collect();
    if lower.split('/').any(|level| level.starts_with(&text)) {
        score += 30;
    } else if lower.contains(&text) {
        score += 15;
    }
    Some(score)
}

/// The best matches for `query` among `known`, leaving out `skip`.
pub fn suggestions(query: &str, known: &BTreeSet<String>, skip: &BTreeSet<String>) -> Vec<String> {
    let mut found = ranked(query, known, skip);
    found.truncate(8);
    found
}

/// Every tag of `known` matching `query`, best first, leaving out `skip`.
pub fn ranked(query: &str, known: &BTreeSet<String>, skip: &BTreeSet<String>) -> Vec<String> {
    let mut scored: Vec<(u32, &String)> = known
        .iter()
        .filter(|tag| !skip.contains(*tag) && !sidecar::is_internal(tag))
        .filter_map(|tag| fuzzy_score(query, tag).map(|score| (score, tag)))
        .collect();
    // Best score first, then the shorter tag, then A to Z.
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.len().cmp(&b.1.len())).then(a.1.cmp(b.1)));
    scored.into_iter().map(|(_, tag)| tag.clone()).collect()
}

/// Every tag used before, in any folder: the tag list in
/// `$XDG_CONFIG_HOME/ommata/tags`, one per line.
pub fn load_list() -> BTreeSet<String> {
    crate::settings::config_file("tags")
        .and_then(|file| std::fs::read_to_string(file).ok())
        .map(|text| {
            text.lines()
                .map(normalize)
                .filter(|tag| !tag.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Write the tag list.
pub fn save_list(tags: &BTreeSet<String>) -> std::io::Result<()> {
    let Some(file) = crate::settings::config_file("tags") else {
        return Ok(());
    };
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text: String = tags.iter().map(|tag| format!("{tag}\n")).collect();
    std::fs::write(file, text)
}

/// Which files the arrow keys step through.
#[derive(Clone, Default, PartialEq, Debug)]
pub struct Filter {
    /// Every one of these (or a tag under it) must be set.
    pub tags: BTreeSet<String>,
    /// At least this many stars; 0 for any.
    pub min_rating: i8,
    pub label: Option<String>,
    /// Only rejected files, to clear them out.
    pub rejected: bool,
}

impl Filter {
    pub fn is_active(&self) -> bool {
        *self != Self::default()
    }

    pub fn matches(&self, meta: &Meta) -> bool {
        let label = self.label.as_ref().is_none_or(|label| {
            meta.label
                .as_ref()
                .is_some_and(|own| own.eq_ignore_ascii_case(label))
        });
        let rating = if self.rejected {
            meta.rating < 0
        } else {
            meta.rating >= self.min_rating
        };
        label && rating && self.tags.iter().all(|tag| meta.has(tag))
    }
}

/// The files of `listing` that `filter` lets through, in order, and where
/// `keep` is among them. `keep` stays even when it no longer matches, so
/// tagging a file never jumps away from it; with no match at all, the whole
/// folder stays browsable.
pub fn narrow(
    listing: &[PathBuf],
    keep: Option<&Path>,
    filter: &Filter,
    meta: impl Fn(&Path) -> Meta,
) -> (Vec<PathBuf>, usize) {
    let matching: Vec<&PathBuf> = listing
        .iter()
        .filter(|path| filter.matches(&meta(path)))
        .collect();
    let files: Vec<PathBuf> = if matching.is_empty() {
        listing.to_vec()
    } else {
        listing
            .iter()
            .filter(|path| Some(path.as_path()) == keep || matching.contains(path))
            .cloned()
            .collect()
    };
    let index = keep
        .and_then(|keep| files.iter().position(|path| path == keep))
        .unwrap_or(0);
    (files, index)
}

/// Draw a five-pointed star, filled or outlined.
pub fn paint_star(
    painter: &egui::Painter,
    center: Pos2,
    radius: f32,
    color: Color32,
    filled: bool,
) {
    let points: Vec<Pos2> = (0..10)
        .map(|index| {
            let angle = -FRAC_PI_2 + index as f32 * PI / 5.0;
            let reach = if index % 2 == 0 {
                radius
            } else {
                radius * 0.45
            };
            center + vec2(angle.cos(), angle.sin()) * reach
        })
        .collect();
    if filled {
        // The star is star-shaped around its centre, so a fan covers it.
        let mut mesh = Mesh::default();
        mesh.colored_vertex(center, color);
        for point in &points {
            mesh.colored_vertex(*point, color);
        }
        for index in 0..10 {
            mesh.add_triangle(0, 1 + index, 1 + (index + 1) % 10);
        }
        painter.add(Shape::mesh(mesh));
    }
    painter.add(Shape::closed_line(points, Stroke::new(1.2, color)));
}

/// Five stars to click, previewing the rating under the pointer. Returns
/// the rating clicked; clicking the current rating clears it.
pub fn stars(ui: &mut egui::Ui, rating: i8, palette: &Palette) -> Option<i8> {
    let size = 22.0;
    let (rect, response) = ui.allocate_exact_size(vec2(size * 5.0, size), Sense::click());
    let under = response
        .hover_pos()
        .map(|pointer| (((pointer.x - rect.left()) / size).floor() as i8 + 1).clamp(1, 5));
    let shown = under.unwrap_or(rating.max(0));
    for star in 1..=5_i8 {
        let center = pos2(
            rect.left() + size * (f32::from(star) - 0.5),
            rect.center().y,
        );
        let color = if star <= shown {
            palette.accent
        } else {
            palette.muted
        };
        paint_star(ui.painter(), center, size * 0.38, color, star <= shown);
    }
    focus_ring(ui, &response);
    let clicked = hand(response).clicked();
    clicked.then(|| {
        let star = under.unwrap_or(1);
        if star == rating { 0 } else { star }
    })
}

/// The colour labels as swatches. Returns the click: a label, or `None`
/// when the selected one was clicked again (cleared).
pub fn labels(
    ui: &mut egui::Ui,
    current: Option<&str>,
    palette: &Palette,
) -> Option<Option<String>> {
    let mut picked = None;
    for (name, color) in LABELS {
        let (rect, response) = ui.allocate_exact_size(vec2(24.0, 24.0), Sense::click());
        let selected = current.is_some_and(|label| label.eq_ignore_ascii_case(name));
        let radius = if response.hovered() || selected {
            8.5
        } else {
            7.0
        };
        ui.painter().circle_filled(rect.center(), radius, color);
        if selected {
            ui.painter()
                .circle_stroke(rect.center(), 11.0, Stroke::new(1.5, palette.fg));
        }
        focus_ring(ui, &response);
        if hand(response).on_hover_text(name).clicked() {
            picked = Some((!selected).then(|| name.to_string()));
        }
    }
    picked
}

/// What was clicked in the top bar's summary.
pub enum SummaryClick {
    /// The rating, label or reject: show or hide the tag bar.
    TagBar,
    /// A tag: open Edit tags.
    EditTags,
}

/// The widest the top bar's tag bubbles get; the rest show as "+n".
const SUMMARY_TAGS_WIDTH: f32 = 240.0;

/// A file's rating, label, reject and tags, small, for the top bar. Nothing
/// when it has none.
pub fn summary(ui: &mut egui::Ui, meta: &Meta, palette: &Palette) -> Option<SummaryClick> {
    let tags: Vec<&str> = meta
        .tags
        .iter()
        .filter(|tag| !sidecar::is_internal(tag))
        .map(|tag| tag.rsplit('/').next().unwrap_or(tag))
        .collect();
    let marks = meta.rating != 0 || meta.label.is_some();
    if tags.is_empty() && !marks {
        return None;
    }
    let mut clicked = None;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        if marks {
            let row = ui.horizontal(|ui| {
                // The tag bar's order: rating, colour label, reject.
                for _ in 0..meta.rating.max(0) {
                    let (star, _) = ui.allocate_exact_size(vec2(12.0, 12.0), Sense::hover());
                    paint_star(ui.painter(), star.center(), 5.5, palette.accent, true);
                }
                if let Some(color) = meta.label.as_deref().and_then(label_color) {
                    let (dot, _) = ui.allocate_exact_size(vec2(10.0, 10.0), Sense::hover());
                    ui.painter().circle_filled(dot.center(), 4.5, color);
                }
                if meta.rating < 0 {
                    let (rect, _) = ui.allocate_exact_size(vec2(18.0, 18.0), Sense::hover());
                    paint_icon(ui.painter(), Icon::Reject, rect.center(), palette.danger);
                }
            });
            let response = row.response.interact(Sense::click());
            if hand(response).on_hover_text("Tag bar (Ctrl+K)").clicked() {
                clicked = Some(SummaryClick::TagBar);
            }
        }
        let mut used = 0.0;
        for (shown, tag) in tags.iter().enumerate() {
            let width = bubble_width(ui, tag, true);
            if used + width > SUMMARY_TAGS_WIDTH && shown > 0 {
                let rest = tags[shown..].join(", ");
                let more = bubble(ui, &format!("+{}", tags.len() - shown), Bubble::Small, palette);
                if more.on_hover_text(format!("{rest}\nEdit tags")).clicked() {
                    clicked = Some(SummaryClick::EditTags);
                }
                break;
            }
            used += width + ui.spacing().item_spacing.x;
            if bubble(ui, tag, Bubble::Small, palette)
                .on_hover_text("Edit tags")
                .clicked()
            {
                clicked = Some(SummaryClick::EditTags);
            }
        }
    });
    clicked
}

/// How a tag bubble looks.
#[derive(Clone, Copy, PartialEq)]
enum Bubble {
    /// In the top bar's summary: small, filled.
    Small,
    /// Not set (Edit tags, Filter): outlined; clicking sets it.
    Off,
    /// Set (Filter): filled.
    On,
    /// The file's tag in Edit tags: filled, red under the pointer, since
    /// clicking removes it.
    Removable,
}

fn bubble_galley(ui: &egui::Ui, text: &str, small: bool) -> std::sync::Arc<egui::Galley> {
    let font = if small {
        egui::FontId::proportional(13.0)
    } else {
        egui::TextStyle::Body.resolve(ui.style())
    };
    ui.painter()
        .layout_no_wrap(text.to_string(), font, Color32::PLACEHOLDER)
}

fn bubble_padding(small: bool) -> egui::Vec2 {
    if small { vec2(7.0, 2.0) } else { vec2(10.0, 4.0) }
}

fn bubble_width(ui: &egui::Ui, text: &str, small: bool) -> f32 {
    bubble_galley(ui, text, small).size().x + 2.0 * bubble_padding(small).x
}

/// A tag as a rounded bubble, shaped like the bars' buttons.
fn bubble(ui: &mut egui::Ui, text: &str, kind: Bubble, palette: &Palette) -> egui::Response {
    let small = kind == Bubble::Small;
    let galley = bubble_galley(ui, text, small);
    let padding = bubble_padding(small);
    let height = if small { 20.0 } else { ui.spacing().interact_size.y };
    let size = vec2(galley.size().x + 2.0 * padding.x, height.max(galley.size().y + 2.0 * padding.y));
    let (rect, response) = ui.allocate_exact_size(size, Sense::click());
    let hovered = response.hovered();
    let widget = ui.style().interact(&response);
    let corner = widget.corner_radius;
    let selection = ui.visuals().selection;
    // Tinted from the bar's own colours, so the bubble reads on any theme
    // (some give buttons almost the bar's colour).
    let tint = |amount: f32| lerp(palette.surface, palette.fg, amount);
    let (fill, stroke, text_color) = match kind {
        Bubble::Small => (tint(if hovered { 0.22 } else { 0.14 }), Stroke::NONE, palette.fg),
        Bubble::Off if hovered => (tint(0.14), Stroke::new(1.0, tint(0.30)), palette.fg),
        Bubble::Off => (Color32::TRANSPARENT, Stroke::new(1.0, tint(0.30)), palette.muted),
        Bubble::Removable if hovered => (
            palette.danger.gamma_multiply(0.25),
            Stroke::new(1.0, palette.danger),
            palette.danger,
        ),
        Bubble::On | Bubble::Removable => (selection.bg_fill, Stroke::NONE, selection.stroke.color),
    };
    ui.painter()
        .rect(rect, corner, fill, stroke, egui::StrokeKind::Inside);
    let at = rect.center() - galley.size() / 2.0;
    ui.painter().galley(at, galley, text_color);
    focus_ring(ui, &response);
    hand(response)
}

/// Tag bubbles for `tags` to turn on and off; returns the one clicked, and
/// whether with the right button.
/// `removable`: the tags that are on turn red under the pointer, since a
/// click removes them from the file. `quick` marks the quick tags with their
/// keys, `quick_keys`.
fn chips<'a>(
    ui: &mut egui::Ui,
    tags: impl IntoIterator<Item = &'a String>,
    on: impl Fn(&str) -> bool,
    removable: bool,
    quick: &[String; 9],
    quick_keys: &[String; 9],
    palette: &Palette,
) -> Option<(String, bool)> {
    let mut clicked = None;
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = vec2(6.0, 6.0);
        for tag in tags {
            let kind = match (on(tag), removable) {
                (false, _) => Bubble::Off,
                (true, false) => Bubble::On,
                (true, true) => Bubble::Removable,
            };
            let action = match kind {
                Bubble::Removable => "Remove",
                Bubble::Off if removable => "Add",
                _ => "",
            };
            let key = quick
                .iter()
                .position(|quick| quick == tag)
                .map(|index| quick_keys[index].clone());
            let tip = match (action, key) {
                ("", Some(key)) => key,
                (action, Some(key)) => format!("{action} ({key})"),
                (action, None) => action.to_string(),
            };
            let chip = bubble(ui, tag, kind, palette);
            let chip = if tip.is_empty() { chip } else { chip.on_hover_text(tip) };
            if chip.clicked() {
                clicked = Some((tag.clone(), false));
            } else if chip.secondary_clicked() {
                clicked = Some((tag.clone(), true));
            }
        }
    });
    clicked
}

fn row_label(ui: &mut egui::Ui, text: &str, palette: &Palette) {
    let label = egui::RichText::new(text).color(palette.muted);
    ui.add_sized([72.0, 24.0], egui::Label::new(label));
}

/// The tag bar under the top bar: rating, colour label, reject, a field to
/// add a tag, and Edit tags. `focus` puts the cursor in the field. Returns
/// whether `meta` changed and whether Edit tags was clicked.
pub fn bar(
    ui: &mut egui::Ui,
    meta: &mut Meta,
    input: &mut String,
    focus: bool,
    known: &BTreeSet<String>,
    palette: &Palette,
) -> (bool, bool) {
    let before = meta.clone();
    let mut edit = false;
    ui.horizontal(|ui| {
        if let Some(rating) = stars(ui, meta.rating, palette) {
            meta.rating = rating;
        }
        ui.separator();
        if let Some(label) = labels(ui, meta.label.as_deref(), palette) {
            meta.label = label;
        }
        let rejected = meta.rating < 0;
        let (color, tip) = if rejected {
            (Some(palette.danger), "Rejected; click to undo (X)")
        } else {
            (None, "Reject (X)")
        };
        if icon_button(ui, Icon::Reject, rejected, color, tip).clicked() {
            meta.rating = if rejected { 0 } else { -1 };
        }
        ui.separator();
        add_field(ui, meta, input, 180.0, focus, Some(known));
        if hand(ui.button("Edit tags"))
            .on_hover_text("Remove tags, or pick from the folder's")
            .clicked()
        {
            edit = true;
        }
    });
    (*meta != before, edit)
}

/// What a tag field's suggestion list remembers between frames.
#[derive(Clone, Copy, Default)]
struct SuggestState {
    /// The suggestion picked with the arrow keys.
    selected: Option<usize>,
    /// Where the list was drawn, so a click on it counts although the field
    /// loses the cursor on the press.
    list: Option<egui::Rect>,
}

/// A field that adds the typed tag on Enter and stays focused for the next.
/// With `suggest`, a list under it offers the best matches among those tags
/// while typing: ↑ ↓ pick one, Tab or a click takes it, Enter adds it.
fn add_field(
    ui: &mut egui::Ui,
    meta: &mut Meta,
    input: &mut String,
    width: f32,
    focus: bool,
    suggest: Option<&BTreeSet<String>>,
) {
    let id = ui.scope_id().with("add tag");
    let empty = BTreeSet::new();
    let known = suggest.unwrap_or(&empty);
    let field = TagField {
        width,
        focus,
        hint: "Add tag, / to nest",
        clear: true,
    };
    if let Some(tag) = field.show(ui, id, input, known, &meta.tags) {
        meta.tags.insert(tag);
    }
}

/// A text field for one tag that suggests the best matches among known tags
/// while typing: ↑ ↓ pick one, Tab fills it in, a click or Enter takes it.
pub struct TagField<'a> {
    pub width: f32,
    /// Put the cursor in the field.
    pub focus: bool,
    pub hint: &'a str,
    /// Empty the field and keep the cursor in it once a tag is taken, ready
    /// for the next; otherwise the field keeps the tag.
    pub clear: bool,
}

impl TagField<'_> {
    /// Returns the tag taken this frame, if any. `skip` is left out of the
    /// suggestions.
    pub fn show(
        &self,
        ui: &mut egui::Ui,
        id: egui::Id,
        input: &mut String,
        known: &BTreeSet<String>,
        skip: &BTreeSet<String>,
    ) -> Option<String> {
        let mut state: SuggestState = ui.data(|data| data.get_temp(id)).unwrap_or_default();
        let had_focus = ui.memory(|memory| memory.has_focus(id));
        // Suggestions only while typing, and not once the field holds one.
        let found = if had_focus && !input.trim().is_empty() {
            let mut found = suggestions(input, known, skip);
            found.retain(|tag| *tag != normalize(input));
            found
        } else {
            Vec::new()
        };
        state.selected = state.selected.filter(|index| *index < found.len());
        let mut completed = false;
        if !found.is_empty() {
            ui.input_mut(|keys| {
                let count = found.len();
                if keys.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown) {
                    state.selected = Some(state.selected.map_or(0, |index| (index + 1) % count));
                }
                if keys.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp) {
                    state.selected = Some(state.selected.map_or(count - 1, |index| {
                        (index + count - 1) % count
                    }));
                }
                if keys.consume_key(egui::Modifiers::NONE, egui::Key::Tab) {
                    *input = found[state.selected.unwrap_or(0)].clone();
                    state.selected = None;
                    completed = true;
                }
            });
        }
        let field = ui.add(
            egui::TextEdit::singleline(input)
                .id(id)
                // While suggestions show, Tab takes one instead of moving on.
                .lock_focus(!found.is_empty())
                .hint_text(self.hint)
                .background_color(ui.visuals().widgets.inactive.bg_fill)
                .desired_width(self.width)
                // A grid column starts narrow; this keeps every field as wide.
                .min_size(vec2(self.width, 0.0)),
        );
        if field.changed() {
            state.selected = None;
        }
        if self.focus || completed {
            field.request_focus();
        }
        if completed {
            // Tab already moved the cursor on; keep it here, after the tag.
            if let Some(mut edit) = egui::TextEdit::load_state(ui.ctx(), id) {
                let end = egui::text::CCursor::new(input.chars().count());
                edit.cursor
                    .set_char_range(Some(egui::text::CCursorRange::one(end)));
                edit.store(ui.ctx(), id);
            }
        }
        let entered = field.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter));
        let picked = state.selected.and_then(|index| found.get(index).cloned());
        let tag = picked.unwrap_or_else(|| normalize(input));
        let mut taken = (entered && !tag.is_empty()).then_some(tag);
        // The list stays up while the pointer is on it, so the click that takes
        // the field's cursor away still lands.
        let over_list = state.list.is_some_and(|rect| {
            ui.input(|input| input.pointer.hover_pos().is_some_and(|pos| rect.contains(pos)))
        });
        state.list = None;
        if taken.is_none() && !found.is_empty() && (field.has_focus() || over_list) {
            let palette_fg = ui.visuals().text_color();
            let list = egui::Area::new(id.with("suggestions"))
                .order(egui::Order::Foreground)
                .fixed_pos(field.rect.left_bottom() + vec2(0.0, 4.0))
                .show(ui.ctx(), |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.set_min_width(field.rect.width());
                        for (index, suggestion) in found.iter().enumerate() {
                            let selected = state.selected == Some(index);
                            let text = egui::RichText::new(suggestion).color(palette_fg);
                            if hand(ui.selectable_label(selected, text)).clicked() {
                                taken = Some(suggestion.clone());
                            }
                        }
                    });
                });
            state.list = Some(list.response.rect);
        }
        if let Some(tag) = &taken {
            state = SuggestState::default();
            if self.clear {
                input.clear();
                field.request_focus();
            } else {
                input.clone_from(tag);
            }
        }
        ui.data_mut(|data| data.insert_temp(id, state));
        taken
    }
}

/// What a click in Edit tags asks for.
#[derive(PartialEq, Debug)]
pub enum PanelChange {
    /// The file's tags changed.
    Tags,
    /// Take this tag off the tag list.
    Forget(String),
}

/// Edit tags for one file: its tags and the folder's as chips to turn on
/// and off, every tag used before under All tags, and a field that adds a
/// new tag and narrows the chips to the tags matching what is typed.
#[allow(clippy::too_many_arguments)]
pub fn panel(
    ui: &mut egui::Ui,
    meta: &mut Meta,
    known: &BTreeSet<String>,
    list: &BTreeSet<String>,
    quick: &[String; 9],
    quick_keys: &[String; 9],
    input: &mut String,
    palette: &Palette,
) -> Option<PanelChange> {
    let before = meta.clone();
    let mut forget = None;
    // Ready for typing: the field takes the cursor whenever nothing has it.
    let idle = ui.memory(|memory| memory.focused().is_none());
    add_field(ui, meta, input, 260.0, idle, None);
    ui.add_space(4.0);
    let shown: BTreeSet<String> = known
        .iter()
        .chain(quick.iter().filter(|tag| !tag.is_empty()))
        .chain(&meta.tags)
        .filter(|tag| !sidecar::is_internal(tag))
        .cloned()
        .collect();
    let toggle = |meta: &mut Meta, tag: String| {
        if !meta.tags.remove(&tag) {
            meta.tags.insert(tag);
        }
    };
    let query = normalize(input);
    if !query.is_empty() {
        let every: BTreeSet<String> = shown.iter().chain(list).cloned().collect();
        let found = ranked(&query, &every, &BTreeSet::new());
        if found.is_empty() {
            let hint = format!("No tag matches; Enter adds {query}.");
            ui.label(egui::RichText::new(hint).color(palette.muted));
        }
        let on = |tag: &str| meta.tags.contains(tag);
        if let Some((tag, _)) = chips(ui, &found, on, true, quick, quick_keys, palette) {
            toggle(meta, tag);
        }
    } else {
        if shown.is_empty() {
            ui.label(egui::RichText::new("No tags yet in this folder.").color(palette.muted));
        } else {
            ui.label(
                egui::RichText::new("Click a tag to remove it from this file, or to add it.")
                    .color(palette.muted)
                    .size(12.0),
            );
        }
        let on = |tag: &str| meta.tags.contains(tag);
        if let Some((tag, _)) = chips(ui, &shown, on, true, quick, quick_keys, palette) {
            toggle(meta, tag);
        }
        let others: Vec<&String> = list.iter().filter(|tag| !shown.contains(*tag)).collect();
        if !others.is_empty() {
            ui.add_space(4.0);
            egui::CollapsingHeader::new(format!("All tags ({})", others.len()))
                .id_salt("all tags")
                .show(ui, |ui| {
                    ui.label(
                        egui::RichText::new("Tags from other folders. Right-click to forget one.")
                            .color(palette.muted)
                            .size(12.0),
                    );
                    let none = &Default::default();
                    if let Some((tag, right)) =
                        chips(ui, others, |_| false, true, none, none, palette)
                    {
                        if right {
                            forget = Some(tag);
                        } else {
                            meta.tags.insert(tag);
                        }
                    }
                });
        }
    }
    if let Some(tag) = forget {
        return Some(PanelChange::Forget(tag));
    }
    (*meta != before).then_some(PanelChange::Tags)
}

/// A change to the tag list asked for in Settings.
#[derive(PartialEq, Debug)]
pub enum ListChange {
    Add(String),
    Forget(String),
}

/// The tag list in Settings: a field that adds a tag to it without putting
/// it on a file, and every tag on it as a bubble a click forgets.
pub fn list_editor(
    ui: &mut egui::Ui,
    list: &BTreeSet<String>,
    input: &mut String,
    palette: &Palette,
) -> Option<ListChange> {
    ui.label(
        egui::RichText::new(
            "Tags offered while typing one, in every folder. Click a tag to forget it.",
        )
        .color(palette.muted)
        .size(12.0),
    );
    let field = TagField {
        width: 260.0,
        focus: false,
        hint: "Add a tag to the list, / to nest",
        clear: true,
    };
    let id = ui.make_persistent_id("tag list field");
    let added = field.show(ui, id, input, list, list);
    if let Some(tag) = added.filter(|tag| !sidecar::is_internal(tag)) {
        return Some(ListChange::Add(tag));
    }
    if list.is_empty() {
        ui.label(egui::RichText::new("No tags yet.").color(palette.muted));
        return None;
    }
    ui.add_space(4.0);
    let none = &Default::default();
    chips(ui, list, |_| true, true, none, none, palette).map(|(tag, _)| ListChange::Forget(tag))
}

/// The filter panel: which tags, rating and label a file needs for the
/// arrow keys to stop on it. Returns whether `filter` changed.
pub fn filter_panel(
    ui: &mut egui::Ui,
    filter: &mut Filter,
    known: &BTreeSet<String>,
    (matching, total): (usize, usize),
    palette: &Palette,
) -> bool {
    let before = filter.clone();
    let count = if filter.is_active() {
        format!("{matching} of {total} files match")
    } else {
        format!("All {total} files")
    };
    ui.label(egui::RichText::new(count).color(palette.muted));
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        row_label(ui, "At least", palette);
        ui.add_enabled_ui(!filter.rejected, |ui| {
            if let Some(rating) = stars(ui, filter.min_rating, palette) {
                filter.min_rating = rating;
            }
        });
        ui.add_space(12.0);
        if hand(ui.selectable_label(filter.rejected, "Rejected only")).clicked() {
            filter.rejected = !filter.rejected;
        }
    });
    ui.horizontal(|ui| {
        row_label(ui, "Label", palette);
        if let Some(label) = labels(ui, filter.label.as_deref(), palette) {
            filter.label = label;
        }
    });
    ui.add_space(8.0);
    ui.label(egui::RichText::new("Tags").size(15.0));
    let shown: BTreeSet<String> = known
        .iter()
        .chain(&filter.tags)
        .filter(|tag| !sidecar::is_internal(tag))
        .cloned()
        .collect();
    if shown.is_empty() {
        ui.label(egui::RichText::new("No tags in this folder.").color(palette.muted));
    } else {
        ui.label(
            egui::RichText::new("Files need every tag picked here.")
                .color(palette.muted)
                .size(12.0),
        );
    }
    if let Some((tag, _)) = chips(
        ui,
        &shown,
        |tag| filter.tags.contains(tag),
        false,
        &Default::default(),
        &Default::default(),
        palette,
    ) && !filter.tags.remove(&tag)
    {
        filter.tags.insert(tag);
    }
    if filter.is_active() {
        ui.add_space(8.0);
        if hand(ui.button("Clear filter")).clicked() {
            *filter = Filter::default();
        }
    }
    *filter != before
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(tags: &[&str], rating: i8, label: Option<&str>) -> Meta {
        Meta {
            tags: tags.iter().map(|tag| tag.to_string()).collect(),
            rating,
            label: label.map(String::from),
        }
    }

    #[test]
    fn fuzzy_search_ranks_level_starts_first() {
        let known: BTreeSet<String> = ["Shot/Action", "Seasonal", "Shot/Establishing", "People/Sam"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(fuzzy_score("xyz", "Shot/Action"), None);
        assert!(fuzzy_score("ACT", "Shot/Action").is_some());
        let found = suggestions("sa", &known, &BTreeSet::new());
        assert_eq!(found.first().map(String::as_str), Some("People/Sam"));
        assert!(found.contains(&"Shot/Action".to_string()));
        let found = suggestions("est", &known, &BTreeSet::new());
        assert_eq!(found, vec!["Shot/Establishing".to_string()]);
        let skip: BTreeSet<String> = ["People/Sam".to_string()].into();
        assert!(!suggestions("sam", &known, &skip).contains(&"People/Sam".to_string()));
    }

    #[test]
    fn tidies_typed_tags() {
        assert_eq!(normalize(" Shot / Action "), "Shot/Action");
        assert_eq!(normalize("//Best//"), "Best");
        assert_eq!(normalize(" / "), "");
    }

    #[test]
    fn narrows_the_listing() {
        let listing: Vec<PathBuf> = ["a.jpg", "b.jpg", "c.jpg", "d.mp4"]
            .map(PathBuf::from)
            .into();
        let meta = |path: &Path| match path.to_str() {
            Some("a.jpg") => meta(&["Best"], 4, None),
            Some("b.jpg") => meta(&["Best", "Shot/People"], 5, None),
            Some("c.jpg") => meta(&[], -1, None),
            _ => meta(&["Shot/Action"], 0, None),
        };
        let names = |(files, index): (Vec<PathBuf>, usize)| {
            let names: Vec<String> = files
                .iter()
                .map(|file| file.display().to_string())
                .collect();
            (names.join(" "), index)
        };
        let best = Filter {
            tags: ["Best".into()].into(),
            ..Default::default()
        };
        // The current file stays in the list while it doesn't match.
        assert_eq!(
            names(narrow(&listing, Some(Path::new("d.mp4")), &best, meta)),
            ("a.jpg b.jpg d.mp4".into(), 2)
        );
        assert_eq!(
            names(narrow(&listing, Some(Path::new("b.jpg")), &best, meta)),
            ("a.jpg b.jpg".into(), 1)
        );
        // No filter: everything.
        assert_eq!(
            names(narrow(
                &listing,
                Some(Path::new("c.jpg")),
                &Filter::default(),
                meta
            ))
            .0,
            "a.jpg b.jpg c.jpg d.mp4"
        );
        // Nothing matches: the whole folder stays browsable.
        let none = Filter {
            label: Some("Purple".into()),
            ..Default::default()
        };
        assert_eq!(
            names(narrow(&listing, Some(Path::new("a.jpg")), &none, meta)),
            ("a.jpg b.jpg c.jpg d.mp4".into(), 0)
        );
        let rejected = Filter {
            rejected: true,
            ..Default::default()
        };
        assert_eq!(
            names(narrow(&listing, Some(Path::new("c.jpg")), &rejected, meta)),
            ("c.jpg".into(), 0)
        );
        let tagged = Filter {
            tags: ["Shot".into()].into(),
            ..Default::default()
        };
        assert_eq!(
            names(narrow(&listing, None, &tagged, meta)),
            ("b.jpg d.mp4".into(), 0)
        );
    }

    #[test]
    fn filter_matches() {
        let clip = meta(&["Shot/Action", "Best"], 3, Some("Red"));
        assert!(Filter::default().matches(&clip));
        let needs = |filter: Filter| filter.matches(&clip);
        assert!(needs(Filter {
            tags: ["Shot".into()].into(),
            ..Default::default()
        }));
        assert!(!needs(Filter {
            tags: ["Shot".into(), "Day 1".into()].into(),
            ..Default::default()
        }));
        assert!(needs(Filter {
            min_rating: 3,
            ..Default::default()
        }));
        assert!(!needs(Filter {
            min_rating: 4,
            ..Default::default()
        }));
        assert!(needs(Filter {
            label: Some("red".into()),
            ..Default::default()
        }));
        assert!(!needs(Filter {
            label: Some("Blue".into()),
            ..Default::default()
        }));
        assert!(!needs(Filter {
            rejected: true,
            ..Default::default()
        }));
        let rejected = meta(&[], -1, None);
        assert!(
            Filter {
                rejected: true,
                ..Default::default()
            }
            .matches(&rejected)
        );
        assert!(
            !Filter {
                min_rating: 1,
                ..Default::default()
            }
            .matches(&rejected)
        );
    }
}
