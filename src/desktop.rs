mod views;

use crate::{
    credentials::{Keychain, SecretEdit},
    tray,
};
use faxe_engine::{
    Binarization, Cancellation, Destination, DocumentInput, DocumentOptions, Engine, EngineEffect,
    EngineHandle, EngineRuntime, EngineView, FaxMode, FaxRequest, Job, PaperSize, PreparedDocument,
    PreviewSource, Resolution, Settings, SipProfile, SipSender, SipTransport, Uuid,
};
use iced::{Subscription, Task, window};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Compose,
    History,
    Inbox,
    Settings,
}

#[derive(Debug, Clone)]
pub enum Message {
    Opened(window::Id),
    TrayInstalled(Result<(), String>),
    Event(iced::Event),
    Started(Result<Startup, String>),
    EngineChanged(Arc<EngineView>),
    Effect(EngineEffect),
    QueueChanged(Result<Job, String>),
    QueueCleared(Result<usize, String>),
    PreviewSourceLoaded(Uuid, u32, Result<Arc<PreviewSource>, String>),
    TonePreviewLoaded(Uuid, u32, u64, Result<iced::widget::image::Handle, String>),
    Tray(tray::Action),
    Tick,
    Animate(Instant),
    ClearQueue,
    ConfirmClear,
    KeepQueue,
    DismissProgress,
    Details(Uuid),
    Tab(Tab),
    Browse,
    Files(Vec<PathBuf>),
    FilesPicked(Result<Vec<PathBuf>, String>),
    Remove(usize),
    Move(usize, isize),
    Destination(String),
    DestinationName(String),
    SaveDestination,
    UseDestination(Uuid),
    Profile(SipProfile),
    SendingMode(FaxMode),
    Paper(PaperSize),
    Resolution(Resolution),
    Binarization(Binarization),
    Contrast(i16),
    ContrastSettled(u64),
    ApplyContrast,
    Prepare,
    Prepared(u64, Result<PreparedDocument, String>),
    CancelPreparation,
    Preview(u32),
    Enqueue,
    Cancel(Uuid),
    Retry(Uuid),
    NewProfile,
    EditProfile(SipProfile),
    Name(String),
    Server(String),
    Port(String),
    Transport(SipTransport),
    Username(String),
    AuthUsername(String),
    Password(SecretEdit),
    Register(bool),
    Proxy(String),
    Station(String),
    SaveProfile,
    EnableReceive(Uuid, bool),
    ReceiveFolder,
    ReceiveFolderSelected(Option<PathBuf>),
    ReceiveEcm(bool),
    ReceiveMode(FaxMode),
    ReceiveNotifications(bool),
    CancelReception(Uuid),
    RetryExport(Uuid),
    OpenReceived(PathBuf, bool),
    Acknowledgments,
    ActionFinished(Result<(), String>),
    AutomaticNat(bool),
    Stun(String),
    AudioPlayoutDelay(String),
    Stopped(Result<(), String>),
    ProfileSaved(u64, Result<(), String>),
    Hide,
    Quit,
}

pub struct App {
    runtime: Option<EngineRuntime>,
    starting: bool,
    closing: bool,
    operation: u64,
    profile_revision: u64,
    preview_image: Option<iced::widget::image::Handle>,
    preview_source: Option<Arc<PreviewSource>>,
    preview_revision: u64,
    preview_displayed_revision: u64,
    preview_rendering: bool,
    contrast_change: Option<(Instant, u64)>,
    contrast_apply_pending: bool,
    engine: Option<EngineHandle>,
    snapshot: Arc<EngineView>,
    window: Option<window::Id>,
    tray_available: bool,
    tab: Tab,
    paths: Vec<PathBuf>,
    selected_profile: Option<SipProfile>,
    destination: String,
    destination_name: String,
    options: DocumentOptions,
    prepared: Option<PreparedDocument>,
    preparing: Option<Cancellation>,
    preview: u32,
    editor: ProfileEditor,
    saving_profile: bool,
    notice: Option<String>,
    confirm_clear: bool,
    expanded_job: Option<Uuid>,
    progress_job: Option<Job>,
    progress_visible: bool,
    reveal: f32,
    animation_time: Instant,
    finished_at: Option<Instant>,
}

/// A startup result transfers its unique runtime owner into App exactly once.
#[derive(Clone)]
pub struct Startup(Arc<Mutex<Option<(EngineRuntime, Result<Settings, String>)>>>);
impl std::fmt::Debug for Startup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Startup")
    }
}

struct ProfileEditor {
    id: Uuid,
    name: String,
    server: String,
    port: String,
    transport: SipTransport,
    username: String,
    auth_username: String,
    password: SecretEdit,
    register: bool,
    proxy: String,
    station: String,
    automatic_nat: bool,
    stun: String,
    audio_playout_delay_ms: String,
    sending_mode: FaxMode,
}

impl Default for ProfileEditor {
    fn default() -> Self {
        Self {
            id: Uuid::new_v4(),
            name: String::new(),
            server: String::new(),
            port: "5060".into(),
            transport: SipTransport::Udp,
            username: String::new(),
            auth_username: String::new(),
            password: SecretEdit(String::new()),
            register: true,
            proxy: String::new(),
            station: "Faxe".into(),
            automatic_nat: true,
            stun: String::new(),
            audio_playout_delay_ms: "200".into(),
            sending_mode: FaxMode::Auto,
        }
    }
}

impl From<SipProfile> for ProfileEditor {
    fn from(profile: SipProfile) -> Self {
        Self {
            id: profile.id,
            name: profile.name,
            server: profile.server,
            port: profile.port.to_string(),
            transport: profile.transport,
            username: profile.username,
            auth_username: profile.auth_username.unwrap_or_default(),
            password: SecretEdit(String::new()),
            register: profile.register,
            proxy: profile.outbound_proxy.unwrap_or_default(),
            station: profile.station_id,
            automatic_nat: profile.automatic_nat,
            stun: profile.stun_server.unwrap_or_default(),
            audio_playout_delay_ms: profile.audio_playout_delay_ms.to_string(),
            sending_mode: profile.sending_mode,
        }
    }
}

impl ProfileEditor {
    fn profile(&self) -> Result<SipProfile, String> {
        let profile = SipProfile {
            id: self.id,
            name: self.name.trim().into(),
            server: self.server.trim().into(),
            port: self
                .port
                .parse()
                .map_err(|_| "Enter a valid SIP port".to_owned())?,
            transport: self.transport,
            username: self.username.trim().into(),
            auth_username: nonempty(&self.auth_username),
            register: self.register,
            outbound_proxy: nonempty(&self.proxy),
            station_id: self.station.clone(),
            automatic_nat: self.automatic_nat,
            stun_server: nonempty(&self.stun),
            audio_playout_delay_ms: self
                .audio_playout_delay_ms
                .parse()
                .map_err(|_| "Enter an audio receive delay from 40 to 1000 ms".to_owned())?,
            sending_mode: self.sending_mode,
        };
        profile.validate().map_err(|e| e.to_string())?;
        Ok(profile)
    }
}

fn nonempty(value: &str) -> Option<String> {
    match value.trim() {
        "" => None,
        value => Some(value.into()),
    }
}

impl App {
    pub fn boot() -> (Self, Task<Message>) {
        let app = Self {
            runtime: None,
            starting: true,
            closing: false,
            operation: 0,
            profile_revision: 0,
            preview_image: None,
            preview_source: None,
            preview_revision: 0,
            preview_displayed_revision: 0,
            preview_rendering: false,
            contrast_change: None,
            contrast_apply_pending: false,
            engine: None,
            snapshot: Arc::new(EngineView::default()),
            window: None,
            tray_available: false,
            tab: Tab::Compose,
            paths: Vec::new(),
            selected_profile: None,
            destination: String::new(),
            destination_name: String::new(),
            options: DocumentOptions::default(),
            prepared: None,
            preparing: None,
            preview: 1,
            editor: ProfileEditor::default(),
            saving_profile: false,
            notice: None,
            confirm_clear: false,
            expanded_job: None,
            progress_job: None,
            progress_visible: false,
            reveal: 0.0,
            animation_time: Instant::now(),
            finished_at: None,
        };
        let start = Task::perform(
            async {
                tokio::task::spawn_blocking(|| {
                    let runtime = EngineRuntime::open(
                        Engine::data_directory()?,
                        Arc::new(Keychain),
                        Some(Arc::new(SipSender::new()?)),
                    )?;
                    let settings = Settings::load().map_err(|e| e.to_string());
                    Ok::<_, faxe_engine::Error>(Startup(Arc::new(Mutex::new(Some((
                        runtime, settings,
                    ))))))
                })
                .await
                .map_err(|e| e.to_string())?
                .map_err(|e| e.to_string())
            },
            Message::Started,
        );
        (
            app,
            Task::batch([
                open_window(),
                start,
                Task::run(tray::events(), Message::Tray),
            ]),
        )
    }

    pub fn subscription(&self) -> Subscription<Message> {
        let mut subscriptions = vec![iced::event::listen_with(|event, _, _| match event {
            iced::Event::Window(window::Event::FileDropped(_) | window::Event::CloseRequested) => {
                Some(Message::Event(event))
            }
            _ => None,
        })];
        if let Some(change) = self.contrast_change {
            subscriptions.push(Subscription::run_with(change, |&(at, revision)| {
                iced::futures::stream::once(async move {
                    tokio::time::sleep_until((at + Duration::from_millis(350)).into()).await;
                    Message::ContrastSettled(revision)
                })
            }));
        }
        if self.progress_visible
            && let Some(at) = self.finished_at
        {
            subscriptions.push(Subscription::run_with(at, |at| {
                let deadline = *at + Duration::from_secs(8);
                iced::futures::stream::once(async move {
                    tokio::time::sleep_until(deadline.into()).await;
                    Message::Tick
                })
            }));
        }
        if self.window.is_some()
            && match self.progress_visible {
                true => self.reveal < 1.0,
                false => self.reveal > 0.0,
            }
        {
            subscriptions.push(iced::time::every(Duration::from_millis(16)).map(Message::Animate));
        }
        Subscription::batch(subscriptions)
    }

    pub fn update(&mut self, message: Message) -> Task<Message> {
        if self.closing && !matches!(&message, Message::Started(_) | Message::Stopped(_)) {
            return Task::none();
        }
        if matches!(
            &message,
            Message::NewProfile
                | Message::EditProfile(_)
                | Message::Name(_)
                | Message::Server(_)
                | Message::Port(_)
                | Message::Transport(_)
                | Message::Username(_)
                | Message::AuthUsername(_)
                | Message::Password(_)
                | Message::Register(_)
                | Message::Proxy(_)
                | Message::Station(_)
                | Message::AutomaticNat(_)
                | Message::Stun(_)
                | Message::AudioPlayoutDelay(_)
                | Message::SendingMode(_)
        ) {
            self.profile_revision += 1;
        }
        match message {
            Message::Animate(now) => {
                let movement = now.duration_since(self.animation_time).as_secs_f32() / 0.26;
                self.animation_time = now;
                self.reveal = match self.progress_visible {
                    true => (self.reveal + movement).min(1.0),
                    false => (self.reveal - movement).max(0.0),
                };
            }
            Message::DismissProgress => self.hide_progress(),
            Message::Details(id) => {
                self.expanded_job = match self.expanded_job {
                    Some(current) if current == id => None,
                    _ => Some(id),
                }
            }
            Message::ClearQueue => self.confirm_clear = true,
            Message::KeepQueue => self.confirm_clear = false,
            Message::ConfirmClear => {
                self.confirm_clear = false;
                if let Some(engine) = self.engine.clone() {
                    return Task::perform(
                        async move { engine.clear_queue().await.map_err(|e| e.to_string()) },
                        Message::QueueCleared,
                    );
                }
            }
            Message::QueueCleared(result) => {
                self.notice = Some(match result {
                    Ok(count) => format!(
                        "Cleared {count} queue/history entries. Spool files remain recoverable."
                    ),
                    Err(error) => report_error(error),
                });
                self.hide_progress();
            }
            Message::QueueChanged(result) => match result {
                Ok(job) => {
                    self.show_progress(job);
                    self.notice = None;
                }
                Err(error) => self.notice = Some(report_error(error)),
            },
            Message::Started(result) => {
                self.starting = false;
                match result {
                    Ok(startup) => {
                        let Some((mut runtime, settings)) =
                            startup.0.lock().ok().and_then(|mut slot| slot.take())
                        else {
                            return Task::none();
                        };
                        if self.closing {
                            return Task::perform(
                                async move { runtime.shutdown().await.map_err(|e| e.to_string()) },
                                Message::Stopped,
                            );
                        }
                        let engine = runtime.handle();
                        self.apply_view(engine.view());
                        match settings {
                            Ok(settings) => {
                                self.options = settings.document_options;
                                self.selected_profile = self
                                    .snapshot
                                    .profiles
                                    .iter()
                                    .find(|p| Some(p.id) == settings.default_profile)
                                    .cloned()
                                    .or_else(|| self.snapshot.profiles.first().cloned());
                            }
                            Err(error) => self.notice = Some(report_error(error)),
                        }
                        if let Some(profile) = self.selected_profile.clone() {
                            self.editor = profile.into();
                        }
                        let views = iced::futures::stream::unfold(
                            engine.observe(),
                            |mut receiver| async move {
                                receiver.changed().await.ok()?;
                                let view = receiver.borrow_and_update().clone();
                                Some((view, receiver))
                            },
                        );
                        let effects = runtime.take_effects().map_or_else(Task::none, |receiver| {
                            Task::run(
                                iced::futures::stream::unfold(
                                    receiver,
                                    |mut receiver| async move {
                                        receiver.recv().await.map(|event| (event, receiver))
                                    },
                                ),
                                Message::Effect,
                            )
                        });
                        self.engine = Some(engine);
                        self.runtime = Some(runtime);
                        return Task::batch([Task::run(views, Message::EngineChanged), effects]);
                    }
                    Err(error) => {
                        if self.closing {
                            return iced::exit();
                        }
                        self.notice = Some(report_error(error));
                    }
                }
            }
            Message::Effect(effect) => match effect {
                EngineEffect::Error(error) => self.notice = Some(report_error(error)),
                EngineEffect::ReceptionFinished { caller, pages, .. } => {
                    return Task::perform(
                        async move {
                            tokio::task::spawn_blocking(move || {
                                notify_rust::Notification::new()
                                    .summary("FAXE · reception finished")
                                    .body(&format!("{caller} · {pages} recovered page(s)"))
                                    .show()
                                    .map(|_| ())
                                    .map_err(|e| e.to_string())
                            })
                            .await
                            .map_err(|e| e.to_string())?
                        },
                        Message::ActionFinished,
                    );
                }
            },
            Message::Opened(id) => {
                self.window = Some(id);
                if !self.tray_available {
                    return window::run(id, |_| tray::install()).map(Message::TrayInstalled);
                }
            }
            Message::TrayInstalled(result) => match result {
                Ok(()) => self.tray_available = true,
                Err(error) => {
                    tracing::warn!(%error, "Tray unavailable; closing the window will quit")
                }
            },
            Message::Event(iced::Event::Window(window::Event::FileDropped(path))) => {
                return self.update(Message::Files(vec![path]));
            }
            Message::Event(iced::Event::Window(window::Event::CloseRequested)) => {
                return self.update(Message::Hide);
            }
            Message::Event(_) => (),
            Message::EngineChanged(view) => {
                if !self.closing {
                    self.apply_view(view);
                }
            }
            Message::Tray(action) => {
                return match action {
                    tray::Action::Show => match self.window {
                        Some(id) => window::gain_focus(id),
                        None => open_window(),
                    },
                    tray::Action::Quit => self.update(Message::Quit),
                };
            }
            Message::Tick => {
                if self.progress_visible
                    && self
                        .finished_at
                        .is_some_and(|at| at.elapsed() >= Duration::from_secs(8))
                {
                    self.hide_progress();
                }
            }
            Message::Tab(tab) => self.tab = tab,
            Message::Browse => {
                #[cfg(target_os = "windows")]
                if windows::ApplicationModel::Package::Current().is_ok()
                    && let Some(window) = self.window
                {
                    return crate::windows_picker::documents(window).map(Message::FilesPicked);
                }
                return Task::perform(
                    async {
                        rfd::AsyncFileDialog::new()
                            .add_filter("Fax documents", &["pdf", "jpg", "jpeg", "png"])
                            .pick_files()
                            .await
                            .unwrap_or_default()
                            .into_iter()
                            .map(|file| file.path().to_owned())
                            .collect()
                    },
                    Message::Files,
                );
            }
            Message::Files(paths) => {
                if self.preparing.is_none() && !paths.is_empty() {
                    self.paths.extend(paths);
                    self.invalidate_document();
                }
            }
            Message::FilesPicked(result) => match result {
                Ok(paths) => return self.update(Message::Files(paths)),
                Err(error) => self.notice = Some(report_error(error)),
            },
            Message::Remove(index) => {
                if self.preparing.is_none() && index < self.paths.len() {
                    self.paths.remove(index);
                    self.invalidate_document();
                }
            }
            Message::Move(index, offset) => {
                if let Some(target) = index.checked_add_signed(offset)
                    && self.preparing.is_none()
                    && index < self.paths.len()
                    && target < self.paths.len()
                {
                    self.paths.swap(index, target);
                    self.invalidate_document();
                }
            }
            Message::Destination(value) => self.destination = value,
            Message::DestinationName(value) => self.destination_name = value,
            Message::Profile(profile) => {
                self.selected_profile = Some(profile);
                return self.save_settings();
            }
            Message::SendingMode(mode) => self.editor.sending_mode = mode,
            Message::Paper(value) => {
                if self.preparing.is_none() {
                    self.options.paper = value;
                    self.invalidate_document();
                    return self.save_settings();
                }
            }
            Message::Resolution(value) => {
                if self.preparing.is_none() {
                    self.options.resolution = value;
                    self.invalidate_document();
                    return self.save_settings();
                }
            }
            Message::Binarization(value) => {
                if self.preparing.is_none() || self.prepared.is_some() {
                    self.options.binarization = value;
                    let preview = self.refresh_preview();
                    let apply = self.update(Message::ApplyContrast);
                    return Task::batch([preview, apply]);
                }
            }
            Message::Contrast(value) => {
                if self.preparing.is_none() || self.prepared.is_some() {
                    self.options.contrast = value.clamp(-100, 100);
                    let preview = self.refresh_preview();
                    self.contrast_change = Some((Instant::now(), self.preview_revision));
                    return preview;
                }
            }
            Message::ContrastSettled(revision) => {
                if self
                    .contrast_change
                    .is_some_and(|(_, current)| current == revision)
                {
                    return self.update(Message::ApplyContrast);
                }
            }
            Message::ApplyContrast => {
                self.contrast_change = None;
                let settings = self.save_settings();
                if self.preparing.is_some() && self.prepared.is_some() {
                    self.contrast_apply_pending = true;
                    return settings;
                }
                if self
                    .prepared
                    .as_ref()
                    .is_some_and(|document| document.options != self.options)
                {
                    let prepare = self.update(Message::Prepare);
                    return Task::batch([settings, prepare]);
                }
                return settings;
            }
            Message::SaveDestination => {
                if let (Some(engine), Some(profile)) = (self.engine.clone(), &self.selected_profile)
                {
                    let destination = Destination {
                        id: Uuid::new_v4(),
                        name: self.destination_name.clone(),
                        address: self.destination.clone(),
                        profile_id: profile.id,
                    };
                    return Task::perform(
                        async move {
                            engine
                                .save_destination(destination)
                                .await
                                .map_err(|e| e.to_string())
                        },
                        Message::ActionFinished,
                    );
                }
            }
            Message::UseDestination(id) => {
                if let Some(destination) = self
                    .snapshot
                    .destinations
                    .iter()
                    .find(|destination| destination.id == id)
                {
                    self.destination = destination.address.clone();
                    self.selected_profile = self
                        .snapshot
                        .profiles
                        .iter()
                        .find(|profile| profile.id == destination.profile_id)
                        .cloned();
                    return self.save_settings();
                }
            }
            Message::Prepare => {
                if self.preparing.is_none()
                    && let Some(engine) = self.engine.clone()
                {
                    let cancellation = Cancellation::default();
                    self.operation += 1;
                    let operation = self.operation;
                    self.preparing = Some(cancellation.clone());
                    let original = self.prepared.as_ref().map(|document| document.id);
                    self.notice = None;
                    let input = DocumentInput {
                        paths: self.paths.clone(),
                        options: self.options,
                    };
                    return Task::perform(
                        async move {
                            match original {
                                Some(id) => {
                                    engine
                                        .adjust_document(id, input.options, cancellation)
                                        .await
                                }
                                None => engine.prepare(input, cancellation).await,
                            }
                            .map_err(|error| error.to_string())
                        },
                        move |result| Message::Prepared(operation, result),
                    );
                }
            }
            Message::Prepared(operation, result) => {
                if self.closing || operation != self.operation {
                    return Task::none();
                }
                self.preparing = None;
                let apply_pending = std::mem::take(&mut self.contrast_apply_pending);
                match result {
                    Ok(document) => {
                        let adjusting = self.prepared.is_some();
                        self.prepared = Some(document);
                        let preview = if adjusting && self.preview_source.is_some() {
                            // The cached grayscale page is unchanged by tone edits.
                            self.refresh_preview()
                        } else {
                            if !adjusting {
                                self.preview = 1;
                            }
                            self.load_preview()
                        };
                        let apply = if apply_pending {
                            self.update(Message::ApplyContrast)
                        } else {
                            Task::none()
                        };
                        return Task::batch([preview, apply]);
                    }
                    Err(error) => self.notice = Some(report_error(error)),
                }
            }
            Message::CancelPreparation => {
                self.contrast_apply_pending = false;
                self.contrast_change = None;
                if let Some(cancellation) = &self.preparing {
                    cancellation.cancel();
                }
            }
            Message::Preview(page) => {
                self.preview = page;
                return self.load_preview();
            }
            Message::PreviewSourceLoaded(id, page, result) => {
                if !self.closing
                    && self.prepared.as_ref().is_some_and(|d| d.id == id)
                    && self.preview == page
                {
                    match result {
                        Ok(source) => {
                            self.preview_source = Some(source);
                            return self.refresh_preview();
                        }
                        Err(error) => {
                            tracing::error!(document_id = %id, page, %error, "Document preview failed");
                            self.notice = Some(report_error(error));
                        }
                    }
                }
            }
            Message::TonePreviewLoaded(id, page, revision, result) => {
                self.preview_rendering = false;
                let current_page = self
                    .prepared
                    .as_ref()
                    .is_some_and(|document| document.id == id)
                    && self.preview == page;
                if current_page && revision > self.preview_displayed_revision {
                    match result {
                        Ok(image) => {
                            self.preview_image = Some(image);
                            self.preview_displayed_revision = revision;
                        }
                        Err(error) if revision == self.preview_revision => {
                            self.notice = Some(report_error(error))
                        }
                        Err(_) => (),
                    }
                }
                // Show each newer completed frame while coalescing to the latest
                // setting. Requiring an exact revision would starve Photo previews
                // when slider events arrive faster than a frame can be rendered.
                if !current_page || revision != self.preview_revision {
                    return self.render_preview();
                }
            }
            Message::Enqueue => {
                if let (Some(engine), Some(profile), Some(document)) =
                    (self.engine.clone(), &self.selected_profile, &self.prepared)
                    && self.preparing.is_none()
                    && document.options == self.options
                {
                    let request = FaxRequest {
                        profile_id: profile.id,
                        destination: self.destination.trim().into(),
                        mode: profile.sending_mode,
                        document: document.clone(),
                    };
                    return Task::perform(
                        async move { engine.enqueue(request).await.map_err(|e| e.to_string()) },
                        Message::QueueChanged,
                    );
                }
            }
            Message::Cancel(id) => {
                if let Some(engine) = self.engine.clone() {
                    return Task::perform(
                        async move { engine.cancel(id).await.map_err(|e| e.to_string()) },
                        Message::ActionFinished,
                    );
                }
            }
            Message::Retry(id) => {
                if let Some(engine) = self.engine.clone() {
                    return Task::perform(
                        async move { engine.retry(id).await.map_err(|e| e.to_string()) },
                        Message::QueueChanged,
                    );
                }
            }
            Message::AutomaticNat(value) => self.editor.automatic_nat = value,
            Message::AudioPlayoutDelay(value) => self.editor.audio_playout_delay_ms = value,
            Message::Stun(value) => self.editor.stun = value,
            Message::EnableReceive(id, enabled) => {
                let mut settings = self.snapshot.receive_settings.clone();
                settings.profile = enabled.then_some(id);
                return self.configure_receive(settings);
            }
            Message::ReceiveEcm(ecm) => {
                let mut settings = self.snapshot.receive_settings.clone();
                settings.ecm = ecm;
                return self.configure_receive(settings);
            }
            Message::ReceiveMode(mode) => {
                let mut settings = self.snapshot.receive_settings.clone();
                settings.mode = mode;
                return self.configure_receive(settings);
            }
            Message::ReceiveNotifications(notifications) => {
                let mut settings = self.snapshot.receive_settings.clone();
                settings.notifications = notifications;
                return self.configure_receive(settings);
            }
            Message::ReceiveFolder => {
                return Task::perform(
                    async {
                        rfd::AsyncFileDialog::new()
                            .pick_folder()
                            .await
                            .map(|folder| folder.path().to_owned())
                    },
                    Message::ReceiveFolderSelected,
                );
            }
            Message::ReceiveFolderSelected(folder) => {
                if folder.is_some() {
                    let mut settings = self.snapshot.receive_settings.clone();
                    settings.folder = folder;
                    return self.configure_receive(settings);
                }
            }
            Message::CancelReception(id) => {
                if let Some(engine) = self.engine.clone() {
                    return Task::perform(
                        async move { engine.cancel_reception(id).await.map_err(|e| e.to_string()) },
                        Message::ActionFinished,
                    );
                }
            }
            Message::RetryExport(id) => {
                if let Some(engine) = self.engine.clone() {
                    return Task::perform(
                        async move { engine.retry_export(id).await.map_err(|e| e.to_string()) },
                        Message::ActionFinished,
                    );
                }
            }
            Message::OpenReceived(path, reveal) => {
                return Task::perform(
                    async move {
                        tokio::task::spawn_blocking(move || open_path(&path, reveal))
                            .await
                            .map_err(|e| e.to_string())?
                    },
                    Message::ActionFinished,
                );
            }
            Message::Acknowledgments => {
                return Task::perform(
                    async {
                        tokio::task::spawn_blocking(crate::acknowledgments::open_page)
                            .await
                            .map_err(|error| error.to_string())?
                    },
                    Message::ActionFinished,
                );
            }
            Message::ActionFinished(result) => {
                if let Err(error) = result {
                    self.notice = Some(report_error(error));
                }
            }
            Message::Stopped(result) => {
                if let Err(error) = result {
                    tracing::error!(%error,"Engine shutdown failed");
                }
                return iced::exit();
            }
            Message::NewProfile => self.editor = ProfileEditor::default(),
            Message::EditProfile(profile) => self.editor = profile.into(),
            Message::Name(value) => self.editor.name = value,
            Message::Server(value) => self.editor.server = value,
            Message::Port(value) => self.editor.port = value,
            Message::Transport(value) => {
                if self.editor.port == self.editor.transport.default_port().to_string() {
                    self.editor.port = value.default_port().to_string();
                }
                self.editor.transport = value;
            }
            Message::Username(value) => self.editor.username = value,
            Message::AuthUsername(value) => self.editor.auth_username = value,
            Message::Password(value) => self.editor.password = value,
            Message::Register(value) => self.editor.register = value,
            Message::Proxy(value) => self.editor.proxy = value,
            Message::Station(value) => self.editor.station = value,
            Message::SaveProfile if self.saving_profile => return Task::none(),
            Message::SaveProfile => match self.editor.profile() {
                Ok(profile) => {
                    if let Some(engine) = self.engine.clone() {
                        self.saving_profile = true;
                        let revision = self.profile_revision;
                        let password = self.editor.password.clone();
                        return Task::perform(
                            async move {
                                let password = (!password.0.is_empty())
                                    .then(|| faxe_engine::Password::new(password.0));
                                engine
                                    .save_profile_with_password(profile, password)
                                    .await
                                    .map_err(|e| e.to_string())
                            },
                            move |result| Message::ProfileSaved(revision, result),
                        );
                    }
                }
                Err(error) => self.notice = Some(report_error(error)),
            },
            Message::ProfileSaved(revision, result) => {
                self.saving_profile = false;
                if revision == self.profile_revision && result.is_ok() {
                    self.editor.password.0.clear();
                }
                self.notice = Some(match result {
                    Ok(()) => "SIP profile saved.".into(),
                    Err(error) => report_error(error),
                });
                if self.selected_profile.is_none() {
                    self.selected_profile = self.snapshot.profiles.first().cloned();
                }
                return self.save_settings();
            }
            Message::Hide => match (self.tray_available, self.window.take()) {
                (true, Some(id)) => return window::close(id),
                (true, None) => (),
                (false, _) => return self.update(Message::Quit),
            },
            Message::Quit => {
                if self.closing {
                    return Task::none();
                }
                self.closing = true;
                self.operation += 1;
                if let Some(cancellation) = &self.preparing {
                    cancellation.cancel();
                }
                self.engine.take();
                if let Some(runtime) = self.runtime.take() {
                    return Task::perform(
                        async move { runtime.shutdown().await.map_err(|e| e.to_string()) },
                        Message::Stopped,
                    );
                }
                if !self.starting {
                    return iced::exit();
                }
            }
        }
        Task::none()
    }

    fn apply_view(&mut self, snapshot: Arc<EngineView>) {
        if let Some(selected) = &self.selected_profile {
            self.selected_profile = snapshot
                .profiles
                .iter()
                .find(|p| p.id == selected.id)
                .cloned();
        }
        self.snapshot = snapshot;
        if self.selected_profile.is_none() {
            self.selected_profile = self.snapshot.profiles.first().cloned();
        }
        if self.snapshot.receive_settings.profile.is_none() && self.tab == Tab::Inbox {
            self.tab = Tab::Compose;
        }
        let active = self
            .snapshot
            .jobs
            .iter()
            .find(|job| job.state.is_active())
            .cloned();
        match active {
            Some(job) => {
                if self
                    .progress_job
                    .as_ref()
                    .is_none_or(|current| current.id != job.id)
                {
                    self.show_progress(job);
                } else {
                    self.progress_job = Some(job);
                }
            }
            None => {
                if let Some(previous) = &self.progress_job
                    && let Some(job) = self
                        .snapshot
                        .jobs
                        .iter()
                        .find(|job| job.id == previous.id)
                        .cloned()
                {
                    if job.state.is_finished() && self.finished_at.is_none() {
                        self.finished_at = Some(Instant::now());
                    }
                    self.progress_job = Some(job);
                }
            }
        }
    }
    fn configure_receive(&mut self, settings: faxe_engine::ReceiveSettings) -> Task<Message> {
        if let Some(engine) = self.engine.clone() {
            Task::perform(
                async move {
                    engine
                        .configure_receiver(settings)
                        .await
                        .map_err(|e| e.to_string())
                },
                Message::ActionFinished,
            )
        } else {
            Task::none()
        }
    }
    fn load_preview(&mut self) -> Task<Message> {
        self.preview_image = None;
        self.preview_source = None;
        self.preview_revision += 1;
        self.preview_displayed_revision = self.preview_revision;
        if let (Some(engine), Some(document)) = (self.engine.clone(), self.prepared.as_ref()) {
            let id = document.id;
            let page = self.preview;
            Task::perform(
                async move {
                    tokio::task::spawn_blocking(move || {
                        engine.documents().preview_source(id, page).map(Arc::new)
                    })
                    .await
                    .map_err(|error| error.to_string())?
                    .map_err(|error| error.to_string())
                },
                move |result| Message::PreviewSourceLoaded(id, page, result),
            )
        } else {
            Task::none()
        }
    }

    fn invalidate_document(&mut self) {
        self.prepared = None;
        self.preview_source = None;
        self.preview_image = None;
        self.preview_revision += 1;
        self.preview_displayed_revision = self.preview_revision;
        self.contrast_change = None;
        self.contrast_apply_pending = false;
    }

    fn refresh_preview(&mut self) -> Task<Message> {
        self.preview_revision += 1;
        self.render_preview()
    }

    fn render_preview(&mut self) -> Task<Message> {
        if self.preview_rendering {
            return Task::none();
        }
        let (Some(document), Some(source)) = (&self.prepared, self.preview_source.clone()) else {
            return Task::none();
        };
        self.preview_rendering = true;
        let (id, page, revision) = (document.id, self.preview, self.preview_revision);
        let options = self.options;
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || {
                    let preview = source.render(options.binarization, options.contrast)?;
                    Ok::<_, faxe_engine::Error>(iced::widget::image::Handle::from_rgba(
                        preview.width,
                        preview.height,
                        preview.pixels,
                    ))
                })
                .await
                .map_err(|error| error.to_string())?
                .map_err(|error| error.to_string())
            },
            move |result| Message::TonePreviewLoaded(id, page, revision, result),
        )
    }
    fn receiver_label(&self) -> String {
        let status = &self.snapshot.receiver_status;
        let profile = self
            .snapshot
            .profiles
            .iter()
            .find(|profile| Some(profile.id) == status.profile);
        let label = if matches!(status.state, faxe_engine::ReceiverState::Error { .. }) {
            "Error".into()
        } else {
            status.to_string()
        };
        match profile {
            Some(profile) => format!("{label} · {}", profile.name),
            None => label,
        }
    }

    fn show_progress(&mut self, job: Job) {
        self.progress_job = Some(job);
        self.progress_visible = true;
        self.finished_at = None;
        self.animation_time = Instant::now();
    }

    fn hide_progress(&mut self) {
        self.progress_visible = false;
        self.animation_time = Instant::now();
    }

    fn save_settings(&mut self) -> Task<Message> {
        let settings = Settings {
            document_options: self.options,
            default_profile: self.selected_profile.as_ref().map(|p| p.id),
        };
        if let Some(engine) = self.engine.clone() {
            Task::perform(
                async move {
                    engine
                        .save_settings(settings)
                        .await
                        .map_err(|e| e.to_string())
                },
                Message::ActionFinished,
            )
        } else {
            Task::none()
        }
    }
}

pub fn theme() -> iced::Theme {
    iced::Theme::custom(
        "FAXE",
        iced::theme::Palette {
            background: iced::color!(0x222831),
            text: iced::color!(0xEEEEEE),
            primary: iced::color!(0x76ABAE),
            success: iced::color!(0x76ABAE),
            warning: iced::color!(0xD6BB86),
            danger: iced::color!(0xE59A9A),
        },
    )
}

#[cfg(test)]
mod preview_tests {
    use super::*;

    #[test]
    fn stale_slider_results_and_idle_events_do_not_replace_newer_edits() {
        let (mut app, _) = App::boot();
        let id = Uuid::new_v4();
        app.prepared = Some(PreparedDocument {
            id,
            pages: 2,
            source_names: vec!["image.png".into()],
            options: DocumentOptions::default(),
        });
        app.preview_rendering = true;
        let old_revision = app.preview_revision;
        let _ = app.update(Message::Contrast(25));
        let revision = app.preview_revision;
        let _ = app.update(Message::Contrast(50));
        assert!(app.preview_rendering);
        let _ = app.update(Message::ContrastSettled(revision));
        assert_eq!(app.contrast_change.unwrap().1, app.preview_revision);
        let image = iced::widget::image::Handle::from_rgba(1, 1, vec![0, 0, 0, 255]);
        let _ = app.update(Message::TonePreviewLoaded(
            id,
            1,
            old_revision,
            Ok(image.clone()),
        ));
        assert!(app.preview_image.is_none());
        assert!(!app.preview_rendering);
        // A completed intermediate frame must appear during continuous dragging,
        // even when a more recent setting is already waiting to be rendered.
        let _ = app.update(Message::TonePreviewLoaded(
            id,
            1,
            revision,
            Ok(image.clone()),
        ));
        assert!(app.preview_image.is_some());
        assert_eq!(app.preview_displayed_revision, revision);
        let _ = app.update(Message::TonePreviewLoaded(
            id,
            1,
            app.preview_revision,
            Ok(image.clone()),
        ));
        assert!(app.preview_image.is_some());
        let _ = app.update(Message::TonePreviewLoaded(
            id,
            1,
            revision,
            Ok(image.clone()),
        ));
        assert_eq!(app.preview_displayed_revision, app.preview_revision);
        app.preview = 2;
        app.preview_image = None;
        let _ = app.update(Message::TonePreviewLoaded(
            id,
            1,
            app.preview_revision,
            Ok(image),
        ));
        assert!(app.preview_image.is_none());
        app.preparing = Some(Cancellation::default());
        let _ = app.update(Message::Contrast(0));
        assert_eq!(app.options.contrast, 0);
        // Returning to the old setting while another setting is being saved
        // must still apply the latest value after that save finishes.
        let _ = app.update(Message::ApplyContrast);
        assert!(app.contrast_apply_pending);
        let _ = app.update(Message::CancelPreparation);
        assert!(!app.contrast_apply_pending);
        app.invalidate_document();
        assert!(app.prepared.is_none() && app.contrast_change.is_none());
    }
}

fn open_window() -> Task<Message> {
    let (_, task) = window::open(window::Settings {
        icon: Some(crate::icons::window()),
        size: iced::Size::new(1120.0, 820.0),
        min_size: Some(iced::Size::new(900.0, 650.0)),
        exit_on_close_request: false,
        #[cfg(target_os = "linux")]
        platform_specific: window::settings::PlatformSpecific {
            application_id: "com.coral.faxe".into(),
            ..Default::default()
        },
        ..window::Settings::default()
    });
    task.map(Message::Opened)
}

fn open_path(path: &std::path::Path, reveal: bool) -> Result<(), String> {
    let mut command;
    #[cfg(target_os = "macos")]
    {
        command = std::process::Command::new("open");
        if reveal {
            command.arg("-R");
        }
        command.arg(path);
    }
    #[cfg(target_os = "windows")]
    {
        command = std::process::Command::new("explorer");
        if reveal {
            command.arg(format!("/select,{}", path.display()));
        } else {
            command.arg(path);
        }
    }
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    {
        command = std::process::Command::new("xdg-open");
        command.arg(if reveal {
            path.parent().unwrap_or(path)
        } else {
            path
        });
    }
    let status = command.status().map_err(|e| e.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err("Could not open the file in its default application".into())
    }
}

// Keep user-visible errors available even when RUST_LOG suppresses tracing.
fn report_error(error: String) -> String {
    eprintln!("FAXE error: {error}");
    error
}
