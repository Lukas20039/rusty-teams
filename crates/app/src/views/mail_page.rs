//! The Mail page: the signed-in user's Outlook mailbox through the tokens the Teams session already
//! has. Graph reads and files mail; Outlook REST sends it (see `graph::mail`).

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::Duration;

use app_ui_kit::render::blocks::{Block, layout_blocks};
use app_ui_kit::render::elements::{emoji_ranges, split_highlights};
use app_ui_kit::render::render_blocks;
use chrono::{Duration as Elapsed, Local, Utc};
use gpui_kit::assets::IconName;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{
    Input, InputEvent, InputState, RangeDecorationCollection, TextDecorationCollection, Textarea,
    TextareaMode, TextareaState,
};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use graph::{
    ARCHIVE, DELETED, Graph, MailAttachment, MailFolder, MailMessage, MailSummary, OutgoingMail,
    Response, split_addresses, split_html_thread, split_thread,
};
use teams_core::{Draft, MarkKind, SizeStep, Span, TypingStyle, html_to_spans};

use crate::app_state::AppState;
use crate::downloads::{self, PartFile};
use crate::format;
use crate::i18n::tr;
use crate::notice::short_error;
use crate::notify::NotificationCenter;
use crate::runtime;
use crate::theme::{self, ElevatedSurface as _};
use crate::views::draft_style::draft_style;
use crate::views::widgets::icon;
use gpui_kit::base::POPUP_PRIORITY;

const FOLDER_WIDTH: f32 = 220.;
const COLLAPSED_WIDTH: f32 = 48.;
const LIST_WIDTH: f32 = 340.;
const NOTIFIED_AT_ONCE: usize = 3;
const ROW_HEIGHT: f32 = 50.;
const MAX_REMOTE_IMAGE: usize = 15 * 1024 * 1024;
const IMAGE_MAX_WIDTH: f32 = 640.;
const ROW_LINE: f32 = 19.;
const POLL: Duration = Duration::from_secs(60);
const FIRST_POLL: Duration = Duration::from_secs(5);
const BODY_ROWS: (usize, usize) = (10, 30);

/// The inbox's unread count, for the rail badge.
#[derive(Default)]
pub struct MailUnread(pub usize);

impl Global for MailUnread {}

/// A mail a notification asked to open.
#[derive(Default)]
pub struct MailFocus(pub Option<String>);

impl Global for MailFocus {}

pub fn mail_unread(cx: &App) -> usize {
    cx.try_global::<MailUnread>().map_or(0, |unread| unread.0)
}

enum Load<T> {
    Idle,
    Loading,
    Failed(String),
    Ready(T),
}

struct Compose {
    /// The message answered or forwarded, with how; `None` writes a new mail.
    response: Option<(Response, MailSummary)>,
    to: Entity<InputState>,
    cc: Entity<InputState>,
    subject: Entity<InputState>,
    body: Entity<TextareaState>,
    draft: Draft,
    decorations: (
        TextDecorationCollection<TextareaMode>,
        RangeDecorationCollection<TextareaMode>,
    ),
    /// Formats switched at the caret for what is typed next.
    pending: Option<TypingStyle>,
    sending: bool,
    _body_events: Subscription,
}

impl Compose {
    fn refresh_style(&self, cx: &mut App) {
        let style = draft_style(&self.draft, cx.theme().mono_font_family.clone());
        self.decorations.0.set(style.text, cx);
        self.decorations.1.set(style.ranges, cx);
    }

    fn format(
        &mut self,
        on_text: impl FnOnce(&mut Draft, Range<usize>),
        on_caret: impl FnOnce(&mut TypingStyle, &[MarkKind]),
        cx: &mut App,
    ) {
        let selection = self.body.read(cx).selected_range();
        if selection.is_empty() {
            let at = selection.start;
            let mut pending = self
                .pending
                .take()
                .filter(|pending| pending.at == at)
                .unwrap_or(TypingStyle {
                    at,
                    ..TypingStyle::default()
                });
            on_caret(&mut pending, &self.draft.style_at(at));
            self.pending = (!pending.is_empty()).then_some(pending);
        } else {
            on_text(&mut self.draft, selection);
            self.draft.take_edits();
            self.refresh_style(cx);
        }
    }
}

pub struct MailPage {
    app: Entity<AppState>,
    folders: Vec<MailFolder>,
    /// `None` while search results are shown.
    folder: Option<String>,
    list: Load<Vec<MailSummary>>,
    list_generation: u64,
    open: Option<String>,
    message: Load<(MailMessage, Vec<MailAttachment>)>,
    message_generation: u64,
    /// Local copies of the open mail's images, by the `src` the HTML gives them.
    images: HashMap<String, PathBuf>,
    /// The open HTML mail laid out, newest part first.
    html_parts: Vec<Vec<Block>>,
    /// Remote images of the open mail left out until the user loads them.
    blocked_images: Vec<String>,
    /// The mail a right click opened the menu for, and where.
    row_menu: Option<(String, Point<Pixels>)>,
    compose: Option<Compose>,
    search: Option<Entity<InputState>>,
    folders_hidden: bool,
    notifications: Option<Entity<NotificationCenter>>,
    /// Inbox mail already known; `None` until the first check, which notifies nothing.
    // ponytail: lives with the page, so mail that arrives while the window sits in the tray is not announced.
    seen: Option<HashSet<String>>,
    _subscriptions: Vec<Subscription>,
}

fn time_label(summary: &MailSummary) -> String {
    summary.received.map_or_else(String::new, |time| {
        format::list_time_label(
            time,
            Local::now().date_naive(),
            app_base::format::Zone::Local,
        )
    })
}

fn sender(summary: &MailSummary) -> String {
    if summary.from_name.is_empty() {
        summary.from_address.clone()
    } else {
        summary.from_name.clone()
    }
}

impl MailPage {
    pub fn new(app: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let mut page = MailPage {
            app,
            folders: Vec::new(),
            folder: None,
            list: Load::Idle,
            list_generation: 0,
            open: None,
            message: Load::Idle,
            message_generation: 0,
            images: HashMap::new(),
            html_parts: Vec::new(),
            blocked_images: Vec::new(),
            row_menu: None,
            compose: None,
            search: None,
            folders_hidden: false,
            notifications: None,
            seen: None,
            _subscriptions: Vec::new(),
        };
        page.start_polling(cx);
        page
    }

    pub fn set_notifications(&mut self, center: Entity<NotificationCenter>) {
        self.notifications = Some(center);
    }

    fn demo(&self, cx: &App) -> bool {
        self.app.read(cx).mode.demo
    }

    fn graph(&self, cx: &App) -> Option<Graph> {
        let engine = self.app.read(cx).engine.clone()?;
        Some(Graph::new(engine.remote().session().clone()))
    }

    /// Runs a Graph call off the UI thread and hands its outcome back to the page.
    fn call<T, Work, Fut>(
        &mut self,
        cx: &mut Context<Self>,
        work: Work,
        done: impl FnOnce(&mut Self, Result<T, String>, &mut Context<Self>) + 'static,
    ) where
        T: Send + 'static,
        Work: FnOnce(Graph) -> Fut + Send + 'static,
        Fut: Future<Output = graph::Result<T>> + Send + 'static,
    {
        let Some(graph) = self.graph(cx) else {
            done(self, Err(tr("mail.not_connected")), cx);
            return;
        };
        let receiver = runtime::spawn(async move { work(graph).await });
        cx.spawn(async move |this, cx| {
            let outcome = match receiver.await {
                Ok(Ok(value)) => Ok(value),
                Ok(Err(error)) => Err(short_error(&error)),
                Err(error) => Err(short_error(&error)),
            };
            this.update(cx, |this, cx| done(this, outcome, cx)).ok();
        })
        .detach();
    }

    fn notice(&self, text: String, cx: &mut Context<Self>) {
        self.app
            .update(cx, |state, cx| state.raise_notice(text, None, cx));
    }

    fn start_polling(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(FIRST_POLL).await;
            loop {
                if this
                    .update(cx, |this, cx| this.refresh_folders(cx))
                    .is_err()
                {
                    break;
                }
                cx.background_executor().timer(POLL).await;
            }
        })
        .detach();
    }

    fn refresh_folders(&mut self, cx: &mut Context<Self>) {
        if self.demo(cx) {
            if self.folders.is_empty() {
                self.folders = demo::folders();
                self.set_unread(cx);
            }
            return;
        }
        if self.app.read(cx).engine.is_none() {
            return;
        }
        self.call(
            cx,
            |graph| async move { graph.mail_folders().await },
            |this, outcome, cx| {
                let Ok(folders) = outcome else {
                    return;
                };
                let inbox_before = this.folders.first().map(|folder| folder.unread);
                this.folders = folders;
                if this.seen.is_none()
                    || inbox_before != this.folders.first().map(|folder| folder.unread)
                {
                    this.check_inbox(cx);
                }
                this.set_unread(cx);
                cx.notify();
            },
        );
    }

    /// Announces unread inbox mail not seen before and refreshes the list when it shows the inbox.
    fn check_inbox(&mut self, cx: &mut Context<Self>) {
        let Some(inbox) = self.folders.first().map(|folder| folder.id.clone()) else {
            return;
        };
        let generation = self.list_generation;
        let query = inbox.clone();
        self.call(
            cx,
            move |graph| async move { graph.folder_mail(&query).await },
            move |this, outcome, cx| {
                let Ok(mail) = outcome else {
                    return;
                };
                let fresh: Vec<&MailSummary> = match &this.seen {
                    Some(seen) => mail
                        .iter()
                        .filter(|mail| !mail.is_read && !seen.contains(&mail.id))
                        .take(NOTIFIED_AT_ONCE)
                        .collect(),
                    None => Vec::new(),
                };
                if let Some(center) = this.notifications.clone() {
                    for mail in fresh {
                        let text = if mail.preview.is_empty() {
                            mail.subject.clone()
                        } else {
                            format!("{} – {}", mail.subject, mail.preview)
                        };
                        center.update(cx, |center, cx| {
                            center.present_mail(&mail.id, &sender(mail), &text, cx)
                        });
                    }
                }
                this.seen = Some(mail.iter().map(|mail| mail.id.clone()).collect());
                if this.folder.as_ref() == Some(&inbox) && generation == this.list_generation {
                    this.list = Load::Ready(mail);
                }
                cx.notify();
            },
        );
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.refresh_folders(cx);
        self.load_list(true, cx);
    }

    fn set_unread(&mut self, cx: &mut Context<Self>) {
        let unread = self.folders.first().map_or(0, |folder| folder.unread) as usize;
        if mail_unread(cx) != unread {
            cx.set_global(MailUnread(unread));
            cx.refresh_windows();
        }
    }

    fn ensure_started(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.search.is_none() {
            let search = cx.new(|cx| InputState::new(window, cx).placeholder(tr("mail.search")));
            let subscription = cx.subscribe_in(&search, window, |this, input, event, _, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    let query = input.read(cx).value().trim().to_owned();
                    this.search_mail(query, cx);
                }
            });
            self._subscriptions.push(subscription);
            self.search = Some(search);
        }
        if let Some(message_id) = cx
            .try_global::<MailFocus>()
            .and_then(|focus| focus.0.clone())
        {
            cx.set_global(MailFocus(None));
            if let Some(inbox) = self.folders.first().map(|folder| folder.id.clone())
                && self.folder.as_ref() != Some(&inbox)
            {
                self.select_folder(inbox, cx);
            }
            self.open_message(message_id, cx);
        }
        if self.folder.is_none() && matches!(self.list, Load::Idle) {
            if self.demo(cx) {
                self.folders = demo::folders();
                self.set_unread(cx);
            }
            match self.folders.first() {
                Some(inbox) => self.select_folder(inbox.id.clone(), cx),
                None => self.load_inbox_first(cx),
            }
        }
    }

    fn load_inbox_first(&mut self, cx: &mut Context<Self>) {
        self.list = Load::Loading;
        self.call(
            cx,
            |graph| async move { graph.mail_folders().await },
            |this, outcome, cx| match outcome {
                Ok(folders) => {
                    this.folders = folders;
                    this.set_unread(cx);
                    match this.folders.first() {
                        Some(inbox) => this.select_folder(inbox.id.clone(), cx),
                        None => this.list = Load::Ready(Vec::new()),
                    }
                    cx.notify();
                }
                Err(reason) => {
                    this.list = Load::Failed(reason);
                    cx.notify();
                }
            },
        );
    }

    fn select_folder(&mut self, folder_id: String, cx: &mut Context<Self>) {
        self.folder = Some(folder_id);
        self.load_list(false, cx);
    }

    fn load_list(&mut self, silent: bool, cx: &mut Context<Self>) {
        let Some(folder_id) = self.folder.clone() else {
            return;
        };
        self.list_generation += 1;
        let generation = self.list_generation;
        if !silent {
            self.list = Load::Loading;
            cx.notify();
        }
        if self.demo(cx) {
            self.list = Load::Ready(demo::messages(&folder_id));
            return;
        }
        self.call(
            cx,
            move |graph| async move { graph.folder_mail(&folder_id).await },
            move |this, outcome, cx| this.finish_list(generation, outcome, cx),
        );
    }

    fn search_mail(&mut self, query: String, cx: &mut Context<Self>) {
        if query.is_empty() {
            if let Some(inbox) = self.folders.first() {
                self.select_folder(inbox.id.clone(), cx);
            }
            return;
        }
        self.folder = None;
        self.list_generation += 1;
        let generation = self.list_generation;
        self.list = Load::Loading;
        cx.notify();
        if self.demo(cx) {
            let needle = query.to_lowercase();
            let found = demo::messages(demo::INBOX)
                .into_iter()
                .filter(|mail| {
                    format!("{} {} {}", mail.subject, mail.from_name, mail.preview)
                        .to_lowercase()
                        .contains(&needle)
                })
                .collect();
            self.list = Load::Ready(found);
            return;
        }
        self.call(
            cx,
            move |graph| async move { graph.search_mail(&query).await },
            move |this, outcome, cx| this.finish_list(generation, outcome, cx),
        );
    }

    fn finish_list(
        &mut self,
        generation: u64,
        outcome: Result<Vec<MailSummary>, String>,
        cx: &mut Context<Self>,
    ) {
        if generation != self.list_generation {
            return;
        }
        self.list = match outcome {
            Ok(mail) => Load::Ready(mail),
            Err(reason) => Load::Failed(reason),
        };
        cx.notify();
    }

    fn summary(&self, message_id: &str) -> Option<MailSummary> {
        match &self.list {
            Load::Ready(mail) => mail.iter().find(|mail| mail.id == message_id).cloned(),
            _ => None,
        }
    }

    fn set_read_locally(&mut self, message_id: &str, read: bool) {
        if let Load::Ready(mail) = &mut self.list
            && let Some(summary) = mail.iter_mut().find(|mail| mail.id == message_id)
            && summary.is_read != read
        {
            summary.is_read = read;
            if let Some(folder) = self
                .folder
                .as_ref()
                .and_then(|id| self.folders.iter_mut().find(|folder| &folder.id == id))
            {
                folder.unread = if read {
                    folder.unread.saturating_sub(1)
                } else {
                    folder.unread + 1
                };
            }
        }
    }

    fn open_message(&mut self, message_id: String, cx: &mut Context<Self>) {
        self.compose = None;
        self.open = Some(message_id.clone());
        self.message_generation += 1;
        let generation = self.message_generation;
        self.message = Load::Loading;
        self.images.clear();
        self.html_parts.clear();
        self.blocked_images.clear();
        let unread = self.summary(&message_id).is_none_or(|mail| !mail.is_read);
        self.set_read_locally(&message_id, true);
        self.set_unread(cx);
        cx.notify();
        if self.demo(cx) {
            self.message = match self.summary(&message_id) {
                Some(summary) => Load::Ready((demo::message(summary), Vec::new())),
                None => Load::Failed(tr("mail.not_found")),
            };
            return;
        }
        self.call(
            cx,
            move |graph| async move {
                let message = graph.mail_message(&message_id).await?;
                let attachments =
                    if message.summary.has_attachments || message.html.contains("cid:") {
                        graph.mail_attachments(&message_id).await?
                    } else {
                        Vec::new()
                    };
                let mut images = HashMap::new();
                for attachment in &attachments {
                    let Some(content_id) = attachment.content_id.as_deref() else {
                        continue;
                    };
                    let source = format!("cid:{content_id}");
                    if !message.html.contains(&source) {
                        continue;
                    }
                    let bytes = graph
                        .mail_attachment_bytes(&message_id, &attachment.id)
                        .await?;
                    if let Some(path) = store_image(&format!("{message_id}{source}"), &bytes) {
                        images.insert(source, path);
                    }
                }
                if unread {
                    graph.set_mail_read(&message_id, true).await?;
                }
                Ok(((message, attachments), images))
            },
            move |this, outcome, cx| {
                if generation != this.message_generation {
                    return;
                }
                this.message = match outcome {
                    Ok((loaded, images)) => {
                        this.images = images;
                        this.html_parts = Vec::new();
                        Load::Ready(loaded)
                    }
                    Err(reason) => Load::Failed(reason),
                };
                this.lay_out_html();
                cx.notify();
            },
        );
    }

    fn lay_out_html(&mut self) {
        let Load::Ready((message, _)) = &self.message else {
            return;
        };
        let mut blocked = Vec::new();
        self.html_parts = split_html_thread(&message.html)
            .into_iter()
            .map(|part| {
                let spans = mail_spans(html_to_spans(part), &self.images, &mut blocked);
                let mut blocks = layout_blocks(&spans);
                while matches!(blocks.last(), Some(Block::Rule)) {
                    blocks.pop();
                }
                blocks
            })
            .filter(|blocks| !blocks.is_empty())
            .collect();
        blocked.sort();
        blocked.dedup();
        self.blocked_images = blocked;
    }

    /// Fetches the remote images without the Teams session, so the sender learns only that the mail was opened.
    fn load_remote_images(&mut self, cx: &mut Context<Self>) {
        let urls = std::mem::take(&mut self.blocked_images);
        let generation = self.message_generation;
        let receiver = runtime::spawn(async move {
            let Ok(client) = reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .build()
            else {
                return Vec::new();
            };
            let mut stored = Vec::new();
            for url in urls {
                let Ok(response) = client.get(&url).send().await else {
                    continue;
                };
                if !response.status().is_success() {
                    continue;
                }
                if let Ok(bytes) = response.bytes().await
                    && bytes.len() <= MAX_REMOTE_IMAGE
                    && let Some(path) = store_image(&url, &bytes)
                {
                    stored.push((url, path));
                }
            }
            stored
        });
        cx.spawn(async move |this, cx| {
            let Ok(stored) = receiver.await else {
                return;
            };
            this.update(cx, |this, cx| {
                if generation != this.message_generation {
                    return;
                }
                this.images.extend(stored);
                this.lay_out_html();
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn file_away(&mut self, message_id: String, destination: &'static str, cx: &mut Context<Self>) {
        let id = message_id.clone();
        let remove = move |this: &mut Self, cx: &mut Context<Self>| {
            if let Load::Ready(mail) = &mut this.list {
                mail.retain(|mail| mail.id != message_id);
            }
            if this.open.as_ref() == Some(&message_id) {
                this.open = None;
                this.message = Load::Idle;
            }
            cx.notify();
        };
        if self.demo(cx) {
            remove(self, cx);
            return;
        }
        self.call(
            cx,
            move |graph| async move { graph.move_mail(&id, destination).await },
            move |this, outcome, cx| match outcome {
                Ok(()) => remove(this, cx),
                Err(reason) => this.notice(reason, cx),
            },
        );
    }

    fn set_read(&mut self, message_id: String, read: bool, cx: &mut Context<Self>) {
        self.set_read_locally(&message_id, read);
        self.set_unread(cx);
        if !read && self.open.as_ref() == Some(&message_id) {
            self.open = None;
            self.message = Load::Idle;
        }
        cx.notify();
        if self.demo(cx) {
            return;
        }
        self.call(
            cx,
            move |graph| async move { graph.set_mail_read(&message_id, read).await },
            |this, outcome, cx| {
                if let Err(reason) = outcome {
                    this.notice(reason, cx);
                }
            },
        );
    }

    fn open_attachment(
        &mut self,
        message_id: String,
        attachment: MailAttachment,
        cx: &mut Context<Self>,
    ) {
        if self.demo(cx) {
            self.notice(tr("mail.demo_attachment"), cx);
            return;
        }
        let name = attachment.name.clone();
        self.call(
            cx,
            move |graph| async move {
                let bytes = graph
                    .mail_attachment_bytes(&message_id, &attachment.id)
                    .await?;
                let mut part =
                    PartFile::create(&downloads::downloads_directory(), &attachment.name)
                        .map_err(|error| graph::Error::Download(error.to_string()))?;
                part.write(&bytes)
                    .map_err(|error| graph::Error::Download(error.to_string()))?;
                part.finish()
                    .map_err(|error| graph::Error::Download(error.to_string()))
            },
            move |this, outcome, cx| match outcome {
                Ok(path) => cx.open_with_system(&path),
                Err(reason) => this.notice(format!("{name}: {reason}"), cx),
            },
        );
    }

    fn start_compose(
        &mut self,
        response: Option<(Response, MailSummary)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let field = |placeholder: String, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
        };
        let body = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(BODY_ROWS.0, BODY_ROWS.1)
                .placeholder(tr("mail.body"))
        });
        let decorations = body.update(cx, |state, cx| {
            (
                state.create_decorations_collection(Vec::new(), cx),
                state.create_range_decorations_collection(Vec::new(), cx),
            )
        });
        let body_events = cx.subscribe_in(&body, window, Self::on_body_event);
        let compose = Compose {
            to: field(tr("mail.to"), window, cx),
            cc: field(tr("mail.cc"), window, cx),
            subject: field(tr("mail.subject"), window, cx),
            body,
            draft: Draft::default(),
            decorations,
            pending: None,
            response,
            sending: false,
            _body_events: body_events,
        };
        let focus = match compose.response {
            Some((Response::Reply | Response::ReplyAll, _)) => None,
            _ => Some(compose.to.clone()),
        };
        match focus {
            Some(to) => to.update(cx, |input, cx| input.focus(window, cx)),
            None => compose.body.update(cx, |input, cx| input.focus(window, cx)),
        }
        self.compose = Some(compose);
        cx.notify();
    }

    /// Keeps the draft, which carries the formatting, in step with what is typed.
    fn on_body_event(
        &mut self,
        input: &Entity<TextareaState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(compose) = self
            .compose
            .as_mut()
            .filter(|compose| matches!(event, InputEvent::Change) && &compose.body == input)
        else {
            return;
        };
        let state = input.read(cx);
        let (value, cursor) = (state.value().to_string(), state.cursor());
        if value == compose.draft.text() {
            return;
        }
        let typing = compose.pending.take();
        let cursor = compose.draft.apply_edit(&value, cursor, typing.as_ref());
        let edits = compose.draft.take_edits();
        if !edits.is_empty() {
            input.update(cx, |state, cx| {
                state.apply_edits(&edits, window, cx);
                state.set_selected_range(cursor..cursor, cx);
            });
            let value = input.read(cx).value().to_string();
            if value != compose.draft.text() {
                compose.draft.apply_edit(&value, cursor, None);
                compose.draft.take_edits();
            }
        }
        compose.refresh_style(cx);
    }

    fn format(
        &mut self,
        on_text: impl FnOnce(&mut Draft, Range<usize>),
        on_caret: impl FnOnce(&mut TypingStyle, &[MarkKind]),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(compose) = self.compose.as_mut() {
            compose.format(on_text, on_caret, cx);
            compose.body.update(cx, |input, cx| input.focus(window, cx));
            cx.notify();
        }
    }

    fn send(&mut self, cx: &mut Context<Self>) {
        let Some(compose) = self.compose.as_mut().filter(|compose| !compose.sending) else {
            return;
        };
        let to = split_addresses(&compose.to.read(cx).value());
        let cc = split_addresses(&compose.cc.read(cx).value());
        let subject = compose.subject.read(cx).value().to_string();
        let body = compose.draft.trimmed().to_html();
        let response = compose.response.clone();
        let needs_recipient = !matches!(response, Some((Response::Reply | Response::ReplyAll, _)));
        if needs_recipient && to.is_empty() {
            self.notice(tr("mail.no_recipient"), cx);
            return;
        }
        compose.sending = true;
        cx.notify();
        let done = |this: &mut Self, outcome: Result<(), String>, cx: &mut Context<Self>| {
            match outcome {
                Ok(()) => {
                    this.compose = None;
                    this.notice(tr("mail.sent"), cx);
                }
                Err(reason) => {
                    if let Some(compose) = this.compose.as_mut() {
                        compose.sending = false;
                    }
                    this.notice(format!("{}: {reason}", tr("mail.not_sent")), cx);
                }
            }
            cx.notify();
        };
        if self.demo(cx) {
            done(self, Ok(()), cx);
            return;
        }
        self.call(
            cx,
            move |graph| async move {
                match response {
                    Some((kind, mail)) => graph.respond_to_mail(&mail.id, kind, &body, &to).await,
                    None => {
                        graph
                            .send_mail(&OutgoingMail {
                                to,
                                cc,
                                subject,
                                body,
                            })
                            .await
                    }
                }
            },
            done,
        );
    }

    fn new_mail_button(&self, cx: &mut Context<Self>) -> Button {
        Button::new("mail-new")
            .primary()
            .icon(IconName::Pencil)
            .on_click(cx.listener(|this, _, window, cx| this.start_compose(None, window, cx)))
    }

    fn render_folders(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.folders_hidden {
            return v_flex()
                .w(px(COLLAPSED_WIDTH))
                .flex_none()
                .h_full()
                .py(px(10.))
                .items_center()
                .border_r_1()
                .border_color(theme::border())
                .child(self.new_mail_button(cx).tooltip(tr("mail.new")))
                .into_any_element();
        }
        let mut column = v_flex()
            .w(px(FOLDER_WIDTH))
            .flex_none()
            .h_full()
            .p(px(10.))
            .gap(px(2.))
            .border_r_1()
            .border_color(theme::border())
            .child(self.new_mail_button(cx).label(tr("mail.new")))
            .children(self.search.as_ref().map(|search| {
                div()
                    .my(px(6.))
                    .h(px(30.))
                    .px(px(2.))
                    .flex()
                    .items_center()
                    .rounded(px(8.))
                    .bg(theme::surface_raised())
                    .child(Input::new(search).appearance(false).bordered(false))
            }));
        for folder in &self.folders {
            let active = self.folder.as_deref() == Some(folder.id.as_str());
            let id = folder.id.clone();
            column = column.child(
                h_flex()
                    .id(SharedString::from(format!("mail-folder-{}", folder.id)))
                    .px(px(10.))
                    .py(px(5.))
                    .rounded(px(6.))
                    .cursor_pointer()
                    .text_size(px(13.))
                    .text_color(theme::text())
                    .when(active, |row| row.bg(theme::border_strong()))
                    .hover(|row| row.bg(theme::row_hover()))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.compose = None;
                        this.select_folder(id.clone(), cx)
                    }))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(folder.name.clone()),
                    )
                    .when(folder.unread > 0, |row| {
                        row.child(
                            div()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(theme::accent_text())
                                .child(folder.unread.to_string()),
                        )
                    }),
            );
        }
        column.into_any_element()
    }

    fn render_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let title = match &self.folder {
            Some(id) => self
                .folders
                .iter()
                .find(|folder| &folder.id == id)
                .map_or_else(String::new, |folder| folder.name.clone()),
            None => tr("mail.results"),
        };
        let body = match &self.list {
            Load::Idle | Load::Loading => centered(tr("mail.loading")),
            Load::Failed(reason) => centered(reason.clone()),
            Load::Ready(mail) if mail.is_empty() => centered(tr("mail.empty")),
            Load::Ready(mail) => v_flex()
                .id("mail-rows")
                .size_full()
                .min_h_0()
                .children(mail.iter().map(|summary| self.render_row(summary, cx)))
                .overflow_y_scrollbar()
                .into_any_element(),
        };
        v_flex()
            .w(px(LIST_WIDTH))
            .flex_none()
            .h_full()
            .border_r_1()
            .border_color(theme::border())
            .child(
                h_flex()
                    .px(px(8.))
                    .py(px(6.))
                    .gap(px(4.))
                    .items_center()
                    .border_b_1()
                    .border_color(theme::border())
                    .child(
                        Button::new("mail-folders")
                            .ghost()
                            .compact()
                            .icon(IconName::PanelLeft)
                            .tooltip(tr("mail.folders"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.folders_hidden = !this.folders_hidden;
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(14.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme::text())
                            .child(title),
                    )
                    .child(
                        Button::new("mail-refresh")
                            .ghost()
                            .compact()
                            .icon(IconName::RefreshCw)
                            .tooltip(tr("mail.refresh"))
                            .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                    ),
            )
            .child(div().flex_1().min_h_0().child(body))
    }

    fn render_row(&self, summary: &MailSummary, cx: &mut Context<Self>) -> AnyElement {
        let id = summary.id.clone();
        let selected = self.open.as_deref() == Some(summary.id.as_str());
        let weight = if summary.is_read {
            FontWeight::NORMAL
        } else {
            FontWeight::SEMIBOLD
        };
        v_flex()
            .id(SharedString::from(format!("mail-row-{}", summary.id)))
            .w_full()
            .flex_none()
            .h(px(ROW_HEIGHT))
            .overflow_hidden()
            .justify_center()
            .px(px(14.))
            .cursor_pointer()
            .border_l_2()
            .border_color(if summary.is_read {
                transparent_black()
            } else {
                theme::accent()
            })
            .when(selected, |row| row.bg(theme::border_strong()))
            .hover(|row| row.bg(theme::row_hover()))
            .on_click({
                let id = id.clone();
                cx.listener(move |this, _, _, cx| this.open_message(id.clone(), cx))
            })
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    this.row_menu = Some((id.clone(), event.position));
                    cx.stop_propagation();
                    cx.notify();
                }),
            )
            .child(
                h_flex()
                    .h(px(ROW_LINE))
                    .overflow_hidden()
                    .gap(px(8.))
                    .text_size(px(13.))
                    .line_height(px(ROW_LINE))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_weight(weight)
                            .text_color(theme::text())
                            .child(one_line(&sender(summary))),
                    )
                    .when(summary.has_attachments, |row| {
                        row.child(icon(IconName::Paperclip, 12., theme::text_faint()))
                    })
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(11.5))
                            .text_color(theme::text_faint())
                            .child(time_label(summary)),
                    ),
            )
            .child(
                h_flex()
                    .h(px(ROW_LINE))
                    .overflow_hidden()
                    .gap(px(6.))
                    .text_size(px(12.))
                    .line_height(px(ROW_LINE))
                    .child(
                        div()
                            .flex_none()
                            .max_w(px(LIST_WIDTH * 0.6))
                            .truncate()
                            .font_weight(weight)
                            .text_color(if summary.is_read {
                                theme::text_muted()
                            } else {
                                theme::accent_text()
                            })
                            .child(one_line(&summary.subject)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(theme::text_faint())
                            .child(one_line(&summary.preview)),
                    ),
            )
            .into_any_element()
    }

    fn render_row_menu(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (id, position) = self.row_menu.clone()?;
        let read = self.summary(&id)?.is_read;
        let item = |key: &'static str, label: String, glyph: IconName| {
            h_flex()
                .id(key)
                .px(px(10.))
                .py(px(6.))
                .gap(px(8.))
                .items_center()
                .rounded(px(4.))
                .cursor_pointer()
                .text_size(px(13.))
                .text_color(theme::text())
                .hover(|row| row.bg(theme::popup_hover()))
                .child(icon(glyph, 14., theme::text_muted()))
                .child(label)
        };
        let menu = v_flex()
            .id("mail-row-menu")
            .occlude()
            .min_w(px(190.))
            .p(px(4.))
            .rounded(px(8.))
            .flyout_surface()
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.row_menu = None;
                cx.notify();
            }))
            .child(
                item(
                    "mail-menu-read",
                    tr(if read {
                        "mail.mark_unread"
                    } else {
                        "mail.mark_read"
                    }),
                    if read {
                        IconName::Mail
                    } else {
                        IconName::MailOpen
                    },
                )
                .on_click(cx.listener({
                    let id = id.clone();
                    move |this, _, _, cx| {
                        this.row_menu = None;
                        this.set_read(id.clone(), !read, cx);
                    }
                })),
            )
            .child(
                item("mail-menu-archive", tr("mail.archive"), IconName::Folder).on_click(
                    cx.listener({
                        let id = id.clone();
                        move |this, _, _, cx| {
                            this.row_menu = None;
                            this.file_away(id.clone(), ARCHIVE, cx);
                        }
                    }),
                ),
            )
            .child(
                item("mail-menu-delete", tr("mail.delete"), IconName::Trash).on_click(cx.listener(
                    move |this, _, _, cx| {
                        this.row_menu = None;
                        this.file_away(id.clone(), DELETED, cx);
                    },
                )),
            );
        Some(
            deferred(anchored().position(position).snap_to_window().child(menu))
                .with_priority(POPUP_PRIORITY)
                .into_any_element(),
        )
    }

    fn render_message(&self, cx: &mut Context<Self>) -> AnyElement {
        let (message, attachments) = match &self.message {
            Load::Idle => return centered(tr("mail.pick")),
            Load::Loading => return centered(tr("mail.loading")),
            Load::Failed(reason) => return centered(reason.clone()),
            Load::Ready(loaded) => loaded,
        };
        let summary = message.summary.clone();
        let id = summary.id.clone();
        let action = |key: &'static str, tooltip: String, glyph: IconName| {
            Button::new(key)
                .ghost()
                .compact()
                .icon(glyph)
                .tooltip(tooltip)
        };
        let respond = |kind: Response, summary: MailSummary| {
            cx.listener(move |this: &mut Self, _, window, cx| {
                this.start_compose(Some((kind, summary.clone())), window, cx)
            })
        };
        let actions = h_flex()
            .flex_none()
            .gap(px(2.))
            .child(
                action("mail-reply", tr("mail.reply"), IconName::Reply)
                    .on_click(respond(Response::Reply, summary.clone())),
            )
            .child(
                action("mail-reply-all", tr("mail.reply_all"), IconName::Users)
                    .on_click(respond(Response::ReplyAll, summary.clone())),
            )
            .child(
                action("mail-forward", tr("mail.forward"), IconName::Forward)
                    .on_click(respond(Response::Forward, summary.clone())),
            )
            .child(
                action("mail-archive", tr("mail.archive"), IconName::Folder).on_click(cx.listener(
                    {
                        let id = id.clone();
                        move |this, _, _, cx| this.file_away(id.clone(), ARCHIVE, cx)
                    },
                )),
            )
            .child(
                action("mail-delete", tr("mail.delete"), IconName::Trash).on_click(cx.listener({
                    let id = id.clone();
                    move |this, _, _, cx| this.file_away(id.clone(), DELETED, cx)
                })),
            )
            .child(
                action("mail-unread", tr("mail.mark_unread"), IconName::Mail).on_click(
                    cx.listener({
                        let id = id.clone();
                        move |this, _, _, cx| this.set_read(id.clone(), false, cx)
                    }),
                ),
            );
        let recipients = [(tr("mail.to"), &message.to), (tr("mail.cc"), &message.cc)]
            .into_iter()
            .filter(|(_, list)| !list.is_empty())
            .map(|(label, list)| format!("{label}: {}", list.join(", ")))
            .collect::<Vec<_>>()
            .join("   ");
        let visible: Vec<MailAttachment> = attachments
            .iter()
            .filter(|attachment| !attachment.inline)
            .cloned()
            .collect();
        let mut parts = split_thread(&message.body).into_iter();
        let newest = parts.next().unwrap_or_default();
        let header = v_flex()
            .flex_none()
            .px(px(20.))
            .pt(px(10.))
            .pb(px(8.))
            .gap(px(2.))
            .border_b_1()
            .border_color(theme::border())
            .child(
                h_flex()
                    .gap(px(8.))
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(15.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme::text())
                            .child(summary.subject.clone()),
                    )
                    .child(actions),
            )
            .child(
                h_flex()
                    .gap(px(8.))
                    .text_size(px(12.5))
                    .child(
                        div()
                            .flex_none()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme::text())
                            .child(sender(&summary)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(theme::text_faint())
                            .child(summary.from_address.clone()),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(11.5))
                            .text_color(theme::text_faint())
                            .child(time_label(&summary)),
                    ),
            )
            .when(!recipients.is_empty(), |header| {
                header.child(
                    div()
                        .truncate()
                        .text_size(px(11.5))
                        .text_color(theme::text_muted())
                        .child(recipients),
                )
            })
            .when(!visible.is_empty(), |header| {
                header.child(h_flex().pt(px(4.)).flex_wrap().gap(px(6.)).children(
                    visible.into_iter().enumerate().map(|(index, attachment)| {
                        let message_id = id.clone();
                        Button::new(ElementId::NamedInteger(
                            "mail-attachment".into(),
                            index as u64,
                        ))
                        .outline()
                        .compact()
                        .icon(IconName::Paperclip)
                        .label(attachment.name.clone())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.open_attachment(message_id.clone(), attachment.clone(), cx)
                        }))
                    }),
                ))
            });
        let card = || {
            v_flex()
                .mt(px(10.))
                .px(px(12.))
                .py(px(8.))
                .gap(px(4.))
                .rounded(px(8.))
                .border_l_2()
                .border_color(theme::border_strong())
                .bg(theme::surface_raised())
        };
        let mut content: Vec<AnyElement> = Vec::new();
        if self.html_parts.is_empty() {
            content.push(mail_text(newest.body, theme::text()).into_any_element());
            content.extend(parts.map(|part| {
                card()
                    .child(
                        h_flex()
                            .gap(px(8.))
                            .text_size(px(12.))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(theme::text_soft())
                                    .child(part.from),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .text_color(theme::text_faint())
                                    .child(part.sent),
                            ),
                    )
                    .child(mail_text(part.body, theme::text_muted()))
                    .into_any_element()
            }));
        } else {
            if !self.blocked_images.is_empty() {
                content.push(
                    h_flex()
                        .mb(px(10.))
                        .px(px(10.))
                        .py(px(6.))
                        .gap(px(8.))
                        .items_center()
                        .rounded(px(6.))
                        .bg(theme::surface_raised())
                        .text_size(px(12.))
                        .text_color(theme::text_muted())
                        .child(icon(IconName::Image, 14., theme::text_muted()))
                        .child(div().flex_1().child(tr("mail.images_blocked")))
                        .child(
                            Button::new("mail-load-images")
                                .outline()
                                .compact()
                                .label(tr("mail.load_images"))
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.load_remote_images(cx)),
                                ),
                        )
                        .into_any_element(),
                );
            }
            for (index, blocks) in self.html_parts.iter().enumerate() {
                let rendered = mail_blocks(blocks, &format!("mail-html-{index}"), cx);
                content.push(if index == 0 {
                    rendered
                } else {
                    card().child(rendered).into_any_element()
                });
            }
        }
        v_flex()
            .size_full()
            .child(header)
            .child(
                v_flex()
                    .id("mail-body")
                    .flex_1()
                    .min_h_0()
                    .px(px(20.))
                    .py(px(12.))
                    .text_size(px(13.))
                    .children(content)
                    .overflow_y_scrollbar(),
            )
            .into_any_element()
    }

    fn render_compose(&self, compose: &Compose, cx: &mut Context<Self>) -> AnyElement {
        let title = match &compose.response {
            None => tr("mail.new"),
            Some((Response::Reply, mail)) => format!("{} {}", tr("mail.reply_to"), sender(mail)),
            Some((Response::ReplyAll, mail)) => {
                format!("{} {}", tr("mail.reply_all_to"), sender(mail))
            }
            Some((Response::Forward, mail)) => format!("{}: {}", tr("mail.forward"), mail.subject),
        };
        let replying = matches!(
            compose.response,
            Some((Response::Reply | Response::ReplyAll, _))
        );
        let new_mail = compose.response.is_none();
        let row = |label: String, input: &Entity<InputState>| {
            h_flex()
                .gap(px(8.))
                .items_center()
                .border_b_1()
                .border_color(theme::border())
                .py(px(4.))
                .child(
                    div()
                        .w(px(70.))
                        .text_size(px(12.5))
                        .text_color(theme::text_muted())
                        .child(label),
                )
                .child(
                    div()
                        .flex_1()
                        .child(Input::new(input).appearance(false).bordered(false)),
                )
        };
        v_flex()
            .size_full()
            .px(px(24.))
            .py(px(16.))
            .gap(px(6.))
            .child(
                h_flex()
                    .gap(px(8.))
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(16.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme::text())
                            .child(title),
                    )
                    .child(
                        Button::new("mail-discard")
                            .ghost()
                            .label(tr("mail.discard"))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.compose = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("mail-send")
                            .primary()
                            .icon(IconName::Send)
                            .label(if compose.sending {
                                tr("mail.sending")
                            } else {
                                tr("mail.send")
                            })
                            .on_click(cx.listener(|this, _, _, cx| this.send(cx))),
                    ),
            )
            .when(!replying, |form| {
                form.child(row(tr("mail.to"), &compose.to))
            })
            .when(new_mail, |form| {
                form.child(row(tr("mail.cc"), &compose.cc))
                    .child(row(tr("mail.subject"), &compose.subject))
            })
            .when(!new_mail, |form| {
                form.child(
                    div()
                        .text_size(px(12.))
                        .text_color(theme::text_faint())
                        .child(tr("mail.quoted_below")),
                )
            })
            .child(self.render_format_bar(cx))
            .child(
                div()
                    .id("mail-compose-body")
                    .flex_1()
                    .min_h_0()
                    .p(px(8.))
                    .rounded(px(8.))
                    .bg(theme::surface_raised())
                    .child(
                        Textarea::new(&compose.body)
                            .appearance(false)
                            .bordered(false),
                    )
                    .overflow_y_scrollbar(),
            )
            .into_any_element()
    }

    fn render_format_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let button = |key: &'static str, tooltip: String, label: Div| {
            Button::new(key)
                .ghost()
                .compact()
                .tooltip(tooltip)
                .child(label.text_color(theme::text()))
        };
        let mark = |kind: MarkKind| {
            cx.listener(move |this: &mut Self, _, window, cx| {
                let (text_kind, caret_kind) = (kind.clone(), kind.clone());
                this.format(
                    move |draft, range| draft.toggle(range, text_kind),
                    move |typing, active| typing.toggle(caret_kind, active),
                    window,
                    cx,
                )
            })
        };
        let size = |step: Option<SizeStep>| {
            cx.listener(move |this: &mut Self, _, window, cx| {
                this.format(
                    move |draft, range| draft.set_size(range, step),
                    move |typing, active| typing.set_size(step, active),
                    window,
                    cx,
                )
            })
        };
        h_flex()
            .mt(px(6.))
            .gap(px(2.))
            .child(
                button(
                    "mail-bold",
                    tr("mail.bold"),
                    div()
                        .text_size(px(14.))
                        .font_weight(FontWeight::BOLD)
                        .child("B"),
                )
                .on_click(mark(MarkKind::Bold)),
            )
            .child(
                button(
                    "mail-italic",
                    tr("mail.italic"),
                    div().text_size(px(14.)).italic().child("I"),
                )
                .on_click(mark(MarkKind::Italic)),
            )
            .child(
                button(
                    "mail-underline",
                    tr("mail.underline"),
                    div().text_size(px(14.)).underline().child("U"),
                )
                .on_click(mark(MarkKind::Underline)),
            )
            .child(div().w(px(1.)).h(px(18.)).mx(px(4.)).bg(theme::border()))
            .child(
                button(
                    "mail-smaller",
                    tr("mail.smaller"),
                    div().child("A").text_size(px(11.)),
                )
                .on_click(size(Some(SizeStep::Small))),
            )
            .child(
                button(
                    "mail-normal",
                    tr("mail.normal_size"),
                    div().text_size(px(14.)).child("A"),
                )
                .on_click(size(None)),
            )
            .child(
                button(
                    "mail-larger",
                    tr("mail.larger"),
                    div().child("A").text_size(px(18.)),
                )
                .on_click(size(Some(SizeStep::Large))),
            )
    }
}

fn image_directory() -> PathBuf {
    directories::ProjectDirs::from("", "", store::DATA_DIR_NAME)
        .map(|directories| directories.cache_dir().join("mail-images"))
        .unwrap_or_else(|| std::env::temp_dir().join("rusty-teams-mail-images"))
}

/// Writes an image under a name derived from `key` and returns its path.
fn store_image(key: &str, bytes: &[u8]) -> Option<PathBuf> {
    use std::hash::{DefaultHasher, Hash as _, Hasher as _};
    let mut hasher = DefaultHasher::new();
    key.hash(&mut hasher);
    let directory = image_directory();
    std::fs::create_dir_all(&directory).ok()?;
    let path = directory.join(format!("{:016x}", hasher.finish()));
    std::fs::write(&path, bytes).ok()?;
    Some(path)
}

fn is_blank(spans: &[Span]) -> bool {
    spans.iter().all(|span| match span {
        Span::Text(text) => text.trim().is_empty(),
        Span::LineBreak => true,
        _ => false,
    })
}

fn holds_blocks(spans: &[Span]) -> bool {
    spans.iter().any(|span| match span {
        Span::Table { .. } | Span::List { .. } | Span::Image { .. } | Span::Heading { .. } => true,
        Span::Bold(children)
        | Span::Italic(children)
        | Span::Underline(children)
        | Span::Sized(_, children)
        | Span::Colored { children, .. }
        | Span::Link { children, .. } => holds_blocks(children),
        _ => false,
    })
}

/// Mail HTML for the chat renderer: images point at local copies or are left out until loaded,
/// and tables that only lay out the mail become plain content.
fn mail_spans(
    spans: Vec<Span>,
    images: &HashMap<String, PathBuf>,
    blocked: &mut Vec<String>,
) -> Vec<Span> {
    macro_rules! walk {
        ($children:expr) => {
            mail_spans($children, images, blocked)
        };
    }
    let mut out = Vec::with_capacity(spans.len());
    for span in spans {
        match span {
            Span::Image { hosted_content_url } => match images.get(&hosted_content_url) {
                Some(path) => out.push(Span::Image {
                    hosted_content_url: file_url(path),
                }),
                None if hosted_content_url.starts_with("http") => blocked.push(hosted_content_url),
                None => {}
            },
            Span::Table { header, rows } => {
                let rows: Vec<Vec<Vec<Span>>> = rows
                    .into_iter()
                    .map(|row| row.into_iter().map(|children| walk!(children)).collect())
                    .collect();
                let layout = rows
                    .iter()
                    .all(|row| row.iter().filter(|cell| !is_blank(cell)).count() <= 1)
                    || rows.iter().flatten().any(|cell| holds_blocks(cell));
                if !layout {
                    out.push(Span::Table { header, rows });
                    continue;
                }
                for cell in rows.into_iter().flatten().filter(|cell| !is_blank(cell)) {
                    if !out.is_empty() && !matches!(out.last(), Some(Span::LineBreak)) {
                        out.push(Span::LineBreak);
                    }
                    out.extend(cell);
                }
            }
            Span::Bold(children) => out.push(Span::Bold(walk!(children))),
            Span::Italic(children) => out.push(Span::Italic(walk!(children))),
            Span::Underline(children) => out.push(Span::Underline(walk!(children))),
            Span::Strike(children) => out.push(Span::Strike(walk!(children))),
            Span::Sized(size, children) => out.push(Span::Sized(size, walk!(children))),
            Span::BlockQuote(children) => out.push(Span::BlockQuote(walk!(children))),
            Span::Colored {
                color,
                background,
                children,
            } => out.push(Span::Colored {
                color,
                background,
                children: walk!(children),
            }),
            Span::Link { url, children } => out.push(Span::Link {
                url,
                children: walk!(children),
            }),
            Span::Heading { level, children } => out.push(Span::Heading {
                level,
                children: walk!(children),
            }),
            Span::Indent { level, children } => out.push(Span::Indent {
                level,
                children: walk!(children),
            }),
            Span::Quote {
                message_id,
                children,
            } => out.push(Span::Quote {
                message_id,
                children: walk!(children),
            }),
            Span::List {
                ordered,
                start,
                items,
            } => out.push(Span::List {
                ordered,
                start,
                items: items.into_iter().map(|children| walk!(children)).collect(),
            }),
            other => out.push(other),
        }
    }
    out
}

fn file_url(path: &Path) -> String {
    format!("file://{}", path.display())
}

/// Blocks with images drawn from their local copies; the chat renderer leaves image blocks to its caller.
fn mail_blocks(blocks: &[Block], id: &str, cx: &App) -> AnyElement {
    let mut column = v_flex().w_full().gap(px(6.));
    let mut start = 0;
    let flush = |column: Div, end: usize, start: usize| {
        if start < end {
            column.child(render_blocks(
                &blocks[start..end],
                &format!("{id}-{start}"),
                false,
                cx,
            ))
        } else {
            column
        }
    };
    for (index, block) in blocks.iter().enumerate() {
        if let Block::Image { url } = block {
            column = flush(column, index, start);
            start = index + 1;
            if let Some(path) = url.strip_prefix("file://") {
                column = column.child(
                    img(PathBuf::from(path))
                        .max_w(px(IMAGE_MAX_WIDTH))
                        .object_fit(ObjectFit::ScaleDown),
                );
            }
        }
    }
    flush(column, blocks.len(), start).into_any_element()
}

/// A preview with line breaks would wrap even when truncated.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Mail text with color emoji: the font fallback would draw some of them as outlines.
fn mail_text(text: String, color: Hsla) -> impl IntoElement {
    let emoji = emoji_ranges(&text);
    let family = SharedString::from(theme::EMOJI_FONT_FAMILY);
    let highlights = split_highlights(Vec::new(), &emoji, text.len());
    let overrides: Vec<_> = emoji
        .into_iter()
        .map(|range| (range, family.clone()))
        .collect();
    div().w_full().text_color(color).child(
        StyledText::new(text)
            .with_highlights(highlights)
            .with_font_family_overrides(overrides),
    )
}

fn centered(text: String) -> AnyElement {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(13.))
        .text_color(theme::text_muted())
        .child(text)
        .into_any_element()
}

impl Render for MailPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ensure_started(window, cx);
        let right = match &self.compose {
            Some(compose) => self.render_compose(compose, cx),
            None => self.render_message(cx),
        };
        h_flex()
            .debug_selector(|| "shell/mail".into())
            .size_full()
            .bg(theme::surface())
            .child(self.render_folders(cx))
            .child(self.render_list(cx))
            .child(div().flex_1().min_w_0().h_full().child(right))
            .children(self.render_row_menu(cx))
    }
}

mod demo {
    use super::{Elapsed, MailFolder, MailMessage, MailSummary, Utc};

    pub const INBOX: &str = "demo-inbox";

    pub fn folders() -> Vec<MailFolder> {
        let folder = |id: &str, name: &str, unread: u64, total: u64| MailFolder {
            id: id.to_owned(),
            name: name.to_owned(),
            unread,
            total,
        };
        vec![
            folder(INBOX, "Inbox", 2, 4),
            folder("demo-sent", "Sent Items", 0, 1),
            folder("demo-drafts", "Drafts", 0, 0),
            folder("demo-archive", "Archive", 0, 0),
        ]
    }

    pub fn messages(folder_id: &str) -> Vec<MailSummary> {
        let mail = |id: &str,
                    from: &str,
                    subject: &str,
                    preview: &str,
                    hours: i64,
                    read: bool,
                    files: bool| {
            MailSummary {
                id: id.to_owned(),
                subject: subject.to_owned(),
                from_name: from.to_owned(),
                from_address: format!(
                    "{}@demo.example",
                    from.split(' ').next().unwrap_or(from).to_lowercase()
                ),
                preview: preview.to_owned(),
                received: Some(Utc::now() - Elapsed::hours(hours)),
                is_read: read,
                has_attachments: files,
            }
        };
        match folder_id {
            INBOX => vec![
                mail(
                    "d1",
                    "Mara Lindqvist",
                    "Release 42 go/no-go",
                    "Can we decide by Thursday? The checklist is attached.",
                    1,
                    false,
                    true,
                ),
                mail(
                    "d2",
                    "Jonas Ortega",
                    "Roadmap Q4 review",
                    "I moved the review to Friday, 10:00.",
                    5,
                    false,
                    false,
                ),
                mail(
                    "d3",
                    "Priya Nair",
                    "Re: Rollout plan",
                    "Thanks, the plan looks good to me.",
                    26,
                    true,
                    false,
                ),
                mail(
                    "d4",
                    "Tobias Klein",
                    "Postmortem draft",
                    "First draft is in the team folder.",
                    50,
                    true,
                    false,
                ),
            ],
            "demo-sent" => vec![mail(
                "s1",
                "Demo User",
                "Re: Roadmap Q4 review",
                "Friday works for me.",
                4,
                true,
                false,
            )],
            _ => Vec::new(),
        }
    }

    pub fn message(summary: MailSummary) -> MailMessage {
        let mut body = format!(
            "Hi,\n\n{}\n\nBest regards\n{}",
            summary.preview, summary.from_name
        );
        if summary.subject.starts_with("Re:") {
            body.push_str(
                "\n\n________________________________\nFrom: Demo User <demo.user@demo.example>\nSent: Monday 09:12\nSubject: Rollout plan\n\nHere is the rollout plan for next week. 🙂\n\n1. Pilot group on Monday\n2. Everyone on Wednesday",
            );
        }
        MailMessage {
            summary,
            to: vec!["demo.user@demo.example".to_owned()],
            cc: Vec::new(),
            body,
            html: String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;

    use teams_core::{Span, html_to_spans};

    use super::mail_spans;

    fn prepared(html: &str, images: &HashMap<String, PathBuf>) -> (Vec<Span>, Vec<String>) {
        let mut blocked = Vec::new();
        (
            mail_spans(html_to_spans(html), images, &mut blocked),
            blocked,
        )
    }

    fn has_table(spans: &[Span]) -> bool {
        spans.iter().any(|span| matches!(span, Span::Table { .. }))
    }

    #[test]
    fn inline_images_point_at_their_copy_and_remote_ones_wait() {
        let images = HashMap::from([("cid:logo".to_owned(), PathBuf::from("/tmp/logo"))]);
        let (spans, blocked) = prepared(
            r#"<p><img src="cid:logo"><img src="https://tracker.example/p.gif"></p>"#,
            &images,
        );
        let text = format!("{spans:?}");
        assert!(text.contains("file:///tmp/logo"));
        assert!(!text.contains("tracker"));
        assert_eq!(blocked, ["https://tracker.example/p.gif"]);
    }

    #[test]
    fn layout_tables_flatten_and_data_tables_stay() {
        let layout = r#"<table><tr><td>Hello</td></tr><tr><td>World</td></tr></table>"#;
        assert!(!has_table(&prepared(layout, &HashMap::new()).0));
        let data =
            r#"<table><tr><td>Name</td><td>Value</td></tr><tr><td>a</td><td>1</td></tr></table>"#;
        assert!(has_table(&prepared(data, &HashMap::new()).0));
    }
}
