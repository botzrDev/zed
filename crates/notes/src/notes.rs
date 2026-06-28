//! A dockable "Notes" panel that turns the active project worktrees into an
//! Obsidian-style vault. It indexes every Markdown file in the project, lets
//! the user fuzzily filter the list, open a note in the editor, and create new
//! notes. This is the front-end surface of the Obsidian/Zed fusion: the notes
//! live as plain Markdown files in the project, so all of Zed's editing,
//! terminal, git and AI tooling operate on them directly.

mod backlinks;

pub use backlinks::BacklinksPanel;

use std::path::PathBuf;

use editor::{Editor, EditorEvent};
use gpui::{
    Action, App, AppContext as _, AsyncWindowContext, Context, Entity, EventEmitter, FocusHandle,
    Focusable, ScrollStrategy, UniformListScrollHandle, WeakEntity, Window, actions, div,
    uniform_list,
};
use project::{Project, ProjectPath};
use ui::{ListItem, Tooltip, prelude::*};
use util::{paths::PathStyle, rel_path::RelPath};
use workspace::{
    OpenOptions, OpenVisible, Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

actions!(
    notes,
    [
        /// Toggles focus on the notes panel.
        ToggleFocus,
        /// Creates a new note in the vault and opens it.
        NewNote,
    ]
);

const NOTES_PANEL_KEY: &str = "NotesPanel";

/// File extensions that the vault treats as notes.
const NOTE_EXTENSIONS: &[&str] = &["md", "markdown", "mdx", "mdown", "mkd", "mdwn"];

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<NotesPanel>(window, cx);
        });
        workspace.register_action(|workspace, _: &NewNote, window, cx| {
            if let Some(panel) = workspace.panel::<NotesPanel>(cx) {
                panel.update(cx, |panel, cx| panel.new_note(window, cx));
            }
        });
        workspace.register_action(|workspace, _: &backlinks::ToggleFocus, window, cx| {
            workspace.toggle_panel_focus::<BacklinksPanel>(window, cx);
        });
    })
    .detach();
}

/// A single Markdown note discovered in one of the project's worktrees.
struct NoteEntry {
    abs_path: PathBuf,
    /// File name without its extension, shown as the note title.
    title: SharedString,
    /// Worktree-relative path, shown as the subtitle and used for filtering.
    relative: SharedString,
}

pub struct NotesPanel {
    project: Entity<Project>,
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    filter_editor: Entity<Editor>,
    /// The full vault index, sorted by title.
    notes: Vec<NoteEntry>,
    /// Indices into `notes` that match the current filter query, in display order.
    filtered: Vec<usize>,
    /// Index into `filtered` of the currently highlighted note, if any.
    selected_index: Option<usize>,
    position: DockPosition,
    scroll_handle: UniformListScrollHandle,
    _subscriptions: Vec<gpui::Subscription>,
}

impl NotesPanel {
    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> anyhow::Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, window, cx| Self::new(workspace, window, cx))
    }

    fn new(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        let project = workspace.project().clone();
        let workspace_handle = cx.entity().downgrade();

        cx.new(|cx| {
            let filter_editor = cx.new(|cx| {
                let mut editor = Editor::single_line(window, cx);
                editor.set_placeholder_text("Search notes…", window, cx);
                editor
            });

            let subscriptions = vec![
                cx.subscribe_in(
                    &filter_editor,
                    window,
                    |this: &mut Self, _, event: &EditorEvent, _window, cx| {
                        if let EditorEvent::BufferEdited = event {
                            this.update_matches(cx);
                            cx.notify();
                        }
                    },
                ),
                cx.subscribe(&project, |this: &mut Self, _, event: &project::Event, cx| {
                    if matches!(
                        event,
                        project::Event::WorktreeAdded(_)
                            | project::Event::WorktreeRemoved(_)
                            | project::Event::WorktreeUpdatedEntries(_, _)
                    ) {
                        this.refresh_notes(cx);
                        cx.notify();
                    }
                }),
            ];

            let mut this = Self {
                project,
                workspace: workspace_handle,
                focus_handle: cx.focus_handle(),
                filter_editor,
                notes: Vec::new(),
                filtered: Vec::new(),
                selected_index: None,
                position: DockPosition::Left,
                scroll_handle: UniformListScrollHandle::new(),
                _subscriptions: subscriptions,
            };
            this.refresh_notes(cx);
            this
        })
    }

    /// Rebuilds the vault index by scanning every visible worktree for Markdown
    /// files, then recomputes the filtered view.
    fn refresh_notes(&mut self, cx: &App) {
        let mut notes = Vec::new();
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
                notes.push(NoteEntry {
                    abs_path: abs_root.join(entry.path.as_std_path()),
                    title,
                    relative,
                });
            }
        }
        notes.sort_by(|left, right| {
            left.title
                .to_lowercase()
                .cmp(&right.title.to_lowercase())
                .then_with(|| left.relative.cmp(&right.relative))
        });
        self.notes = notes;
        self.update_matches(cx);
    }

    /// Recomputes `filtered` from the current filter query. Matching is a simple
    /// case-insensitive substring test against the title and relative path.
    fn update_matches(&mut self, cx: &App) {
        let query = self.filter_editor.read(cx).text(cx).trim().to_lowercase();
        self.filtered = self
            .notes
            .iter()
            .enumerate()
            .filter(|(_, note)| {
                query.is_empty()
                    || note.title.to_lowercase().contains(query.as_str())
                    || note.relative.to_lowercase().contains(query.as_str())
            })
            .map(|(index, _)| index)
            .collect();
        self.selected_index = if self.filtered.is_empty() {
            None
        } else {
            Some(0)
        };
    }

    fn open_note(&mut self, filtered_index: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(note_index) = self.filtered.get(filtered_index).copied() else {
            return;
        };
        let Some(abs_path) = self.notes.get(note_index).map(|note| note.abs_path.clone()) else {
            return;
        };
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };
        self.selected_index = Some(filtered_index);
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
        cx.notify();
    }

    /// Creates a new uniquely-named Markdown note at the root of the first
    /// visible worktree and opens it for editing.
    fn new_note(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(worktree) = self.project.read(cx).visible_worktrees(cx).next() else {
            return;
        };
        let worktree = worktree.read(cx);
        let worktree_id = worktree.id();
        let abs_root = worktree.abs_path().to_path_buf();
        let snapshot = worktree.snapshot();

        let mut file_name = "Untitled.md".to_string();
        let mut counter = 1;
        let relative_path = loop {
            let Ok(candidate) = RelPath::unix(&file_name) else {
                return;
            };
            if snapshot.entry_for_path(candidate).is_none() {
                break candidate.into_arc();
            }
            counter += 1;
            file_name = format!("Untitled {counter}.md");
        };

        let abs_path = abs_root.join(relative_path.as_std_path());
        let project_path = ProjectPath {
            worktree_id,
            path: relative_path,
        };
        let create = self
            .project
            .update(cx, |project, cx| project.create_entry(project_path, false, cx));
        let workspace = self.workspace.clone();
        cx.spawn_in(window, async move |this, cx| {
            create.await?;
            let open_task = workspace.update_in(cx, |workspace, window, cx| {
                workspace.open_abs_path(
                    abs_path,
                    OpenOptions {
                        visible: Some(OpenVisible::None),
                        ..Default::default()
                    },
                    window,
                    cx,
                )
            })?;
            open_task.await?;
            this.update(cx, |this, cx| {
                this.refresh_notes(cx);
                cx.notify();
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
    }

    fn select_next(&mut self, _: &menu::SelectNext, _window: &mut Window, cx: &mut Context<Self>) {
        if self.filtered.is_empty() {
            return;
        }
        let next = match self.selected_index {
            Some(index) => (index + 1).min(self.filtered.len() - 1),
            None => 0,
        };
        self.selected_index = Some(next);
        self.scroll_handle.scroll_to_item(next, ScrollStrategy::Nearest);
        cx.notify();
    }

    fn select_previous(
        &mut self,
        _: &menu::SelectPrevious,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.filtered.is_empty() {
            return;
        }
        let previous = match self.selected_index {
            Some(index) => index.saturating_sub(1),
            None => 0,
        };
        self.selected_index = Some(previous);
        self.scroll_handle
            .scroll_to_item(previous, ScrollStrategy::Nearest);
        cx.notify();
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(selected) = self.selected_index {
            self.open_note(selected, window, cx);
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, window: &mut Window, cx: &mut Context<Self>) {
        self.filter_editor.update(cx, |editor, cx| {
            editor.clear(window, cx);
        });
    }

    fn render_note(
        &self,
        filtered_index: usize,
        note_index: usize,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        let note = self.notes.get(note_index)?;
        let is_selected = self.selected_index == Some(filtered_index);
        Some(
            ListItem::new(filtered_index)
                .toggle_state(is_selected)
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(
                            Icon::new(IconName::FileDoc)
                                .size(IconSize::Small)
                                .color(Color::Muted),
                        )
                        .child(
                            v_flex()
                                .child(Label::new(note.title.clone()))
                                .child(
                                    Label::new(note.relative.clone())
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                ),
                        ),
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.open_note(filtered_index, window, cx)
                })),
        )
    }
}

impl Focusable for NotesPanel {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.filter_editor.focus_handle(cx)
    }
}

impl EventEmitter<PanelEvent> for NotesPanel {}

impl Panel for NotesPanel {
    fn persistent_name() -> &'static str {
        "Notes Panel"
    }

    fn panel_key() -> &'static str {
        NOTES_PANEL_KEY
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
        Some(IconName::Book)
    }

    fn icon_tooltip(&self, _: &Window, _: &App) -> Option<&'static str> {
        Some("Notes")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleFocus)
    }

    fn activation_priority(&self) -> u32 {
        7
    }
}

impl Render for NotesPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let border_color = cx.theme().colors().border;
        let note_count = self.filtered.len();

        v_flex()
            .key_context("NotesPanel")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_previous))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .size_full()
            .child(
                h_flex()
                    .p_2()
                    .gap_2()
                    .items_center()
                    .border_b_1()
                    .border_color(border_color)
                    .child(
                        Icon::new(IconName::MagnifyingGlass)
                            .size(IconSize::Small)
                            .color(Color::Muted),
                    )
                    .child(div().flex_1().child(self.filter_editor.clone()))
                    .child(
                        IconButton::new("new-note", IconName::Plus)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text("New note"))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.new_note(window, cx)
                            })),
                    ),
            )
            .child(if note_count == 0 {
                v_flex()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .p_4()
                    .child(
                        Label::new(if self.notes.is_empty() {
                            "No notes in this vault yet. Create one with the + button."
                        } else {
                            "No notes match your search."
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
                            "notes-entries",
                            note_count,
                            cx.processor(|this, range: std::ops::Range<usize>, _window, cx| {
                                range
                                    .filter_map(|filtered_index| {
                                        let note_index =
                                            this.filtered.get(filtered_index).copied()?;
                                        this.render_note(filtered_index, note_index, cx)
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
