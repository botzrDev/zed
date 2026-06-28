//! A dockable "Backlinks" panel — the "what links here" surface of the
//! Obsidian/Zed fusion. It maintains a vault-wide index mapping each note name
//! to the `[[wiki link]]`s that point at it, and, for whichever note is open in
//! the active editor, lists the notes that link back to it. Clicking a backlink
//! opens the linking note.

use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration};

use collections::HashSet;
use fs::Fs;
use gpui::{
    Action, App, AppContext as _, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle,
    Focusable, Task, UniformListScrollHandle, WeakEntity, Window, actions, div, uniform_list,
};
use project::{Project, ProjectPath};
use ui::{ListItem, prelude::*};
use util::{ResultExt, paths::PathStyle};
use workspace::{
    OpenOptions, OpenVisible, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

actions!(
    backlinks,
    [
        /// Toggles focus on the backlinks panel.
        ToggleFocus,
    ]
);

const BACKLINKS_PANEL_KEY: &str = "BacklinksPanel";

/// File extensions that the vault treats as notes. Kept in sync with the notes
/// panel so both surfaces agree on what counts as a note.
const NOTE_EXTENSIONS: &[&str] = &["md", "markdown", "mdx", "mdown", "mkd", "mdwn"];

/// Coalesce bursts of worktree change events (which fire on every keystroke in a
/// saved buffer) before re-reading every note to rebuild the index.
const REINDEX_DEBOUNCE: Duration = Duration::from_millis(300);

/// A single `[[wiki link]]` pointing at the active note, discovered in some other
/// note in the vault.
#[derive(Clone)]
struct Backlink {
    /// Absolute path of the note containing the link, used to open it.
    source_abs_path: PathBuf,
    /// File name of the linking note without its extension, shown as the title.
    source_title: SharedString,
    /// Worktree-relative path of the linking note, shown as the subtitle.
    source_relative: SharedString,
    /// 1-based line number of the link within the source note.
    line: u32,
}

/// A note whose links are indexed: its location plus how it is displayed.
struct NoteSource {
    abs_path: PathBuf,
    title: SharedString,
    relative: SharedString,
}

pub struct BacklinksPanel {
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    /// Maps a normalized (trimmed, lowercased) link target to every backlink
    /// pointing at it. A note can be referenced by stem or relative path, so the
    /// same backlink may appear under multiple keys.
    index: HashMap<String, Vec<Backlink>>,
    /// Display title of the note in the active editor, if it is a note.
    active_title: Option<SharedString>,
    /// Normalized identifiers of the active note (stem, relative path, relative
    /// path without extension) used to look the note up in `index`.
    active_keys: Vec<String>,
    /// Backlinks for the active note, in display order.
    current: Vec<Backlink>,
    position: DockPosition,
    scroll_handle: UniformListScrollHandle,
    _index_task: Option<Task<()>>,
    _subscriptions: Vec<gpui::Subscription>,
}

impl BacklinksPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> anyhow::Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, window, cx| Self::new(workspace, window, cx))
    }

    fn new(
        workspace: &mut Workspace,
        _window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        let project = workspace.project().clone();
        let workspace_entity = cx.entity();
        let workspace_handle = workspace_entity.downgrade();

        cx.new(|cx| {
            let subscriptions = vec![
                cx.subscribe(&project, |this: &mut Self, _, event: &project::Event, cx| {
                    if matches!(
                        event,
                        project::Event::WorktreeAdded(_)
                            | project::Event::WorktreeRemoved(_)
                            | project::Event::WorktreeUpdatedEntries(_, _)
                    ) {
                        this.rebuild_index(cx);
                    }
                }),
                cx.subscribe(
                    &workspace_entity,
                    |this: &mut Self, workspace, event: &workspace::Event, cx| {
                        if matches!(event, workspace::Event::ActiveItemChanged) {
                            this.update_active_note(&workspace, cx);
                        }
                    },
                ),
            ];

            let mut this = Self {
                project,
                workspace: workspace_handle,
                focus_handle: cx.focus_handle(),
                index: HashMap::new(),
                active_title: None,
                active_keys: Vec::new(),
                current: Vec::new(),
                position: DockPosition::Right,
                scroll_handle: UniformListScrollHandle::new(),
                _index_task: None,
                _subscriptions: subscriptions,
            };
            this.rebuild_index(cx);
            this.update_active_note(&workspace_entity, cx);
            this
        })
    }

    /// Collects the active editor's note identity and recomputes the displayed
    /// backlinks. A non-note (or no) active item clears the panel.
    fn update_active_note(&mut self, workspace: &Entity<Workspace>, cx: &mut Context<Self>) {
        let project_path = workspace
            .read(cx)
            .active_item(cx)
            .and_then(|item| item.project_path(cx));

        let (title, keys) = match project_path.and_then(|path| self.note_identity(&path)) {
            Some(identity) => identity,
            None => {
                if self.active_title.is_some() || !self.active_keys.is_empty() {
                    self.active_title = None;
                    self.active_keys.clear();
                    self.current.clear();
                    cx.notify();
                }
                return;
            }
        };

        self.active_title = Some(title);
        self.active_keys = keys;
        self.recompute_current();
        cx.notify();
    }

    /// Returns the display title and lookup keys for a project path, or `None` if
    /// it is not a Markdown note.
    fn note_identity(&self, path: &ProjectPath) -> Option<(SharedString, Vec<String>)> {
        let extension = path.path.extension()?;
        if !NOTE_EXTENSIONS
            .iter()
            .any(|known| known.eq_ignore_ascii_case(extension))
        {
            return None;
        }

        let stem = path.path.file_stem().unwrap_or("untitled");
        let relative = path.path.display(PathStyle::local()).into_owned();
        let relative_without_extension = relative
            .strip_suffix(&format!(".{extension}"))
            .unwrap_or(relative.as_str());

        let mut keys = Vec::new();
        for key in [
            stem.to_lowercase(),
            relative.to_lowercase(),
            relative_without_extension.to_lowercase(),
        ] {
            if !key.is_empty() && !keys.contains(&key) {
                keys.push(key);
            }
        }

        Some((SharedString::from(stem.to_string()), keys))
    }

    /// Recomputes `current` from `index` for the active note, deduplicating
    /// backlinks that match under more than one key.
    fn recompute_current(&mut self) {
        let mut seen = HashSet::default();
        let mut current = Vec::new();
        for key in &self.active_keys {
            let Some(links) = self.index.get(key) else {
                continue;
            };
            for link in links {
                if seen.insert((link.source_abs_path.clone(), link.line)) {
                    current.push(link.clone());
                }
            }
        }
        current.sort_by(|left, right| {
            left.source_title
                .to_lowercase()
                .cmp(&right.source_title.to_lowercase())
                .then_with(|| left.source_relative.cmp(&right.source_relative))
                .then_with(|| left.line.cmp(&right.line))
        });
        self.current = current;
    }

    /// Rebuilds the vault-wide link index: gathers every note's location on the
    /// foreground, then reads and parses their contents on a background task.
    /// Each call replaces (and thereby cancels) any in-flight rebuild, so bursts
    /// of worktree events coalesce after the debounce.
    fn rebuild_index(&mut self, cx: &mut Context<Self>) {
        let fs = self.project.read(cx).fs().clone();
        let mut sources = Vec::new();
        for worktree in self.project.read(cx).visible_worktrees(cx) {
            let worktree = worktree.read(cx);
            let abs_root = worktree.abs_path();
            let snapshot = worktree.snapshot();
            for entry in snapshot.entries(false, 0) {
                if !entry.is_file() {
                    continue;
                }
                let Some(extension) = entry.path.extension() else {
                    continue;
                };
                if !NOTE_EXTENSIONS
                    .iter()
                    .any(|known| known.eq_ignore_ascii_case(extension))
                {
                    continue;
                }
                let title = entry
                    .path
                    .file_stem()
                    .unwrap_or("untitled")
                    .to_string()
                    .into();
                let relative = entry.path.display(PathStyle::local()).into_owned().into();
                sources.push(NoteSource {
                    abs_path: abs_root.join(entry.path.as_std_path()),
                    title,
                    relative,
                });
            }
        }

        self._index_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(REINDEX_DEBOUNCE).await;
            let index = cx
                .background_spawn(async move { build_index(fs.as_ref(), sources).await })
                .await;
            this.update(cx, |this, cx| {
                this.index = index;
                this.recompute_current();
                cx.notify();
            })
            .log_err();
        }));
    }

    fn open_backlink(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(backlink) = self.current.get(index) else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        let abs_path = backlink.source_abs_path.clone();
        workspace
            .update(cx, |workspace, cx| {
                workspace.open_abs_path(
                    abs_path,
                    OpenOptions {
                        visible: Some(OpenVisible::None),
                        ..Default::default()
                    },
                    window,
                    cx,
                )
            })
            .detach_and_log_err(cx);
    }

    fn render_backlink(&self, index: usize, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let backlink = self.current.get(index)?;
        let line = backlink.line;
        let subtitle = format!("{}:{}", backlink.source_relative, line);
        Some(
            ListItem::new(index)
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(
                            Icon::new(IconName::ArrowDownRight)
                                .size(IconSize::Small)
                                .color(Color::Muted),
                        )
                        .child(
                            v_flex()
                                .child(Label::new(backlink.source_title.clone()))
                                .child(
                                    Label::new(subtitle)
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                ),
                        ),
                )
                .on_click(
                    cx.listener(move |this, _, window, cx| this.open_backlink(index, window, cx)),
                ),
        )
    }
}

/// Reads each note and folds its `[[wiki link]]`s into a target -> backlinks map.
async fn build_index(fs: &dyn Fs, sources: Vec<NoteSource>) -> HashMap<String, Vec<Backlink>> {
    let mut index: HashMap<String, Vec<Backlink>> = HashMap::new();
    for source in sources {
        let Some(text) = fs.load(&source.abs_path).await.log_err() else {
            continue;
        };
        for link in markdown::extract_wiki_links(&text) {
            // A target may carry a `#heading` fragment (`[[Note#Section]]`);
            // backlinks are tracked per note, so drop it. `split` always yields
            // at least one element, so the fallback is never used.
            let target = link.target.split('#').next().unwrap_or("");
            let key = target.trim().to_lowercase();
            if key.is_empty() {
                continue;
            }
            let line = line_for_offset(&text, link.range.start);
            index.entry(key).or_default().push(Backlink {
                source_abs_path: source.abs_path.clone(),
                source_title: source.title.clone(),
                source_relative: source.relative.clone(),
                line,
            });
        }
    }
    index
}

/// 1-based line number containing the byte at `offset`. Counts newline bytes,
/// which is safe regardless of UTF-8 char boundaries.
fn line_for_offset(text: &str, offset: usize) -> u32 {
    let end = offset.min(text.len());
    text.as_bytes()[..end].iter().filter(|&&b| b == b'\n').count() as u32 + 1
}

impl Focusable for BacklinksPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for BacklinksPanel {}

impl Panel for BacklinksPanel {
    fn persistent_name() -> &'static str {
        "Backlinks Panel"
    }

    fn panel_key() -> &'static str {
        BACKLINKS_PANEL_KEY
    }

    fn position(&self, _: &Window, _: &App) -> DockPosition {
        self.position
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(&mut self, position: DockPosition, _: &mut Window, cx: &mut Context<Self>) {
        self.position = position;
        cx.notify();
    }

    fn default_size(&self, _: &Window, _: &App) -> Pixels {
        px(260.)
    }

    fn icon(&self, _: &Window, _: &App) -> Option<IconName> {
        Some(IconName::Link)
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("Backlinks")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        8
    }
}

impl Render for BacklinksPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let border_color = cx.theme().colors().border;
        let count = self.current.len();

        let header = match &self.active_title {
            Some(title) => format!("Backlinks to {title}"),
            None => "Backlinks".to_string(),
        };

        v_flex()
            .key_context("BacklinksPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .child(
                h_flex()
                    .p_2()
                    .gap_2()
                    .items_center()
                    .border_b_1()
                    .border_color(border_color)
                    .child(
                        Icon::new(IconName::Link)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(Label::new(header).single_line()),
            )
            .child(if count == 0 {
                v_flex()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .p_4()
                    .child(
                        Label::new(match &self.active_title {
                            Some(_) => "No notes link here yet.",
                            None => "Open a note to see what links to it.",
                        })
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    )
                    .into_any_element()
            } else {
                div()
                    .flex_1()
                    .child(
                        uniform_list(
                            "backlink-entries",
                            count,
                            cx.processor(|this, range: std::ops::Range<usize>, _window, cx| {
                                range
                                    .filter_map(|index| {
                                        this.render_backlink(index, cx)
                                            .map(IntoElement::into_any_element)
                                    })
                                    .collect()
                            }),
                        )
                        .size_full()
                        .track_scroll(self.scroll_handle.clone()),
                    )
                    .into_any_element()
            })
    }
}
