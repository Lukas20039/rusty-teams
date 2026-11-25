//! The OneDrive page: the signed-in user's own files, through the Graph token the Teams session already has.

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use teams_core::{CloudFile, DriveEntry, DriveFolder, QuickSite};

use crate::app_state::AppState;
use crate::channel_files::{Crumb, LibraryPath, library_file, sorted_entries};
use crate::demo_library::DemoLibrary;
use crate::downloads::{self, PartFile};
use crate::i18n::tr;
use crate::notice::short_error;
use crate::runtime;
use crate::theme;
use crate::views::attachment_tray::{display_name, read_attachment};
use crate::views::file_preview::{
    self, Decoded, MAX_PREVIEW_BYTES, PreviewKind, pages_view, preview_kind, render_image,
    text_view,
};
use crate::views::shared_tab::{LibraryView, Listing, SharedCommand, SharedRun, render_library};

const DEMO_DRIVE: &str = "onedrive";
/// Library rows carry their kind in `drive_id` and the team or site id in `id`.
const TEAM_LIBRARY: &str = "team-library";
const SITE_LIBRARY: &str = "site-library";
const DEMO_TEAMS: [(&str, &str); 2] = [
    ("demo-team-company", "Company"),
    ("demo-team-product", "Product"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Recent,
    MyFiles,
    Shared,
    Libraries,
}

impl Section {
    const ALL: [Section; 4] = [
        Section::Recent,
        Section::MyFiles,
        Section::Shared,
        Section::Libraries,
    ];

    fn label(self) -> String {
        match self {
            Section::Recent => tr("onedrive.recent"),
            Section::MyFiles => tr("onedrive.my_files"),
            Section::Shared => tr("onedrive.shared"),
            Section::Libraries => tr("onedrive.libraries"),
        }
    }
}

fn library_row(kind: &str, id: &str, name: &str) -> DriveEntry {
    DriveEntry {
        drive_id: kind.to_owned(),
        id: id.to_owned(),
        name: name.to_owned(),
        web_url: String::new(),
        size: 0,
        modified_at: None,
        modified_by: None,
        child_count: Some(0),
        download_url: None,
    }
}

fn is_library(entry: &DriveEntry) -> bool {
    entry.drive_id == TEAM_LIBRARY || entry.drive_id == SITE_LIBRARY
}

/// The user's teams' libraries, then the sites of recently opened files that are not one of those teams.
pub fn library_rows(teams: &[(String, String)], sites: &[QuickSite]) -> Vec<DriveEntry> {
    let team_rows = teams
        .iter()
        .map(|(id, name)| library_row(TEAM_LIBRARY, id, name));
    let site_rows = sites
        .iter()
        .filter(|site| {
            !teams
                .iter()
                .any(|(_, name)| name.eq_ignore_ascii_case(&site.name))
        })
        .map(|site| library_row(SITE_LIBRARY, &site.id, &site.name));
    team_rows.chain(site_rows).collect()
}

/// A recently opened file as a library row; it has no drive ids when Graph only knows its link.
pub fn recent_entry(file: CloudFile, owner: Option<String>) -> DriveEntry {
    DriveEntry {
        drive_id: file.drive_id.unwrap_or_default(),
        id: file.item_id.unwrap_or_default(),
        name: file.name,
        web_url: file.web_url,
        size: file.size,
        modified_at: file.last_opened,
        modified_by: owner,
        child_count: None,
        download_url: None,
    }
}

/// Graph returns no link for the drive root, so it is the folder the first entry sits in.
pub fn root_web_url(entries: &[DriveEntry]) -> String {
    entries
        .first()
        .and_then(|entry| entry.web_url.rsplit_once('/'))
        .map(|(folder, _)| folder.to_owned())
        .unwrap_or_default()
}

type Loaded = (Option<Crumb>, Vec<DriveEntry>);

enum PreviewBody {
    Loading,
    Failed(String),
    Pages(Vec<Arc<RenderImage>>, usize),
    Text(String),
}

struct Preview {
    entry: DriveEntry,
    body: PreviewBody,
}

pub struct OneDrivePage {
    app: Entity<AppState>,
    section: Section,
    path: Option<LibraryPath>,
    listing: Listing,
    generation: u64,
    listed_at: Option<Instant>,
    loaded: bool,
    new_folder: Option<Entity<InputState>>,
    new_folder_open: bool,
    uploads_running: usize,
    downloading: Option<String>,
    preview: Option<Preview>,
    preview_generation: u64,
    demo: DemoLibrary,
    _subscriptions: Vec<Subscription>,
}

impl OneDrivePage {
    pub fn new(app: Entity<AppState>) -> Self {
        OneDrivePage {
            app,
            section: Section::Recent,
            path: None,
            listing: Listing::Loading,
            generation: 0,
            listed_at: None,
            loaded: false,
            new_folder: None,
            new_folder_open: false,
            uploads_running: 0,
            downloading: None,
            preview: None,
            preview_generation: 0,
            demo: DemoLibrary::default(),
            _subscriptions: Vec::new(),
        }
    }

    fn entry(&self, index: usize) -> Option<DriveEntry> {
        match &self.listing {
            Listing::Ready(entries) => entries.get(index).cloned(),
            _ => None,
        }
    }

    fn select(&mut self, section: Section, cx: &mut Context<Self>) {
        self.section = section;
        self.path = None;
        self.new_folder_open = false;
        self.load(cx);
    }

    fn run(&mut self, command: SharedCommand, window: &mut Window, cx: &mut Context<Self>) {
        match command {
            SharedCommand::Crumb(index) => {
                if let Some(path) = self.path.as_mut() {
                    path.go_to(index);
                }
                self.new_folder_open = false;
                self.load(cx);
            }
            SharedCommand::Open(index) => {
                let Some(entry) = self.entry(index) else {
                    return;
                };
                if !entry.is_folder() {
                    self.open_file(entry, cx);
                    return;
                }
                if is_library(&entry) {
                    self.open_library(entry, cx);
                    return;
                }
                match self.path.as_mut() {
                    Some(path) => path.enter(&entry),
                    None => {
                        self.path = Some(LibraryPath::new(Crumb {
                            name: entry.name.clone(),
                            folder: entry.folder(),
                            web_url: entry.web_url.clone(),
                        }))
                    }
                }
                self.new_folder_open = false;
                self.load(cx);
            }
            SharedCommand::OpenInBrowser(index) => {
                if let Some(entry) = self.entry(index).filter(|entry| !entry.web_url.is_empty()) {
                    cx.open_url(&entry.web_url);
                }
            }
            SharedCommand::Download(index) => {
                if let Some(entry) = self.entry(index) {
                    self.download(entry, cx);
                }
            }
            SharedCommand::CopyLink(index) => {
                if let Some(entry) = self.entry(index).filter(|entry| !entry.web_url.is_empty()) {
                    cx.write_to_clipboard(ClipboardItem::new_string(entry.web_url));
                    self.notice(tr("onedrive.link_copied"), cx);
                }
            }
            SharedCommand::Retry => self.load(cx),
            SharedCommand::ToggleNewFolder => self.toggle_new_folder(window, cx),
            SharedCommand::Upload => self.pick_upload(cx),
            SharedCommand::OpenSharePoint => {
                if let Some(path) = self
                    .path
                    .as_ref()
                    .filter(|path| !path.current_web_url().is_empty())
                {
                    cx.open_url(path.current_web_url());
                }
            }
            SharedCommand::Scope(_)
            | SharedCommand::Filter(_)
            | SharedCommand::OpenItem(_)
            | SharedCommand::JumpToPost(_) => {}
        }
    }

    fn notice(&self, text: String, cx: &mut Context<Self>) {
        self.app
            .update(cx, |state, cx| state.raise_notice(text, None, cx));
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        self.listing = Listing::Loading;
        cx.notify();
        let state = self.app.read(cx);
        let (demo, engine) = (state.mode.demo, state.engine.clone());
        let teams: Vec<(String, String)> = state
            .sidebar
            .teams
            .iter()
            .filter(|team| !team.hidden)
            .map(|team| (team.team.id.clone(), team.team.name.clone()))
            .collect();
        let (section, folder) = (
            self.section,
            self.path.as_ref().map(|path| path.current().clone()),
        );
        if demo {
            let outcome = Ok(self.demo_listing(section, folder));
            self.finish_load(generation, outcome, cx);
            return;
        }
        let Some(engine) = engine else {
            self.listing = Listing::Failed(tr("onedrive.not_connected"));
            return;
        };
        let my_files = tr("onedrive.my_files");
        let receiver = runtime::spawn(async move {
            Ok::<Loaded, teams_core::Error>(match (section, folder) {
                (_, Some(folder)) => (None, engine.library_children(&folder).await?),
                (Section::MyFiles, None) => {
                    let root = engine.picker_root(None).await?;
                    let entries = engine.library_children(&root).await?;
                    let crumb = Crumb {
                        name: my_files,
                        folder: root,
                        web_url: root_web_url(&entries),
                    };
                    (Some(crumb), entries)
                }
                (Section::Recent, None) => (
                    None,
                    engine
                        .picker_recent()
                        .await?
                        .into_iter()
                        .map(|(file, owner)| recent_entry(file, owner))
                        .collect(),
                ),
                (Section::Shared, None) => (None, engine.picker_shared().await?),
                (Section::Libraries, None) => {
                    let sites = engine.picker_sites().await.unwrap_or_default();
                    (None, library_rows(&teams, &sites))
                }
            })
        });
        self.finish_when(generation, receiver, cx);
    }

    fn finish_when<E: std::fmt::Display + 'static>(
        &self,
        generation: u64,
        receiver: impl Future<Output = Result<Result<Loaded, teams_core::Error>, E>> + 'static,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            let outcome = match receiver.await {
                Ok(Ok(loaded)) => Ok(loaded),
                Ok(Err(error)) => Err(short_error(&error)),
                Err(error) => Err(short_error(&error)),
            };
            this.update(cx, |this, cx| this.finish_load(generation, outcome, cx))
                .ok();
        })
        .detach();
    }

    fn open_library(&mut self, library: DriveEntry, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        self.listing = Listing::Loading;
        self.new_folder_open = false;
        cx.notify();
        let state = self.app.read(cx);
        let (demo, engine) = (state.mode.demo, state.engine.clone());
        if demo {
            let root = self.demo.root(&library.id, &library.name);
            let entries = self.demo.children(&root.id);
            let crumb = Crumb {
                name: library.name,
                folder: root.folder(),
                web_url: root.web_url,
            };
            self.finish_load(generation, Ok((Some(crumb), entries)), cx);
            return;
        }
        let Some(engine) = engine else {
            self.listing = Listing::Failed(tr("onedrive.not_connected"));
            return;
        };
        let receiver = runtime::spawn(async move {
            let root = if library.drive_id == TEAM_LIBRARY {
                engine.team_library_root(&library.id).await?
            } else {
                engine.picker_root(Some(&library.id)).await?
            };
            let entries = engine.library_children(&root).await?;
            let crumb = Crumb {
                name: library.name,
                folder: root,
                web_url: root_web_url(&entries),
            };
            Ok::<Loaded, teams_core::Error>((Some(crumb), entries))
        });
        self.finish_when(generation, receiver, cx);
    }

    fn demo_listing(&mut self, section: Section, folder: Option<DriveFolder>) -> Loaded {
        if let Some(folder) = folder {
            return (None, self.demo.children(&folder.item_id));
        }
        let root = self.demo.root(DEMO_DRIVE, &tr("onedrive.my_files"));
        let entries = self.demo.children(&root.id);
        match section {
            Section::MyFiles => {
                let crumb = Crumb {
                    name: root.name.clone(),
                    folder: root.folder(),
                    web_url: root.web_url,
                };
                (Some(crumb), entries)
            }
            Section::Recent => {
                let mut files: Vec<DriveEntry> = entries
                    .into_iter()
                    .filter(|entry| !entry.is_folder())
                    .collect();
                files.sort_by_key(|entry| std::cmp::Reverse(entry.modified_at));
                (None, files)
            }
            Section::Shared => (
                None,
                entries.into_iter().filter(DriveEntry::is_folder).collect(),
            ),
            Section::Libraries => {
                let teams: Vec<(String, String)> = DEMO_TEAMS
                    .iter()
                    .map(|(id, name)| ((*id).to_owned(), (*name).to_owned()))
                    .collect();
                (None, library_rows(&teams, &[]))
            }
        }
    }

    fn finish_load(
        &mut self,
        generation: u64,
        outcome: Result<Loaded, String>,
        cx: &mut Context<Self>,
    ) {
        if generation != self.generation {
            return;
        }
        match outcome {
            Ok((root, entries)) => {
                if let Some(root) = root {
                    self.path = Some(LibraryPath::new(root));
                }
                self.listing =
                    Listing::Ready(if self.section == Section::Recent && self.path.is_none() {
                        entries
                    } else {
                        sorted_entries(entries)
                    });
                self.listed_at = Some(Instant::now());
            }
            Err(reason) => self.listing = Listing::Failed(reason),
        }
        cx.notify();
    }

    fn download(&mut self, entry: DriveEntry, cx: &mut Context<Self>) {
        if entry.drive_id.is_empty() {
            cx.open_url(&entry.web_url);
            return;
        }
        let state = self.app.read(cx);
        let (demo, engine) = (state.mode.demo, state.engine.clone());
        let name = entry.name.clone();
        let receiver = if demo {
            let path = downloads::write_demo_file(&name).map_err(|error| short_error(&error));
            runtime::spawn(async move { path })
        } else {
            let Some(engine) = engine else {
                return;
            };
            let file = library_file(&entry, self.listed_at, Instant::now());
            let file_name = name.clone();
            runtime::spawn(async move {
                let mut part = PartFile::create(&downloads::downloads_directory(), &file_name)
                    .map_err(|error| short_error(&error))?;
                engine
                    .download_library_file(&file, |bytes| part.write(bytes), |_| {})
                    .await
                    .map_err(|error| short_error(&error))?;
                part.finish().map_err(|error| short_error(&error))
            })
        };
        self.downloading = Some(name.clone());
        cx.notify();
        cx.spawn(async move |this, cx| {
            let outcome = receiver
                .await
                .unwrap_or_else(|error| Err(short_error(&error)));
            this.update(cx, |this, cx| {
                this.downloading = None;
                match outcome {
                    Ok(path) => cx.open_with_system(&path),
                    Err(reason) => this.notice(format!("{name}: {reason}"), cx),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn toggle_new_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.new_folder_open {
            self.new_folder_open = false;
            cx.notify();
            return;
        }
        let input = match &self.new_folder {
            Some(input) => input.clone(),
            None => {
                let input = cx
                    .new(|cx| InputState::new(window, cx).placeholder(tr("onedrive.folder_name")));
                let subscription = cx.subscribe_in(&input, window, |this, _, event, _, cx| {
                    if matches!(event, InputEvent::PressEnter { .. }) {
                        this.create_folder(cx);
                    }
                });
                self._subscriptions.push(subscription);
                self.new_folder = Some(input.clone());
                input
            }
        };
        input.update(cx, |input, cx| {
            input.set_value("", window, cx);
            input.focus(window, cx);
        });
        self.new_folder_open = true;
        cx.notify();
    }

    fn create_folder(&mut self, cx: &mut Context<Self>) {
        let Some(input) = self.new_folder.clone() else {
            return;
        };
        let name = input.read(cx).value().trim().to_owned();
        let Some(parent) = self.path.as_ref().map(|path| path.current().clone()) else {
            return;
        };
        if name.is_empty() {
            return;
        }
        self.new_folder_open = false;
        let state = self.app.read(cx);
        let (demo, engine) = (state.mode.demo, state.engine.clone());
        if demo {
            self.demo.create_folder(&parent.item_id, &name);
            self.load(cx);
            return;
        }
        let Some(engine) = engine else {
            return;
        };
        let receiver =
            runtime::spawn(async move { engine.create_library_folder(&parent, &name).await });
        cx.spawn(async move |this, cx| {
            let outcome = match receiver.await {
                Ok(Ok(_)) => Ok(()),
                Ok(Err(error)) => Err(short_error(&error)),
                Err(error) => Err(short_error(&error)),
            };
            this.update(cx, |this, cx| {
                this.finish_change(outcome, tr("onedrive.folder_failed"), cx)
            })
            .ok();
        })
        .detach();
    }

    fn finish_change(
        &mut self,
        outcome: Result<(), String>,
        failure: String,
        cx: &mut Context<Self>,
    ) {
        if let Err(reason) = outcome {
            self.notice(format!("{failure}: {reason}"), cx);
        }
        self.load(cx);
    }

    fn pick_upload(&mut self, cx: &mut Context<Self>) {
        let receiver = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: None,
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = receiver.await {
                this.update(cx, |this, cx| this.upload_paths(paths, cx))
                    .ok();
            }
        })
        .detach();
    }

    fn upload_paths(&mut self, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        let Some(folder) = self.path.as_ref().map(|path| path.current().clone()) else {
            return;
        };
        let state = self.app.read(cx);
        let (demo, engine) = (state.mode.demo, state.engine.clone());
        for path in paths {
            self.uploads_running += 1;
            let (folder, engine) = (folder.clone(), engine.clone());
            cx.spawn(async move |this, cx| {
                let name = display_name(&path);
                let loaded = cx
                    .background_executor()
                    .spawn(async move { read_attachment(&path) })
                    .await;
                let outcome = match (loaded, engine) {
                    (Err(reason), _) => Err(reason),
                    (Ok(file), _) if demo => {
                        let (folder_id, file_name) = (folder.item_id.clone(), name.clone());
                        this.update(cx, |this, _| {
                            this.demo
                                .add_file(&folder_id, &file_name, file.bytes.len() as u64);
                        })
                        .ok();
                        Ok(())
                    }
                    (Ok(_), None) => Err(tr("onedrive.not_connected")),
                    (Ok(file), Some(engine)) => {
                        let file_name = name.clone();
                        let receiver = runtime::spawn(async move {
                            engine
                                .upload_to_library(&folder, &file_name, &file.bytes, |_| {})
                                .await
                        });
                        match receiver.await {
                            Ok(Ok(_)) => Ok(()),
                            Ok(Err(error)) => Err(short_error(&error)),
                            Err(error) => Err(short_error(&error)),
                        }
                    }
                };
                this.update(cx, |this, cx| {
                    this.uploads_running = this.uploads_running.saturating_sub(1);
                    if this.uploads_running == 0 || outcome.is_err() {
                        this.finish_change(
                            outcome,
                            format!("{name}: {}", tr("onedrive.upload_failed")),
                            cx,
                        );
                    } else {
                        cx.notify();
                    }
                })
                .ok();
            })
            .detach();
        }
        cx.notify();
    }

    fn open_file(&mut self, entry: DriveEntry, cx: &mut Context<Self>) {
        match preview_kind(&entry.name) {
            Some(PreviewKind::Media) => self.download(entry, cx),
            Some(kind) if !entry.drive_id.is_empty() && entry.size <= MAX_PREVIEW_BYTES => {
                self.show_preview(entry, kind, cx)
            }
            _ => cx.open_url(&entry.web_url),
        }
    }

    fn show_preview(&mut self, entry: DriveEntry, kind: PreviewKind, cx: &mut Context<Self>) {
        self.preview_generation += 1;
        let generation = self.preview_generation;
        let state = self.app.read(cx);
        let (demo, engine) = (state.mode.demo, state.engine.clone());
        let body = match (demo, engine) {
            (true, _) => PreviewBody::Failed(tr("onedrive.preview_demo")),
            (false, None) => PreviewBody::Failed(tr("onedrive.not_connected")),
            (false, Some(engine)) => {
                let file = library_file(&entry, self.listed_at, Instant::now());
                let (drive_id, item_id) = (entry.drive_id.clone(), entry.id.clone());
                let fetched = runtime::spawn(async move {
                    if kind == PreviewKind::Office {
                        return engine.library_file_as_pdf(&drive_id, &item_id).await;
                    }
                    let mut bytes = Vec::new();
                    engine
                        .download_library_file(
                            &file,
                            |chunk| {
                                bytes.extend_from_slice(chunk);
                                Ok(())
                            },
                            |_| {},
                        )
                        .await?;
                    Ok(bytes)
                });
                cx.spawn(async move |this, cx| {
                    let decoded = match fetched.await {
                        Ok(Ok(bytes)) => {
                            cx.background_executor()
                                .spawn(async move { file_preview::decode(kind, bytes) })
                                .await
                        }
                        Ok(Err(error)) => Err(short_error(&error)),
                        Err(error) => Err(short_error(&error)),
                    };
                    this.update(cx, |this, cx| this.finish_preview(generation, decoded, cx))
                        .ok();
                })
                .detach();
                PreviewBody::Loading
            }
        };
        self.preview = Some(Preview { entry, body });
        cx.notify();
    }

    fn finish_preview(
        &mut self,
        generation: u64,
        decoded: Result<Decoded, String>,
        cx: &mut Context<Self>,
    ) {
        let Some(preview) = self
            .preview
            .as_mut()
            .filter(|_| generation == self.preview_generation)
        else {
            return;
        };
        preview.body = match decoded {
            Ok(Decoded::Pages(pages, total)) => {
                PreviewBody::Pages(pages.into_iter().map(render_image).collect(), total)
            }
            Ok(Decoded::Text(text)) => PreviewBody::Text(text),
            Err(reason) => PreviewBody::Failed(reason),
        };
        cx.notify();
    }

    fn close_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(Preview {
            body: PreviewBody::Pages(pages, _),
            ..
        }) = self.preview.take()
        {
            for page in pages {
                window.drop_image(page).ok();
            }
        }
        self.preview_generation += 1;
        cx.notify();
    }

    fn render_preview(&self, preview: &Preview, cx: &mut Context<Self>) -> AnyElement {
        let entry = preview.entry.clone();
        let (download_entry, web_url) = (entry.clone(), entry.web_url.clone());
        let header = h_flex()
            .w_full()
            .flex_none()
            .px(px(12.))
            .py(px(8.))
            .gap(px(8.))
            .items_center()
            .border_b_1()
            .border_color(theme::border())
            .child(
                Button::new("onedrive-preview-back")
                    .ghost()
                    .compact()
                    .icon(IconName::ArrowLeft)
                    .label(tr("onedrive.back"))
                    .on_click(cx.listener(|this, _, window, cx| this.close_preview(window, cx))),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(13.5))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme::text())
                    .child(entry.name.clone()),
            )
            .child(
                Button::new("onedrive-preview-download")
                    .ghost()
                    .compact()
                    .icon(IconName::Download)
                    .label(tr("onedrive.download"))
                    .on_click(
                        cx.listener(move |this, _, _, cx| {
                            this.download(download_entry.clone(), cx)
                        }),
                    ),
            )
            .child(
                Button::new("onedrive-preview-browser")
                    .ghost()
                    .compact()
                    .icon(IconName::ExternalLink)
                    .label(tr("onedrive.open_web"))
                    .on_click(move |_, _, cx| cx.open_url(&web_url)),
            );
        let note = |text: String| {
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(13.))
                .text_color(theme::text_muted())
                .child(text)
                .into_any_element()
        };
        let body = match &preview.body {
            PreviewBody::Loading => note(tr("onedrive.preview_loading")),
            PreviewBody::Failed(reason) => {
                note(format!("{}: {reason}", tr("onedrive.preview_failed")))
            }
            PreviewBody::Pages(pages, total) => pages_view(
                pages,
                *total,
                format!("{} ({total})", tr("onedrive.preview_more_pages")),
            ),
            PreviewBody::Text(text) => text_view(text, cx),
        };
        v_flex()
            .size_full()
            .child(header)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .bg(theme::surface_raised())
                    .child(body),
            )
            .into_any_element()
    }

    fn section_switch(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut switch = h_flex()
            .flex_none()
            .p(px(2.))
            .gap(px(2.))
            .rounded(px(8.))
            .bg(theme::surface_raised());
        for section in Section::ALL {
            let active = section == self.section;
            switch = switch.child(
                div()
                    .id(ElementId::NamedInteger(
                        "onedrive-section".into(),
                        section as u64,
                    ))
                    .px(px(12.))
                    .py(px(4.))
                    .rounded(px(6.))
                    .cursor_pointer()
                    .text_size(px(12.5))
                    .font_weight(if active {
                        FontWeight::SEMIBOLD
                    } else {
                        FontWeight::NORMAL
                    })
                    .text_color(if active {
                        theme::text()
                    } else {
                        theme::text_muted()
                    })
                    .when(active, |segment| segment.bg(theme::border_strong()))
                    .on_click(cx.listener(move |this, _, _, cx| this.select(section, cx)))
                    .child(section.label()),
            );
        }
        switch.into_any_element()
    }
}

impl Render for OneDrivePage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.loaded {
            self.loaded = true;
            self.load(cx);
        }
        let view = cx.weak_entity();
        let run: SharedRun = Rc::new(move |command, window, cx| {
            view.update(cx, |this, cx| this.run(command, window, cx))
                .ok();
        });
        let status = match (&self.downloading, self.uploads_running) {
            (Some(name), _) => Some(format!("{} {name} …", tr("onedrive.downloading"))),
            (None, 0) => None,
            (None, running) => Some(format!("{} ({running}) …", tr("onedrive.uploading"))),
        };
        let web_label = format!("{} \u{2197}", tr("onedrive.open_web"));
        let empty = tr("onedrive.empty");
        if let Some(preview) = &self.preview {
            return div()
                .debug_selector(|| "shell/onedrive".into())
                .size_full()
                .bg(theme::surface())
                .child(self.render_preview(preview, cx));
        }
        div()
            .debug_selector(|| "shell/onedrive".into())
            .size_full()
            .bg(theme::surface())
            .child(render_library(
                LibraryView {
                    path: self.path.as_ref(),
                    listing: &self.listing,
                    new_folder: self.new_folder.as_ref().filter(|_| self.new_folder_open),
                    status: status.as_deref(),
                    download_states: Vec::new(),
                    lead: Some(self.section_switch(cx)),
                    web_label: &web_label,
                    empty: &empty,
                    hide_sizes: self.section == Section::Libraries && self.path.is_none(),
                },
                &run,
            ))
    }
}

#[cfg(test)]
mod tests {
    use teams_core::{CloudFile, DriveEntry, QuickSite};

    use super::{SITE_LIBRARY, TEAM_LIBRARY, library_rows, recent_entry, root_web_url};

    fn entry(web_url: &str) -> DriveEntry {
        recent_entry(
            CloudFile {
                name: "a.docx".to_owned(),
                size: 1,
                web_url: web_url.to_owned(),
                drive_id: None,
                item_id: None,
                last_opened: None,
            },
            Some("Mara".to_owned()),
        )
    }

    #[test]
    fn a_recent_file_without_ids_is_a_file_row_owned_by_its_owner() {
        let row = entry("https://x/a.docx");
        assert!(!row.is_folder());
        assert!(row.drive_id.is_empty());
        assert_eq!(row.modified_by.as_deref(), Some("Mara"));
    }

    #[test]
    fn the_root_link_is_the_folder_of_its_first_file() {
        let entries = [entry(
            "https://tenant-my.sharepoint.com/personal/me/Documents/a.docx",
        )];
        assert_eq!(
            root_web_url(&entries),
            "https://tenant-my.sharepoint.com/personal/me/Documents"
        );
        assert_eq!(root_web_url(&[]), "");
    }

    #[test]
    fn libraries_list_teams_then_sites_that_are_not_a_team() {
        let teams = [("t1".to_owned(), "Company".to_owned())];
        let sites = [
            QuickSite {
                id: "s1".to_owned(),
                name: "company".to_owned(),
            },
            QuickSite {
                id: "s2".to_owned(),
                name: "Marketing".to_owned(),
            },
        ];
        let rows = library_rows(&teams, &sites);
        let kinds: Vec<(&str, &str)> = rows
            .iter()
            .map(|row| (row.drive_id.as_str(), row.id.as_str()))
            .collect();
        assert_eq!(kinds, [(TEAM_LIBRARY, "t1"), (SITE_LIBRARY, "s2")]);
        assert!(rows.iter().all(DriveEntry::is_folder));
    }
}
