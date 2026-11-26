use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use egui::{Align, ColorImage, Frame, Layout, Margin, RichText, TextureHandle, vec2};
use t7_proto::files::{self, ChannelFile, Command, Cursor, Event, FileId, GB, MAX_LIMIT, valid_name};
use t7_proto::model::ServerView;
use t7_proto::{C2S, ChannelId};
use ureq::http::Uri;

use super::App;
use super::chat::{Line, LineKind};
use super::key_bindings::modal_closed;
use crate::bindings::Action;
use crate::file_transfer::{self, Cancel, Download, Operation, Outcome, Source, Task, Update};
use crate::icons::{self, Icon};
use crate::image_preview::ImageSize;
use crate::net::Net;
use crate::theme::{self, Btn};
use crate::{file_dialog, image_preview};

const MAX_STAGED: usize = 4;
const MAX_TEXTURES: usize = 16;
const MAX_PREVIEW_ENTRIES: usize = 2048;
const SEARCH_DELAY: Duration = Duration::from_millis(240);
const MAX_REMOTE_REQUESTS: usize = 2;
static REMOTE_REQUESTS: AtomicUsize = AtomicUsize::new(0);

#[derive(Default)]
pub(super) struct Files {
    pub limit: Option<u64>,
    pub(super) drawer_open: bool,
    pub(super) library_open: bool,
    pub(super) limit_error: Option<String>,
    pub(super) notice: Option<(String, bool)>,
    readable: Vec<ChannelId>,
    can_post_here: bool,
    channel: Option<ChannelId>,
    serial: u64,
    recent: Vec<ChannelFile>,
    recent_request: Option<u64>,
    library: Library,
    staged: Vec<Staged>,
    queued: VecDeque<Operation>,
    uploading: Option<Active>,
    downloading: Option<Active>,
    playing: Option<tempfile::TempPath>,
    previewing: Option<(u64, PreviewKey, Cancel)>,
    detail: Option<ChannelFile>,
    jump: Option<ChannelFile>,
    scroll_to: Option<FileId>,
    dialog: bool,
    workers: Workers,
    previews: HashMap<PreviewKey, (u64, Preview)>,
    preview_touch: u64,
}

#[derive(Default)]
struct Library {
    query: String,
    scope: Option<ChannelId>,
    rows: Vec<ChannelFile>,
    next: Option<Cursor>,
    pending: Option<(u64, bool)>,
    search_at: Option<Instant>,
    error: Option<String>,
}

impl Library {
    fn posted(&mut self, file: &ChannelFile, channel_name: &str) {
        if self.scope.is_some_and(|id| id != file.channel)
            || !file.attachment.matches_query(channel_name, &self.query.trim().to_lowercase())
        {
            return;
        }
        self.insert(file.clone());
    }

    fn insert(&mut self, file: ChannelFile) {
        let pos = self.rows.partition_point(|row| row.attachment.key > file.attachment.key);
        if self.rows.get(pos).is_none_or(|row| row.attachment.key != file.attachment.key) {
            self.rows.insert(pos, file);
        }
    }
}

struct Staged {
    name: String,
    size: u64,
    source: DraftSource,
    thumbnail: Option<TextureHandle>,
}

enum DraftSource {
    Preparing(u64),
    Ready(Source),
}

struct Active {
    id: u64,
    name: String,
    channel: ChannelId,
    total: u64,
    bytes: u64,
    finalizing: bool,
    stopping: bool,
    cancel: Cancel,
}

#[derive(Clone, PartialEq, Eq, Hash)]
enum PreviewKey {
    File(ChannelId, FileId, ImageSize),
    Link(Uri),
}

enum Preview {
    Loading,
    Ready(TextureHandle),
    Failed,
    NotImage,
    Evicted,
}

struct Workers {
    tx: mpsc::Sender<WorkerResult>,
    rx: mpsc::Receiver<WorkerResult>,
}

impl Default for Workers {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        Self { tx, rx }
    }
}

enum WorkerResult {
    Pick(ChannelId, Result<Vec<Result<Staged, String>>, String>),
    Paste(u64, Result<Vec<u8>, String>),
    Save(ChannelFile, Result<Option<PathBuf>, String>),
    Image(PreviewKey, Result<Option<ColorImage>, String>),
}

impl Files {
    fn next_id(&mut self) -> u64 {
        self.serial += 1;
        self.serial
    }

    #[cfg(test)]
    pub(super) fn seed_thumbnail(&mut self, file: &ChannelFile, texture: TextureHandle) {
        self.track_image(
            PreviewKey::File(file.channel, file.attachment.key.id, ImageSize::Thumbnail),
            Preview::Ready(texture),
        );
    }

    #[cfg(test)]
    pub(super) fn linked_preview_ready(&self, url: &Uri) -> bool {
        matches!(self.previews.get(&PreviewKey::Link(url.clone())), Some((_, Preview::Ready(_))))
    }

    pub fn can_post(&self) -> bool {
        self.can_post_here && self.limit.is_some()
    }

    pub fn has_staged(&self) -> bool {
        !self.staged.is_empty()
    }

    pub fn preparing_image(&self) -> bool {
        self.staged.iter().any(|file| matches!(file.source, DraftSource::Preparing(_)))
    }

    pub(super) fn reads(&self, channel: ChannelId) -> bool {
        self.readable.binary_search(&channel).is_ok()
    }

    fn error(&mut self, error: String) {
        self.notice = Some((error, false));
    }

    fn channel_changed(&mut self, channel: ChannelId, net: &Net) -> Option<ChannelFile> {
        self.channel = Some(channel);
        self.staged.clear();
        self.queued.clear();
        if let Some(transfer) = &mut self.uploading {
            transfer.stopping = transfer.cancel.request();
        }
        self.recent.clear();
        self.recent_request = None;
        self.request_recent(net);
        self.jump.take().filter(|f| f.channel == channel)
    }

    fn request_recent(&mut self, net: &Net) {
        let Some(channel) = self.channel.filter(|&ch| self.reads(ch)) else { return };
        let request = self.next_id();
        self.recent_request = Some(request);
        net.send(C2S::Files(Command::List { request, channel: Some(channel), before: None, query: String::new() }));
    }

    fn request_library(&mut self, net: &Net, append: bool) {
        let request = self.next_id();
        let list = &mut self.library;
        let before = if append { list.next } else { None };
        list.pending = Some((request, append));
        list.search_at = None;
        list.error = None;
        net.send(C2S::Files(Command::List { request, channel: list.scope, before, query: list.query.clone() }));
    }

    pub fn open_library(&mut self, net: &Net) {
        self.library_open = true;
        self.request_library(net, false);
    }

    fn receive(&mut self, event: Event, net: &Net, view: Option<&ServerView>) -> Vec<ChannelFile> {
        match event {
            Event::Ready { limit, .. } | Event::Limit(limit) => {
                self.limit = Some(limit);
                self.limit_error = None;
            }
            Event::Access { mut readable, can_post_here } => {
                readable.sort_unstable();
                self.readable = readable;
                self.can_post_here = can_post_here;
                let reads = |id| self.readable.binary_search(&id).is_ok();
                if !can_post_here {
                    self.queued.clear();
                    self.staged.clear();
                }
                if self.detail.as_ref().is_some_and(|f| !reads(f.channel)) {
                    self.detail = None;
                }
                self.library.rows.retain(|f| reads(f.channel));
                if self.library.scope.is_some_and(|channel| !reads(channel)) {
                    self.library.scope = None;
                }
                self.previews.retain(|key, _| !matches!(key, PreviewKey::File(channel, _, _) if !reads(*channel)));
                if let Some((_, PreviewKey::File(channel, _, _), cancel)) = &self.previewing
                    && !reads(*channel)
                {
                    cancel.request();
                }
                if let Some(active) = &mut self.downloading
                    && !reads(active.channel)
                {
                    active.stopping = active.cancel.request();
                }
                if self.channel.is_some_and(reads) {
                    self.request_recent(net);
                } else {
                    self.recent.clear();
                    self.recent_request = None;
                }
                if self.library_open {
                    self.request_library(net, false);
                }
            }
            Event::Page { request, mut files, .. } if self.recent_request == Some(request) => {
                self.recent_request = None;
                files.retain(|f| self.channel == Some(f.channel) && self.reads(f.channel));
                for file in &files {
                    self.remember_recent(file);
                }
                return files;
            }
            Event::Page { request, mut files, next } => {
                if let Some((wanted, append)) = self.library.pending
                    && request == wanted
                {
                    files.retain(|f| self.reads(f.channel));
                    self.library.pending = None;
                    if !append {
                        self.library.rows.clear();
                    }
                    for file in files {
                        self.library.insert(file);
                    }
                    self.library.next = next;
                }
            }
            Event::Posted(file) if self.reads(file.channel) => {
                if self.library_open
                    && let Some(ch) = view.and_then(|view| view.channels.get(&file.channel))
                {
                    self.library.posted(&file, &ch.name);
                }
                if self.channel == Some(file.channel) {
                    self.remember_recent(&file);
                    return vec![file];
                }
            }
            Event::Error { request: Some(request), message } => {
                if self.library.pending.is_some_and(|(wanted, _)| wanted == request) {
                    self.library.pending = None;
                    self.library.error = Some(message);
                } else if self.recent_request == Some(request) {
                    self.recent_request = None;
                    self.error(message);
                }
            }
            Event::Error { request: None, message } => {
                self.limit_error = Some(message.clone());
                self.error(message);
            }
            _ => {}
        }
        Vec::new()
    }

    fn remember_recent(&mut self, file: &ChannelFile) {
        let at = self.recent.partition_point(|f| f.attachment.key > file.attachment.key);
        if self.recent.get(at).is_none_or(|f| f.attachment.key != file.attachment.key) {
            self.recent.insert(at, file.clone());
            self.recent.truncate(files::PAGE_SIZE);
        }
    }

    fn worker(&self, ctx: &egui::Context, run: impl FnOnce() -> WorkerResult + Send + 'static) {
        let tx = self.workers.tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(run());
            ctx.request_repaint();
        });
    }

    pub fn pick(&mut self, ctx: &egui::Context) {
        let Some(channel) = self.channel.filter(|_| self.can_post() && !self.dialog) else { return };
        self.dialog = true;
        self.worker(ctx, move || {
            WorkerResult::Pick(
                channel,
                file_dialog::pick(MAX_STAGED).map(|paths| paths.into_iter().map(stage_file).collect()),
            )
        });
    }

    pub fn paste(&mut self, ctx: &egui::Context, image: Arc<ColorImage>) {
        if self.channel.is_none() || !self.can_post() {
            return;
        }
        if self.staged.len() >= MAX_STAGED {
            self.error(format!("Attach up to {MAX_STAGED} files per message"));
            return;
        }
        let thumb = match image_preview::clipboard_thumbnail(&image) {
            Ok(thumb) => thumb,
            Err(error) => return self.error(error),
        };
        let id = self.next_id();
        self.staged.push(Staged {
            name: "Clipboard image.png".into(),
            size: 0,
            source: DraftSource::Preparing(id),
            thumbnail: Some(ctx.load_texture("Clipboard thumbnail", thumb, egui::TextureOptions::LINEAR)),
        });
        self.worker(ctx, move || WorkerResult::Paste(id, image_preview::clipboard_png(&image)));
    }

    fn saving(&mut self, ctx: &egui::Context, file: ChannelFile) {
        self.dialog = true;
        self.worker(ctx, move || {
            let path = file_dialog::save(&file.attachment.name);
            WorkerResult::Save(file, path)
        });
    }

    fn stage(&mut self, file: Result<Staged, String>) {
        match file {
            Ok(_) if self.staged.len() >= MAX_STAGED => {
                self.error(format!("Attach up to {MAX_STAGED} files per message"));
            }
            Ok(file) if self.limit.is_some_and(|limit| file.size <= limit) => self.staged.push(file),
            Ok(file) => self.error(format!("{} exceeds this server's per-file limit", file.name)),
            Err(error) => self.error(error),
        }
    }

    pub fn queue_uploads(&mut self, caption: &str, net: &Net) -> bool {
        if self.preparing_image() {
            return false;
        }
        let Some(channel) = self.channel.filter(|_| self.can_post() && self.uploading.is_none()) else { return false };
        if self.staged.iter().any(|file| self.limit.is_none_or(|limit| file.size > limit)) {
            self.error("A selected file exceeds the server's per-file limit".into());
            return false;
        }
        self.queued.extend(self.staged.drain(..).enumerate().map(|(i, stage)| {
            let DraftSource::Ready(source) = stage.source else { unreachable!("all staged files are ready") };
            Operation::Upload {
                channel,
                name: stage.name,
                size: stage.size,
                source,
                caption: if i == 0 { caption.to_owned() } else { String::new() },
            }
        }));
        self.start_next(net);
        true
    }

    fn start_next(&mut self, net: &Net) {
        if self.uploading.is_none()
            && let Some(op @ Operation::Upload { .. }) = self.queued.pop_front()
            && let Operation::Upload { channel, name, size, .. } = &op
        {
            self.uploading = Some(self.start(net, *channel, name.clone(), *size, op));
        }
    }

    fn start(&mut self, net: &Net, channel: ChannelId, name: String, total: u64, operation: Operation) -> Active {
        let id = self.next_id();
        let (cancel, token) = file_transfer::cancellation();
        net.transfer(Task { id, operation, cancel: token });
        Active { id, name, channel, total, cancel, bytes: 0, finalizing: false, stopping: false }
    }

    pub fn transfer_update(&mut self, id: u64, update: Update, ctx: &egui::Context, net: &Net) {
        if self.previewing.as_ref().is_some_and(|(wanted, ..)| *wanted == id) {
            if let Update::Finished(result) = update {
                let (_, key, _) = self.previewing.take().expect("checked");
                if let PreviewKey::File(channel, id, size) = key
                    && self.reads(channel)
                    && (size == ImageSize::Thumbnail
                        || self.detail.as_ref().is_some_and(|f| f.channel == channel && f.attachment.key.id == id))
                {
                    self.finish_image(
                        ctx,
                        key,
                        result.and_then(|outcome| match outcome {
                            Outcome::Preview(image) => Ok(image),
                            _ => Err(String::new()),
                        }),
                    );
                }
            }
            return;
        }
        let uploading = self.uploading.as_ref().is_some_and(|transfer| transfer.id == id);
        let slot = if uploading { &mut self.uploading } else { &mut self.downloading };
        let Some(transfer) = slot.as_mut().filter(|transfer| transfer.id == id) else { return };
        match update {
            Update::Progress { bytes, finalizing } => {
                transfer.bytes = bytes;
                transfer.finalizing = finalizing;
            }
            Update::Finished(result) => {
                let transfer = slot.take().expect("checked");
                let name = transfer.name;
                match result {
                    Ok(Outcome::Uploaded) if self.queued.is_empty() => {
                        self.notice = Some((format!("Uploaded {name}"), true));
                    }
                    Ok(Outcome::Saved) => self.notice = Some((format!("Saved {name}"), true)),
                    Ok(Outcome::Video(path)) if self.reads(transfer.channel) => {
                        let opener = if cfg!(windows) {
                            "explorer.exe"
                        } else if cfg!(target_os = "macos") {
                            "open"
                        } else {
                            "xdg-open"
                        };
                        match std::process::Command::new(opener).arg(path.as_os_str()).spawn() {
                            Ok(_) => self.playing = Some(path),
                            Err(error) => self.error(format!("Could not open the video player: {error}")),
                        }
                    }
                    Ok(Outcome::Canceled) => self.notice = Some((format!("Canceled {name}"), true)),
                    Err(error) => self.error(format!("{name}: {error}")),
                    _ => {}
                }
                if uploading {
                    self.start_next(net);
                }
            }
        }
    }

    fn poll_workers(&mut self, ctx: &egui::Context, net: &Net) {
        while let Ok(result) = self.workers.rx.try_recv() {
            match result {
                WorkerResult::Pick(channel, result) => {
                    self.dialog = false;
                    if self.channel == Some(channel) && self.can_post() {
                        match result {
                            Ok(files) => files.into_iter().for_each(|file| self.stage(file)),
                            Err(error) => self.error(error),
                        }
                    }
                }
                WorkerResult::Paste(id, result) => {
                    let Some(index) = self
                        .staged
                        .iter()
                        .position(|f| matches!(f.source, DraftSource::Preparing(wanted) if wanted == id))
                    else {
                        continue;
                    };
                    match result {
                        Ok(bytes) if self.limit.is_some_and(|limit| bytes.len() as u64 <= limit) => {
                            let file = &mut self.staged[index];
                            file.size = bytes.len() as u64;
                            file.source = DraftSource::Ready(Source::Clipboard(bytes));
                        }
                        other => {
                            let file = self.staged.remove(index);
                            self.error(
                                other
                                    .err()
                                    .unwrap_or_else(|| format!("{} exceeds this server's per-file limit", file.name)),
                            );
                        }
                    }
                }
                WorkerResult::Save(file, result) => {
                    self.dialog = false;
                    match result {
                        Ok(Some(path)) if self.reads(file.channel) && self.downloading.is_none() => {
                            let ChannelFile { channel, attachment: a } = file;
                            let op = Operation::Download { file: a.key.id, size: a.size, target: Download::Save(path) };
                            self.downloading = Some(self.start(net, channel, a.name, a.size, op));
                        }
                        Ok(Some(_)) => self.error("This file is unavailable".into()),
                        Ok(None) => {}
                        Err(error) => self.error(error),
                    }
                }
                WorkerResult::Image(key, Ok(None)) => self.track_image(key, Preview::NotImage),
                WorkerResult::Image(key, Ok(Some(image))) => self.finish_image(ctx, key, Ok(image)),
                WorkerResult::Image(key, Err(_)) => self.track_image(key, Preview::Failed),
            }
        }
        if self.library_open
            && let Some(at) = self.library.search_at
        {
            if let Some(delay) = at.checked_duration_since(Instant::now()) {
                ctx.request_repaint_after(delay);
            } else {
                self.request_library(net, false);
            }
        }
    }

    fn track_image(&mut self, key: PreviewKey, state: Preview) {
        if matches!(state, Preview::Ready(_)) {
            let textures = self.previews.values().filter(|(_, state)| matches!(state, Preview::Ready(_)));
            if textures.count() >= MAX_TEXTURES
                && let Some((_, oldest)) = self
                    .previews
                    .values_mut()
                    .filter(|(_, state)| matches!(state, Preview::Ready(_)))
                    .min_by_key(|(age, _)| *age)
            {
                *oldest = Preview::Evicted;
            }
        }
        if self.previews.len() >= MAX_PREVIEW_ENTRIES
            && !self.previews.contains_key(&key)
            && let Some((oldest, _)) = self
                .previews
                .iter()
                .filter(|(_, (_, state))| !matches!(state, Preview::Loading))
                .min_by_key(|(_, (age, _))| *age)
        {
            self.previews.remove(&oldest.clone());
        }
        self.preview_touch += 1;
        self.previews.insert(key, (self.preview_touch, state));
    }

    fn finish_image(&mut self, ctx: &egui::Context, key: PreviewKey, result: Result<ColorImage, String>) {
        let state = match result {
            Ok(image) => Preview::Ready(ctx.load_texture("file-preview", image, egui::TextureOptions::LINEAR)),
            Err(_) => Preview::Failed,
        };
        self.track_image(key, state);
    }

    fn see_image(&mut self, key: &PreviewKey, visible: bool) -> Option<&Preview> {
        if !visible && matches!(self.previews.get(key), Some((_, Preview::Evicted))) {
            self.previews.remove(key);
            return None;
        }
        self.preview_touch += 1;
        let (age, image) = self.previews.get_mut(key)?;
        if visible {
            *age = self.preview_touch;
        }
        Some(image)
    }

    fn preview_file(&mut self, ui: &egui::Ui, net: &Net, file: &ChannelFile, size: ImageSize) -> Option<&Preview> {
        if file.attachment.size > image_preview::MAX_ENCODED {
            return None;
        }
        let target = if image_preview::is_image_name(&file.attachment.name) {
            Download::Preview(size)
        } else {
            #[cfg(windows)]
            {
                if size == ImageSize::Expanded {
                    return None;
                }
                Download::VideoPoster(video_suffix(&file.attachment.name)?)
            }
            #[cfg(not(windows))]
            return None;
        };
        let key = PreviewKey::File(file.channel, file.attachment.key.id, size);
        let may_load = self.previewing.is_none() && self.reads(file.channel);
        let visible = ui.is_rect_visible(egui::Rect::from_min_size(ui.next_widget_position(), vec2(1.0, 200.0)));
        if self.see_image(&key, visible).is_none() && may_load && visible {
            let id = self.next_id();
            let (cancel, token) = file_transfer::cancellation();
            self.previewing = Some((id, key.clone(), cancel));
            self.track_image(key.clone(), Preview::Loading);
            let operation = Operation::Download { file: file.attachment.key.id, size: file.attachment.size, target };
            net.transfer(Task { id, operation, cancel: token });
        }
        self.previews.get(&key).map(|(_, state)| state)
    }

    pub fn linked_image(&mut self, ui: &mut egui::Ui, url: &Uri) {
        let key = PreviewKey::Link(url.clone());
        let link = ui.hyperlink_to(url.host().unwrap_or("Open original"), url.to_string());
        if let Some(state) = self.see_image(&key, ui.is_rect_visible(link.rect)) {
            show_preview(ui, state);
        } else if ui.is_rect_visible(link.rect)
            && REMOTE_REQUESTS
                .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                    (count < MAX_REMOTE_REQUESTS).then_some(count + 1)
                })
                .is_ok()
        {
            self.track_image(key.clone(), Preview::Loading);
            let url = url.clone();
            self.worker(ui.ctx(), move || {
                let result = image_preview::fetch(&url);
                REMOTE_REQUESTS.fetch_sub(1, Ordering::AcqRel);
                WorkerResult::Image(key, result)
            });
            show_preview(ui, &Preview::Loading);
        }
    }

    pub fn staged_tiles(&mut self, ui: &mut egui::Ui) {
        let mut remove = None;
        if self.staged.iter().any(|f| f.thumbnail.is_some()) {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = vec2(6.0, 6.0);
                for (i, file) in self.staged.iter().enumerate() {
                    if let Some(texture) = &file.thumbnail {
                        let detail = match file.source {
                            DraftSource::Preparing(_) => "Preparing image…".into(),
                            DraftSource::Ready(_) => size_label(file.size),
                        };
                        if image_tile(ui, texture, &file.name, &detail) {
                            remove = Some(i);
                        }
                    }
                }
            });
        }
        for (i, file) in self.staged.iter().enumerate() {
            if file.thumbnail.is_none() && staged_file_row(ui, &file.name, &size_label(file.size)) {
                remove = Some(i);
            }
        }
        if let Some(i) = remove {
            self.staged.remove(i);
        }
    }

    pub fn transfer_rows(&mut self, ui: &mut egui::Ui) {
        if self.uploading.is_none() && self.downloading.is_none() && self.notice.is_none() {
            return;
        }
        egui::Panel::top("file-transfers")
            .frame(Frame::new().fill(theme::SURFACE).inner_margin(Margin::symmetric(16, 5)))
            .show(ui, |ui| {
                if let Some(transfer) = &mut self.uploading
                    && transfer_row(ui, "Uploading", transfer)
                {
                    self.queued.clear();
                }
                if let Some(transfer) = &mut self.downloading {
                    transfer_row(ui, "Downloading", transfer);
                }
                if let Some((message, ok)) = &self.notice {
                    let mut dismiss = false;
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::Label::new(RichText::new(message).small().color(if *ok {
                                theme::TEXT_DIM
                            } else {
                                theme::RED
                            }))
                            .truncate(),
                        )
                        .on_hover_text(message);
                        dismiss = ui.add(Btn::ghost("Dismiss").small()).clicked();
                    });
                    if dismiss {
                        self.notice = None;
                    }
                }
            });
    }

    pub fn drawer(&mut self, ui: &mut egui::Ui, view: &ServerView, net: &Net) -> Option<ChannelId> {
        let mut unlock = None;
        let width = (ui.available_width() * 0.34).clamp(174.0, 246.0);
        let frame = Frame::new().fill(theme::SURFACE).inner_margin(Margin::symmetric(12, 12));
        egui::Panel::right("channel-files").exact_size(width).frame(frame).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Channel files").size(15.0).strong());
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.add(Btn::ghost("×").small()).on_hover_text("Close files").clicked() {
                        self.drawer_open = false;
                    }
                });
            });
            if let Some(channel) = self.channel.and_then(|id| view.channels.get(&id)) {
                ui.add(egui::Label::new(RichText::new(&channel.name).small().color(theme::TEXT_DIM)).truncate());
            }
            if let Some(ch) = self.channel.and_then(|id| view.channels.get(&id))
                && ch.has_password
                && !self.reads(ch.id)
                && view.may_in(ch.id, t7_proto::perms::Perm::Join)
                && ui.add(Btn::secondary("Unlock files").small()).clicked()
            {
                unlock = Some(ch.id);
            }
            if ui.add(Btn::ghost("View all files").small()).clicked() {
                self.open_library(net);
            }
            ui.add_space(8.0);
            if self.recent.is_empty() {
                let hint = if self.recent_request.is_some() {
                    "Loading files…"
                } else if self.channel.is_some_and(|id| self.reads(id)) {
                    "No files shared here yet"
                } else {
                    "Files are unavailable in this channel"
                };
                ui.label(RichText::new(hint).color(theme::TEXT_DIM));
            }
            egui::ScrollArea::vertical().id_salt("recent-files").show(ui, |ui| {
                for file in &self.recent {
                    if self.file_row(ui, file, false) {
                        self.detail = Some(file.clone());
                    }
                }
            });
        });
        unlock
    }

    pub fn library(&mut self, ui: &mut egui::Ui, view: &ServerView, net: &Net) {
        Frame::new().inner_margin(Margin::symmetric(20, 12)).show(ui, |ui| {
            let scope_before = self.library.scope;
            let mut channels: Vec<_> = self.readable.iter().filter_map(|id| view.channels.get(id)).collect();
            channels.sort_by(|a, b| a.name.cmp(&b.name));
            ui.horizontal(|ui| {
                let search =
                    super::widgets::text_input(&mut self.library.query).hint_text("Search files").char_limit(60);
                if ui.add(search.desired_width((ui.available_width() * 0.61).max(100.0))).changed() {
                    self.library.rows.clear();
                    self.library.pending = None;
                    self.library.next = None;
                    self.library.search_at = Some(Instant::now() + SEARCH_DELAY);
                    ui.ctx().request_repaint_after(SEARCH_DELAY);
                }
                let scope =
                    self.library.scope.and_then(|id| view.channels.get(&id)).map_or("All channels", |c| &c.name);
                egui::ComboBox::from_id_salt("file-channel-scope").selected_text(scope).truncate().show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.library.scope, None, "All channels");
                    for channel in &channels {
                        ui.selectable_value(&mut self.library.scope, Some(channel.id), &channel.name);
                    }
                });
            });
            if self.library.scope != scope_before {
                self.library.rows.clear();
                self.library.next = None;
                self.request_library(net, false);
            }
            ui.add_space(10.0);
            if let Some(error) = &self.library.error {
                ui.label(RichText::new(error).color(theme::RED));
                if ui.add(Btn::ghost("Retry").small()).clicked() {
                    self.request_library(net, false);
                }
            } else if self.library.rows.is_empty() {
                let text = if self.library.pending.is_some() || self.library.search_at.is_some() {
                    "Loading files…"
                } else {
                    "No files match this view"
                };
                ui.label(RichText::new(text).color(theme::TEXT_DIM));
            }
            let mut more = false;
            egui::ScrollArea::vertical().id_salt("all-files").show(ui, |ui| {
                let mut grouped = HashMap::<ChannelId, Vec<&ChannelFile>>::new();
                for file in &self.library.rows {
                    grouped.entry(file.channel).or_default().push(file);
                }
                for channel in channels {
                    if let Some(files) = grouped.get(&channel.id) {
                        ui.add_space(9.0);
                        ui.label(RichText::new(&channel.name).size(14.0).strong());
                        for file in files {
                            if self.file_row(ui, file, true) {
                                self.detail = Some((*file).clone());
                            }
                        }
                    }
                }
                if self.library.next.is_some() {
                    more =
                        ui.add_enabled(self.library.pending.is_none(), Btn::secondary("Load more").small()).clicked();
                }
            });
            if more {
                self.request_library(net, true);
            }
        });
    }

    fn file_row(&self, ui: &mut egui::Ui, file: &ChannelFile, library: bool) -> bool {
        let a = &file.attachment;
        ui.separator();
        let (rect, response) = ui.allocate_exact_size(vec2(ui.available_width(), 38.0), egui::Sense::click());
        if ui.is_rect_visible(rect) {
            let p = ui.painter();
            if response.hovered() {
                p.rect_filled(rect, 5, theme::SURFACE_HOVER);
            }
            let center = rect.left_center() + vec2(15.0, 0.0);
            let texture = self.previews.get(&PreviewKey::File(file.channel, a.key.id, ImageSize::Thumbnail));
            if let Some((_, Preview::Ready(texture))) = texture {
                let wh = texture.size_vec2();
                let display = egui::Rect::from_center_size(center, wh * (26.0 / wh.max_elem()));
                p.image(
                    texture.id(),
                    display,
                    egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)),
                    egui::Color32::WHITE,
                );
            } else {
                icons::paint(p, egui::Rect::from_center_size(center, vec2(18.0, 18.0)), Icon::File, theme::ACCENT_TEXT);
            }
            let mut meta = format!("{} · {}", size_label(a.size), a.nickname);
            if library {
                meta.push_str(" · ");
                meta.push_str(&posted_date(file));
            }
            let width = (rect.width() - 48.0).max(0.0);
            let name = super::widgets::truncated(ui, &a.name, egui::FontId::proportional(13.0), theme::TEXT, width);
            let info = super::widgets::truncated(ui, &meta, egui::FontId::proportional(11.5), theme::TEXT_DIM, width);
            p.galley(rect.left_top() + vec2(32.0, 2.0), name, theme::TEXT);
            p.galley(rect.left_top() + vec2(32.0, 19.0), info, theme::TEXT_DIM);
            icons::paint(
                p,
                egui::Rect::from_center_size(rect.right_center() - vec2(7.0, 0.0), vec2(12.0, 12.0)),
                Icon::ChevronRight,
                theme::TEXT_FAINT,
            );
        }
        response.on_hover_text(&a.name).on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
    }

    pub fn chat_attachment(&mut self, ui: &mut egui::Ui, net: &Net, file: &ChannelFile) {
        let a = &file.attachment;
        let response = ui.vertical(|ui| {
            let available = self.downloading.is_none() && !self.dialog;
            let preview = self.preview_file(ui, net, file, ImageSize::Thumbnail);
            if let Some(suffix) = video_suffix(&a.name) {
                let poster = match preview {
                    Some(Preview::Ready(texture)) => Some(texture),
                    _ => None,
                };
                let (play, details) = video_tile(ui, &a.name, a.size, poster, available);
                if play {
                    let op = Operation::Download { file: a.key.id, size: a.size, target: Download::Video(suffix) };
                    self.downloading = Some(self.start(net, file.channel, a.name.clone(), a.size, op));
                }
                if details {
                    self.detail = Some(file.clone());
                }
            } else if let Some(Preview::Ready(texture)) = preview {
                let image =
                    preview_image(ui, texture, vec2(480.0, 300.0)).alt_text(&a.name).sense(egui::Sense::click());
                if ui.add(image).on_hover_text(&a.name).on_hover_cursor(egui::CursorIcon::ZoomIn).clicked() {
                    self.detail = Some(file.clone());
                }
            } else {
                let detail = match preview {
                    Some(Preview::Loading) => "Loading image…".to_owned(),
                    Some(Preview::Failed) => format!("Preview unavailable · {}", size_label(a.size)),
                    _ => size_label(a.size),
                };
                if document_tile(ui, &a.name, &detail) {
                    self.detail = Some(file.clone());
                }
            }
        });
        if self.scroll_to == Some(a.key.id) {
            response.response.scroll_to_me(Some(Align::Center));
            self.scroll_to = None;
        }
        ui.add_space(4.0);
    }
}

impl App {
    pub(super) fn file_event(&mut self, event: Event) {
        for file in self.files.receive(event, &self.net, self.view.as_ref()) {
            self.insert_file_post(file);
        }
        let files = &self.files;
        self.tabs[1].lines.retain(|line| !matches!(&line.kind, LineKind::File(file) if !files.reads(file.channel)));
    }

    pub(super) fn files_joined(&mut self, channel: ChannelId) {
        self.tabs[1].lines.retain(|line| !matches!(line.kind, LineKind::File(_)));
        if let Some(file) = self.files.channel_changed(channel, &self.net) {
            self.active_tab = 1;
            self.files.library_open = false;
            self.files.drawer_open = false;
            self.files.scroll_to = Some(file.attachment.key.id);
            self.insert_file_post(file);
        }
    }

    fn insert_file_post(&mut self, file: ChannelFile) {
        let a = &file.attachment;
        let Some(at) = chrono::DateTime::from_timestamp_millis(a.key.posted_ms as i64) else { return };
        let at = at.with_timezone(&chrono::Local).naive_local();
        let tab = &mut self.tabs[1];
        let id = a.key.id;
        if tab.lines.iter().any(|line| matches!(&line.kind, LineKind::File(f) if f.attachment.key.id == id)) {
            return;
        }
        let i = tab.lines.partition_point(|line| line.at <= at);
        tab.lines.insert(i, Line::new(at, LineKind::File(file), String::new()));
        if i == 0 && self.files.scroll_to == Some(id) && tab.lines.len() > super::chat::MAX_LINES {
            tab.lines.remove(1);
        } else {
            super::chat::trim_lines(&mut tab.lines);
        }
    }

    pub(super) fn poll_file_workers(&mut self, ctx: &egui::Context) {
        self.files.poll_workers(ctx, &self.net);
    }

    pub(super) fn file_detail(&mut self, ctx: &egui::Context) {
        let Some(file) = self.files.detail.clone() else { return };
        let Some(view) = &self.view else { return };
        let here = view.my_channel() == Some(file.channel);
        let channel = view.channels.get(&file.channel).map_or("Channel", |ch| ch.name.as_str());
        let mut save = false;
        let mut jump = false;
        let mut close = false;
        let modal = theme::modal("file-detail").show(ctx, |ui| {
            let width: f32 = if image_preview::is_image_name(&file.attachment.name) { 960.0 } else { 392.0 };
            ui.set_width(width.min(ctx.content_rect().width() - 48.0).max(1.0));
            ui.label(RichText::new(&file.attachment.name).size(17.0).strong().color(theme::TEXT));
            ui.add_space(8.0);
            if let Some(state) = self.files.preview_file(ui, &self.net, &file, ImageSize::Expanded) {
                if let Preview::Ready(texture) = state {
                    ui.add(preview_image(ui, texture, vec2(width, (ctx.content_rect().height() - 170.0).max(1.0))));
                } else {
                    show_preview(ui, state);
                }
            }
            ui.label(
                RichText::new(format!("{} · {}", size_label(file.attachment.size), channel)).color(theme::TEXT_DIM),
            );
            ui.label(
                RichText::new(format!("Uploaded by {} · {}", file.attachment.nickname, posted_date(&file)))
                    .color(theme::TEXT_DIM),
            );
            ui.add_space(12.0);
            ui.horizontal(|ui| {
                save = ui
                    .add_enabled(!self.files.dialog && self.files.downloading.is_none(), Btn::primary("Save file…"))
                    .clicked();
                jump = ui.add(Btn::secondary(if here { "Show in chat" } else { "Join and show in chat" })).clicked();
                close = ui.add(Btn::ghost("Close")).clicked();
            });
        });
        if save {
            self.files.saving(ctx, file.clone());
        }
        if jump {
            if here {
                self.active_tab = 1;
                self.files.library_open = false;
                self.files.drawer_open = false;
                self.files.scroll_to = Some(file.attachment.key.id);
                self.insert_file_post(file.clone());
            } else {
                let channel = file.channel;
                self.files.jump = Some(file.clone());
                self.join_channel(channel);
            }
        }
        if save || jump || close || modal_closed(&modal, self.pressed.contains(&Action::DismissDialog)) {
            self.files.previews.remove(&PreviewKey::File(file.channel, file.attachment.key.id, ImageSize::Expanded));
            self.files.detail = None;
        }
    }
}

fn stage_file(path: PathBuf) -> Result<Staged, String> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| valid_name(n))
        .ok_or("That file name is not supported")?
        .to_owned();
    let metadata = path.metadata().map_err(|e| format!("{name}: {e}"))?;
    if !metadata.is_file() {
        return Err(format!("{name} is not a regular file"));
    }
    Ok(Staged { name, size: metadata.len(), source: DraftSource::Ready(Source::Disk(path)), thumbnail: None })
}

fn image_tile(ui: &mut egui::Ui, texture: &TextureHandle, name: &str, detail: &str) -> bool {
    let (tile, response) = ui.allocate_exact_size(vec2(68.0, 68.0), egui::Sense::hover());
    let size = texture.size_vec2();
    let uv = egui::Rect::from_center_size(egui::pos2(0.5, 0.5), egui::Vec2::splat(size.min_elem()) / size);
    ui.painter().rect_filled(tile, 6, theme::SURFACE_ACTIVE);
    egui::Image::new(texture).uv(uv).corner_radius(6).paint_at(ui, tile);
    response.on_hover_text(format!("{name}\n{detail}"));
    let hit = egui::Rect::from_min_size(tile.right_top() - vec2(26.0, 0.0), vec2(26.0, 26.0));
    let close = ui.interact(hit, ui.scope_id().with(("remove-image", texture.id())), egui::Sense::click());
    let center = hit.center();
    let color =
        if close.hovered() || close.has_focus() { theme::SURFACE_ACTIVE } else { egui::Color32::from_black_alpha(195) };
    ui.painter().circle_filled(center, 9.0, color);
    icons::paint(
        ui.painter(),
        egui::Rect::from_center_size(center, vec2(15.0, 15.0)),
        Icon::Cross,
        egui::Color32::WHITE,
    );
    close.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, format!("Remove {name}")));
    close.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text("Remove image").clicked()
}

fn staged_file_row(ui: &mut egui::Ui, name: &str, detail: &str) -> bool {
    ui.horizontal(|ui| {
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let remove = ui.add(Btn::ghost("Remove").small()).clicked();
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 1.0;
                ui.add(egui::Label::new(name).truncate()).on_hover_text(name);
                ui.label(RichText::new(detail).small().color(theme::TEXT_DIM));
            });
            remove
        })
        .inner
    })
    .inner
}

fn document_tile(ui: &mut egui::Ui, name: &str, detail: &str) -> bool {
    let width = ui.available_width().min(320.0);
    let (rect, row) = ui.allocate_exact_size(vec2(width, 46.0), egui::Sense::click());
    let p = ui.painter();
    p.rect_filled(rect, 6, if row.hovered() { theme::SURFACE_HOVER } else { theme::SURFACE });
    icons::paint(
        p,
        egui::Rect::from_center_size(rect.left_center() + vec2(21.0, 0.0), vec2(20.0, 20.0)),
        Icon::File,
        theme::ACCENT_TEXT,
    );
    let label =
        super::widgets::truncated(ui, name, egui::FontId::proportional(13.0), theme::TEXT, (width - 53.0).max(0.0));
    p.galley(rect.min + vec2(42.0, 6.0), label, theme::TEXT);
    p.text(
        rect.min + vec2(42.0, 24.0),
        egui::Align2::LEFT_TOP,
        detail,
        egui::FontId::proportional(11.5),
        theme::TEXT_DIM,
    );
    row.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, name));
    row.on_hover_text(name).on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
}

fn video_tile(
    ui: &mut egui::Ui,
    name: &str,
    bytes: u64,
    poster: Option<&TextureHandle>,
    available: bool,
) -> (bool, bool) {
    let width = ui.available_width().min(320.0);
    let (rect, _) = ui.allocate_exact_size(vec2(width, width * 9.0 / 16.0), egui::Sense::hover());
    let (screen, footer) = rect.split_top_bottom_at_y((rect.bottom() - 32.0).max(rect.top()));
    let play = ui.interact(
        screen,
        ui.auto_id_with("play"),
        if available { egui::Sense::click() } else { egui::Sense::hover() },
    );
    let details = ui.interact(footer, ui.auto_id_with("details"), egui::Sense::click());
    let p = ui.painter();
    p.rect_filled(rect, 7, theme::SURFACE);
    p.rect_filled(screen, egui::CornerRadius { nw: 7, ne: 7, sw: 0, se: 0 }, theme::SURFACE_HOVER);
    if let Some(texture) = poster {
        let size = texture.size_vec2();
        let scale = (screen.width() / size.x).min(screen.height() / size.y);
        let rect = egui::Rect::from_center_size(screen.center(), size * scale);
        egui::Image::new((texture.id(), rect.size())).corner_radius(6).paint_at(ui, rect);
    }
    let center = screen.center();
    p.circle_filled(center, 21.0, if play.hovered() && available { theme::ACCENT_HOVER } else { theme::BG });
    p.add(egui::Shape::convex_polygon(
        vec![center + vec2(-4.0, -7.0), center + vec2(7.0, 0.0), center + vec2(-4.0, 7.0)],
        if available { theme::TEXT } else { theme::TEXT_FAINT },
        egui::Stroke::NONE,
    ));
    let size = p.layout_no_wrap(size_label(bytes), egui::FontId::proportional(11.5), theme::TEXT_DIM);
    let size_pos = footer.right_center() - vec2(size.size().x + 10.0, size.size().y / 2.0);
    let label = super::widgets::truncated(
        ui,
        name,
        egui::FontId::proportional(13.0),
        theme::TEXT,
        (size_pos.x - footer.left() - 18.0).max(0.0),
    );
    p.galley(footer.left_center() + vec2(10.0, -label.size().y / 2.0), label, theme::TEXT);
    p.galley(size_pos, size, theme::TEXT_DIM);
    play.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, available, format!("Play {name}")));
    details.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Button, true, format!("File details for {name}")));
    (
        available
            && play
                .on_hover_text("Play in your video player")
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .clicked(),
        details.on_hover_text("File details").on_hover_cursor(egui::CursorIcon::PointingHand).clicked(),
    )
}

fn video_suffix(name: &str) -> Option<&'static str> {
    let extension = std::path::Path::new(name).extension()?.to_str()?;
    [".mp4", ".m4v", ".webm", ".mov", ".mkv", ".avi", ".mpeg", ".mpg", ".ogv"]
        .into_iter()
        .find(|suffix| extension.eq_ignore_ascii_case(&suffix[1..]))
}

fn preview_image(ui: &egui::Ui, texture: &TextureHandle, bounds: egui::Vec2) -> egui::Image<'static> {
    let native = texture.size_vec2();
    let scale = (ui.available_width().min(bounds.x).max(1.0) / native.x).min(bounds.y / native.y).min(1.0);
    egui::Image::new((texture.id(), native * scale)).corner_radius(6)
}

fn show_preview(ui: &mut egui::Ui, state: &Preview) {
    match state {
        Preview::Ready(texture) => {
            let native = texture.size_vec2();
            let scale = (ui.available_width().min(410.0) / native.x).min(200.0 / native.y).min(1.0);
            ui.add_space(8.0);
            ui.add(egui::Image::new((texture.id(), native * scale.max(0.01))));
            ui.add_space(8.0);
        }
        Preview::Loading => {
            ui.label(RichText::new("Loading image…").small().color(theme::TEXT_DIM));
        }
        Preview::NotImage | Preview::Evicted => {}
        Preview::Failed => {
            ui.label(RichText::new("Image preview unavailable").small().color(theme::TEXT_DIM));
        }
    }
}

fn transfer_row(ui: &mut egui::Ui, verb: &str, active: &mut Active) -> bool {
    ui.horizontal(|ui| {
        let status = if active.stopping {
            "Canceling"
        } else if active.finalizing {
            "Finishing"
        } else {
            verb
        };
        ui.add(egui::Label::new(format!("{status} {}", active.name)).truncate()).on_hover_text(&active.name);
        let fraction =
            if active.total == 0 { f32::from(active.finalizing) } else { active.bytes as f32 / active.total as f32 };
        ui.add(egui::ProgressBar::new(fraction).desired_width(105.0).show_percentage());
        if ui.add_enabled(!active.finalizing && !active.stopping, Btn::ghost("Cancel").small()).clicked() {
            active.stopping = active.cancel.request();
            return active.stopping;
        }
        false
    })
    .inner
}

fn posted_date(file: &ChannelFile) -> String {
    chrono::DateTime::from_timestamp_millis(file.attachment.key.posted_ms as i64)
        .map_or_default(|t| t.with_timezone(&chrono::Local).format("%b %-d · %H:%M").to_string())
}

pub(super) fn size_label(size: u64) -> String {
    if size >= GB {
        format!("{:.2} GB", size as f64 / GB as f64)
    } else if size >= 1_000_000 {
        format!("{:.1} MB", size as f64 / 1_000_000.0)
    } else if size >= 1000 {
        format!("{:.1} KB", size as f64 / 1000.0)
    } else {
        format!("{size} B")
    }
}

pub(super) fn limit_from_gb(text: &str) -> Option<u64> {
    let (whole, decimals) = text.trim().split_once('.').unwrap_or((text.trim(), ""));
    if decimals.len() > 9 || !decimals.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let whole = if whole.is_empty() { 0 } else { whole.parse::<u64>().ok()? };
    let fraction = if decimals.is_empty() { 0 } else { decimals.parse::<u64>().ok()? };
    let size = whole.checked_mul(GB)?.checked_add(fraction * 10_u64.pow(9 - decimals.len() as u32))?;
    (1..=MAX_LIMIT).contains(&size).then_some(size)
}

pub(super) fn limit_in_gb(size: u64) -> String {
    let fraction = size % GB;
    let whole = size / GB;
    if fraction == 0 { whole.to_string() } else { format!("{whole}.{fraction:09}").trim_end_matches('0').to_owned() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use t7_proto::{Identity, files::Attachment};

    #[test]
    fn a_live_post_keeps_older_pages_and_the_next_cursor() {
        let make = |n: u8, channel| ChannelFile {
            channel,
            attachment: Attachment {
                key: Cursor { posted_ms: u64::from(n), id: [n; 16] },
                author: Identity([0; 32]),
                nickname: "Mira".into(),
                name: format!("{n}.png"),
                size: 1,
                caption: String::new(),
            },
        };
        let rows: Vec<_> = (2..=49).rev().map(|n| make(n, 1)).collect();
        let next = rows.last().map(|f| f.attachment.key);
        let mut lib = Library { rows, next, scope: Some(1), query: "MIRA".into(), ..Default::default() };
        let post = make(50, 1);
        lib.posted(&post, "Lobby");
        lib.posted(&post, "Lobby");
        lib.posted(&make(50, 2), "Studio");
        assert_eq!(lib.rows.len(), 49);
        assert_eq!(lib.rows[0], post);
        assert_eq!(lib.next, next);

        let delayed = make(1, 1);
        lib.posted(&delayed, "Lobby");
        lib.insert(delayed.clone());
        lib.insert(make(0, 1));
        assert_eq!(lib.rows.len(), 51);
        assert_eq!(lib.rows[49], delayed);
        assert_eq!(lib.rows[50].attachment.key.posted_ms, 0);
    }
}
