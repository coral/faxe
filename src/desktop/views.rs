use super::{App, Message, Tab};
use crate::credentials::SecretEdit;
use faxe_engine::{
    Binarization, FaxMode, FaxStage, Job, JobState, PaperSize, Resolution, SipTransport,
};
use iced::{
    Element,
    Length::{Fill, FillPortion},
    alignment::Vertical,
    widget::{
        button, checkbox, column, container, image, pick_list, progress_bar, row, rule, scrollable,
        space, text, text_input,
    },
    window,
};

impl App {
    pub fn view(&self, _window: window::Id) -> Element<'_, Message> {
        let mut navigation = row![
            text("FAXE").size(30).font(iced::Font {
                weight: iced::font::Weight::Bold,
                ..Default::default()
            }),
            space().width(28),
            nav("New fax", Tab::Compose, self.tab),
            nav("Queue & history", Tab::History, self.tab),
        ]
        .spacing(8)
        .align_y(Vertical::Center);
        if self.snapshot.receive_settings.profile.is_some() {
            navigation = navigation.push(nav("Inbox", Tab::Inbox, self.tab));
        }
        navigation = navigation
            .push(nav("Settings", Tab::Settings, self.tab))
            .push(space().width(Fill))
            .push(
                button(text(self.receiver_label()).size(13))
                    .style(button::text)
                    .on_press(Message::Tab(Tab::Settings)),
            );
        let mut content = column![navigation, rule::horizontal(1)].spacing(18);
        if !self.snapshot.sending_available {
            content = content.push(
                container(text(
                    "Fax engine unavailable. Resolve the startup error before sending.",
                ))
                .padding(12)
                .width(Fill)
                .style(container::rounded_box),
            );
        }
        if let Some(notice) = &self.notice {
            content = content.push(text(notice).size(14));
        }
        content = content.push(match self.tab {
            Tab::Compose => self.compose(),
            Tab::History => self.history(),
            Tab::Inbox => self.inbox(),
            Tab::Settings => self.settings(),
        });
        if let Some(job) = &self.progress_job
            && self.reveal > 0.0
        {
            let eased = 1.0 - (1.0 - self.reveal).powi(3);
            let footer = column![
                row![
                    text(format!("Sending · {}", job.request.destination)).size(18),
                    space().width(Fill),
                    button("View queue")
                        .style(button::text)
                        .on_press(Message::Tab(Tab::History)),
                    button("×")
                        .style(button::text)
                        .on_press(Message::DismissProgress)
                ]
                .align_y(Vertical::Center),
                row![
                    text(match job.state {
                        JobState::Failed { .. } => "Fax failed · view queue for details".into(),
                        _ => status(job),
                    })
                    .size(14)
                    .width(Fill),
                    text(page_status(job)).size(13)
                ]
                .spacing(12),
                progress_bar(0.0..=1.0, progress(job))
                    .girth(6)
                    .style(move |theme| progress_style(theme, job)),
            ]
            .spacing(9);
            content = content.push(
                scrollable(
                    container(footer)
                        .padding(16)
                        .width(Fill)
                        .height(124)
                        .style(surface),
                )
                .height(124.0 * eased)
                .width(Fill)
                .direction(scrollable::Direction::Vertical(
                    scrollable::Scrollbar::new().width(0).scroller_width(0),
                )),
            );
        }
        if let Some(fax) = self
            .snapshot
            .inbox
            .iter()
            .find(|fax| fax.outcome.is_active())
        {
            content = content.push(
                container(
                    row![
                        column![
                            text(format!("Receiving · {}", fax.caller)).size(18),
                            text(format!(
                                "{} received page(s) · {}",
                                fax.confirmed_pages,
                                fax.transport
                                    .map(|m| m.to_string())
                                    .unwrap_or_else(|| "Negotiating fax".into())
                            ))
                            .size(14)
                        ]
                        .spacing(8)
                        .width(Fill),
                        button("Inbox")
                            .style(button::text)
                            .on_press(Message::Tab(Tab::Inbox)),
                        button("Cancel")
                            .style(button::secondary)
                            .on_press(Message::CancelReception(fax.id))
                    ]
                    .spacing(12)
                    .align_y(Vertical::Center),
                )
                .padding(16)
                .width(Fill)
                .style(surface),
            );
        }
        container(content)
            .padding(24)
            .width(Fill)
            .height(Fill)
            .into()
    }

    fn inbox(&self) -> Element<'_, Message> {
        use faxe_engine::{ExportStatus, ReceptionOutcome};
        let mut list = column![text("Inbox").size(26)].spacing(14);
        if self.snapshot.inbox.is_empty() {
            list = list.push(text("Incoming faxes will appear here."));
        }
        for fax in self.snapshot.inbox.iter() {
            let outcome = match &fax.outcome {
                ReceptionOutcome::Receiving => "Receiving".into(),
                ReceptionOutcome::Received => "Received".into(),
                ReceptionOutcome::Interrupted => "Interrupted".into(),
                ReceptionOutcome::Partial { reason } => format!("Partial · {reason}"),
                ReceptionOutcome::Failed { reason } => format!("Failed · {reason}"),
            };
            let mut card = column![
                text(&fax.caller).size(19),
                text(format!("{} · {}", fax.filename_stem, fax.profile_name)).size(12),
                text(outcome),
                text(format!(
                    "{} confirmed page(s) · {} recovered image(s) · {}",
                    fax.confirmed_pages,
                    fax.recovered_pages,
                    fax.transport
                        .map(|mode| mode.to_string())
                        .unwrap_or_else(|| "No media".into())
                ))
                .size(13)
            ]
            .spacing(8);
            if let Some(faxe_engine::ReceptionResult::Failed { reason, .. }) = &fax.result {
                card = card.push(text(format!("Call result · {reason}")).size(13));
            }
            card = match &fax.export {
                ExportStatus::Published { path } => card.push(
                    row![
                        button("Open PDF").on_press(Message::OpenReceived(path.clone(), false)),
                        button("Show in folder")
                            .style(button::secondary)
                            .on_press(Message::OpenReceived(path.clone(), true))
                    ]
                    .spacing(8),
                ),
                ExportStatus::Failed { reason } => card
                    .push(text(format!("Export failed · {reason}")))
                    .push(button("Retry export").on_press(Message::RetryExport(fax.id))),
                ExportStatus::Publishing { error, .. } => card
                    .push(text(
                        error
                            .as_ref()
                            .map(|error| format!("Export failed · {error}"))
                            .unwrap_or_else(|| "Publishing PDF · internal copy retained".into()),
                    ))
                    .push(button("Retry export").on_press(Message::RetryExport(fax.id))),
                ExportStatus::NoContent => {
                    card.push(text("No readable image content was recovered"))
                }
                ExportStatus::Pending if !fax.outcome.is_active() => {
                    card.push(text("Preparing PDF…"))
                }
                _ => card,
            };
            if fax.outcome.is_active() {
                card = card
                    .push(button("Cancel reception").on_press(Message::CancelReception(fax.id)));
            }
            list = list.push(container(card).padding(16).width(Fill).style(surface));
        }
        scrollable(list).height(Fill).into()
    }

    fn compose(&self) -> Element<'_, Message> {
        let mut attachments = column![
            text("Documents").size(24),
            text("Drop PDFs, JPEGs or PNGs here. Files send in the order below.").size(14)
        ]
        .spacing(10);
        for (index, path) in self.paths.iter().enumerate() {
            attachments = attachments.push(
                row![
                    text(format!(
                        "{}. {}",
                        index + 1,
                        path.file_name().unwrap_or_default().to_string_lossy()
                    ))
                    .width(Fill),
                    button("↑").on_press_maybe(
                        (index > 0 && self.preparing.is_none()).then_some(Message::Move(index, -1))
                    ),
                    button("↓").on_press_maybe(
                        (index + 1 < self.paths.len() && self.preparing.is_none())
                            .then_some(Message::Move(index, 1))
                    ),
                    button("Remove")
                        .on_press_maybe(self.preparing.is_none().then_some(Message::Remove(index))),
                ]
                .spacing(6)
                .align_y(Vertical::Center),
            );
        }
        attachments = attachments.push(
            button("Add documents…")
                .on_press_maybe(self.preparing.is_none().then_some(Message::Browse)),
        );
        let options = row![
            pick_list(PaperSize::ALL, Some(self.options.paper), Message::Paper),
            pick_list(
                Resolution::ALL,
                Some(self.options.resolution),
                Message::Resolution
            ),
            pick_list(
                Binarization::ALL,
                Some(self.options.binarization),
                Message::Binarization
            ),
        ]
        .spacing(8);
        let prepare: Element<'_, Message> = match &self.preparing {
            Some(_) => row![
                text("Preparing pages…"),
                button("Cancel").on_press(Message::CancelPreparation)
            ]
            .spacing(12)
            .into(),
            None => button("Prepare & preview")
                .on_press_maybe(
                    (!self.paths.is_empty() && self.engine.is_some()).then_some(Message::Prepare),
                )
                .into(),
        };
        let mut route = column![
            text("Send to").size(24),
            pick_list(
                self.snapshot.profiles.as_ref().clone(),
                self.selected_profile.clone(),
                Message::Profile
            )
            .placeholder("Choose a SIP profile")
            .width(Fill),
            text_input("Fax number or sip:user@server", &self.destination)
                .on_input(Message::Destination),
            row![
                text_input("Save destination as…", &self.destination_name)
                    .on_input(Message::DestinationName),
                button("Save").on_press(Message::SaveDestination)
            ]
            .spacing(8),
        ]
        .spacing(10);
        for destination in self.snapshot.destinations.iter() {
            route = route.push(
                button(text(&destination.name)).on_press(Message::UseDestination(destination.id)),
            );
        }
        route = route.push(
            button("Send fax").on_press_maybe(
                (self.prepared.is_some()
                    && self.selected_profile.is_some()
                    && !self.destination.is_empty()
                    && self.preparing.is_none())
                .then_some(Message::Enqueue),
            ),
        );
        let left = scrollable(
            column![attachments, options, prepare, rule::horizontal(1), route].spacing(20),
        )
        .width(FillPortion(3));
        let preview: Element<'_, Message> = match (&self.prepared, &self.engine) {
            (Some(document), Some(_engine)) => column![
                row![
                    button("←").on_press_maybe(
                        (self.preview > 1).then(|| Message::Preview(self.preview - 1))
                    ),
                    text(format!("Page {} of {}", self.preview, document.pages)),
                    button("→").on_press_maybe(
                        (self.preview < document.pages).then(|| Message::Preview(self.preview + 1))
                    )
                ]
                .spacing(12)
                .align_y(Vertical::Center),
                scrollable(match &self.preview_image {
                    Some(handle) => Element::from(image(handle.clone()).width(Fill)),
                    None => Element::from(text("Loading preview…")),
                }),
            ]
            .spacing(12)
            .into(),
            _ => container(column![text("Preview").size(24),].spacing(12))
                .padding(30)
                .center_y(Fill)
                .into(),
        };
        row![
            left,
            container(preview)
                .padding(20)
                .width(FillPortion(2))
                .height(Fill)
                .style(surface)
        ]
        .spacing(28)
        .height(Fill)
        .into()
    }

    fn history(&self) -> Element<'_, Message> {
        let mut list = column![
            row![
                text("Queue & history").size(26),
                space().width(Fill),
                button("Clear queue")
                    .style(button::secondary)
                    .on_press_maybe(
                        (!self.snapshot.jobs.is_empty()).then_some(Message::ClearQueue)
                    )
            ]
            .align_y(Vertical::Center)
        ]
        .spacing(16);
        if self.confirm_clear {
            list = list.push(container(column![
                text("Clear the queue and history?").size(18),
                text("Pending faxes will be removed and the active fax will be cancelled. Prepared documents and SIP profiles are kept.").size(14),
                row![button("Keep queue").style(button::secondary).on_press(Message::KeepQueue), button("Clear and cancel").style(button::danger).on_press(Message::ConfirmClear)].spacing(10)
            ].spacing(12)).padding(18).width(Fill).style(surface));
        }
        if self.snapshot.jobs.is_empty() {
            list = list.push(text(
                "No faxes yet. Prepare a document to create your first job.",
            ));
        }
        for job in self.snapshot.jobs.iter() {
            list = list.push(job_row(job, self.expanded_job == Some(job.id)));
        }
        scrollable(list).height(Fill).into()
    }

    fn settings(&self) -> Element<'_, Message> {
        let mut profiles = column![
            text("SIP profiles").size(20),
            button("New profile").on_press(Message::NewProfile)
        ]
        .spacing(12);
        let mut profile_list = column![].spacing(0);
        for profile in self.snapshot.profiles.iter() {
            let selected = self.editor.id == profile.id;
            profile_list = profile_list.push(
                button(
                    row![
                        container(space().width(3).height(36)).style(move |_| container::Style {
                            background: selected.then(|| iced::color!(0x76ABAE).into()),
                            ..Default::default()
                        }),
                        column![
                            text(&profile.name).size(16),
                            text(format!("{} · {}", profile.server, profile.transport)).size(12)
                        ]
                        .spacing(5)
                        .width(Fill)
                    ]
                    .spacing(12)
                    .align_y(Vertical::Center),
                )
                .width(Fill)
                .padding(14)
                .style(move |theme, state| {
                    let mut style = button::text(theme, state);
                    style.text_color = iced::color!(0xEEEEEE);
                    style.background = (selected || matches!(state, button::Status::Hovered))
                        .then(|| iced::color!(0x31363F).into());
                    style.border = iced::Border::default();
                    style
                })
                .on_press(Message::EditProfile(profile.clone())),
            );
        }
        profiles = profiles.push(profile_list);
        let profile_id = self.editor.id;
        let saved_profile = self
            .snapshot
            .profiles
            .iter()
            .find(|profile| profile.id == profile_id);
        let receive_enabled = self.snapshot.receive_settings.profile == Some(profile_id);
        let can_enable = self.engine.is_some()
            && saved_profile.is_some_and(|profile| profile.register)
            && self.snapshot.receive_settings.folder.is_some();
        let mut receiving = column![
            checkbox(receive_enabled)
                .label("Enable receiving")
                .on_toggle_maybe(
                    (receive_enabled || can_enable)
                        .then_some(move |enabled| Message::EnableReceive(profile_id, enabled))
                )
        ]
        .spacing(10);
        if receive_enabled {
            receiving = receiving.push(
                checkbox(self.snapshot.receive_settings.ecm)
                    .label("Error correction (ECM)")
                    .on_toggle(Message::ReceiveEcm),
            );
        } else if saved_profile.is_none() {
            receiving = receiving.push(text("Save this profile to enable receiving.").size(13));
        } else if !saved_profile.is_some_and(|profile| profile.register) {
            receiving = receiving
                .push(text("Enable registration and save this profile to receive faxes.").size(13));
        }
        if self.snapshot.receiver_status.profile == Some(profile_id) {
            receiving = receiving.push(text(self.snapshot.receiver_status.to_string()).size(13));
        }
        let editor = column![
            text(match self.editor.name.is_empty() { true => "New SIP profile", false => &self.editor.name }).size(24),
            row![
                text("Sending mode").width(Fill),
                pick_list(FaxMode::ALL, Some(self.editor.sending_mode), Message::SendingMode)
            ].spacing(16).align_y(Vertical::Center),
            receiving,
            rule::horizontal(1),
            text("Connection").size(14),
            text_input("Profile name", &self.editor.name).on_input(Message::Name),
            row![text_input("SIP server hostname or IP", &self.editor.server).on_input(Message::Server),
                text_input("Port", &self.editor.port).on_input(Message::Port).width(90),
                pick_list(SipTransport::ALL, Some(self.editor.transport), Message::Transport)].spacing(8),
            text_input("SIP username / local identity", &self.editor.username).on_input(Message::Username),
            text_input("Authentication username (optional)", &self.editor.auth_username).on_input(Message::AuthUsername),
            text_input("Password (blank keeps existing)", &self.editor.password.0).secure(true).on_input(|value| Message::Password(SecretEdit(value))),
            checkbox(self.editor.register).label("Register with this server").on_toggle(Message::Register),
            text("Leave registration off for an IP-authenticated PBX route.").size(14),
            checkbox(self.editor.automatic_nat).label("Automatic NAT handling").on_toggle(Message::AutomaticNat),
            text_input("STUN server host:port (optional)", &self.editor.stun).on_input(Message::Stun),
            text_input("Outbound proxy sip: URI (optional)", &self.editor.proxy).on_input(Message::Proxy),
            text_input("Fax station ID (up to 20 ASCII characters)", &self.editor.station).on_input(Message::Station),
            text("TLS uses the system trust store and verifies the server certificate. Fax media is not encrypted by SIP TLS.").size(13),
            button(match self.saving_profile { true => "Saving…", false => "Save profile" })
                .on_press_maybe((!self.saving_profile && self.engine.is_some() && self.editor.profile().is_ok()).then_some(Message::SaveProfile)),
        ].spacing(12).max_width(620);
        let settings = column![
            text("Settings").size(26),
            container(
                column![
                    row![
                        column![
                            text("Receive folder"),
                            text(
                                self.snapshot
                                    .receive_settings
                                    .folder
                                    .as_ref()
                                    .map(|path| path.display().to_string())
                                    .unwrap_or_else(|| "Not selected".into())
                            )
                            .size(13)
                        ]
                        .spacing(4)
                        .width(Fill),
                        button("Change folder").on_press(Message::ReceiveFolder)
                    ]
                    .spacing(16)
                    .align_y(Vertical::Center),
                    checkbox(self.snapshot.receive_settings.notifications)
                        .label("Desktop notifications")
                        .on_toggle(Message::ReceiveNotifications),
                ]
                .spacing(16)
            )
            .padding(18)
            .width(Fill)
            .style(surface),
            rule::horizontal(1),
            row![
                profiles.width(FillPortion(1)),
                container(editor).width(FillPortion(3))
            ]
            .spacing(30),
            rule::horizontal(1),
            button("Acknowledgments")
                .style(button::text)
                .on_press(Message::Acknowledgments)
        ]
        .spacing(22);
        scrollable(settings).height(Fill).into()
    }
}

fn job_row(job: &Job, expanded: bool) -> Element<'_, Message> {
    let action = match job.state.is_finished() {
        true => button(match job.state {
            JobState::Succeeded { .. } => "Send again",
            _ => "Retry",
        })
        .style(button::secondary)
        .on_press(Message::Retry(job.id)),
        false => button("Cancel")
            .style(button::secondary)
            .on_press(Message::Cancel(job.id)),
    };
    let mut card = column![
        row![
            text(&job.request.destination).size(22),
            space().width(Fill),
            text(format!(
                "{} · {}",
                job.profile.name,
                job.created_at.format("%b %d, %H:%M UTC")
            ))
            .size(12),
            action
        ]
        .spacing(14)
        .align_y(Vertical::Center),
        row![
            text(status(job)).size(15).width(Fill),
            text(page_status(job)).size(13)
        ]
        .spacing(12),
        progress_bar(0.0..=1.0, progress(job))
            .girth(5)
            .style(move |theme| progress_style(theme, job)),
        row![
            text("Connection  ›  Calling  ›  Negotiation  ›  Sending  ›  Delivery")
                .size(12)
                .width(Fill),
            button(match expanded {
                true => "Hide details",
                false => "Details",
            })
            .style(button::text)
            .on_press(Message::Details(job.id))
        ]
        .align_y(Vertical::Center)
    ]
    .spacing(12);
    if expanded {
        for step in &job.steps {
            card = card.push(
                row![
                    text(step.at.format("%H:%M:%S").to_string()).size(12),
                    text(step.stage.to_string()).size(13)
                ]
                .spacing(14),
            );
        }
        card = card.push(text(format!("Requested mode: {}", job.request.mode)).size(12));
    }
    container(card)
        .padding(20)
        .width(Fill)
        .style(surface)
        .into()
}

fn status(job: &Job) -> String {
    match &job.state {
        JobState::Succeeded { .. } => "Delivered".into(),
        state if state.is_active() => job
            .steps
            .last()
            .map(|s| s.stage.to_string())
            .unwrap_or_else(|| "Setting up connection".into()),
        state => state.to_string(),
    }
}

fn page_status(job: &Job) -> String {
    format!(
        "{} / {} {} confirmed",
        job.state.acknowledged_pages(),
        job.request.document.pages,
        match job.request.document.pages {
            1 => "page",
            _ => "pages",
        }
    )
}

fn progress(job: &Job) -> f32 {
    let pages = job.state.acknowledged_pages() as f32 / job.request.document.pages.max(1) as f32;
    match job.state {
        JobState::Succeeded { .. } => 1.0,
        JobState::Queued => 0.0,
        _ => match job.steps.last().map(|s| s.stage) {
            Some(FaxStage::Connecting | FaxStage::Securing) => 0.08,
            Some(FaxStage::Registering) => 0.16,
            Some(FaxStage::Calling) => 0.25,
            Some(FaxStage::Ringing) => 0.32,
            Some(FaxStage::Connected) => 0.4,
            Some(FaxStage::OfferingT38 | FaxStage::FallingBack | FaxStage::Negotiating) => 0.5,
            Some(FaxStage::SendingT38 | FaxStage::SendingG711) => 0.6 + 0.35 * pages,
            Some(FaxStage::Confirming) => 0.97,
            None => pages,
        },
    }
}

fn progress_style(theme: &iced::Theme, job: &Job) -> progress_bar::Style {
    match job.state {
        JobState::Failed { .. } => progress_bar::danger(theme),
        JobState::Cancelled { .. } | JobState::Interrupted => progress_bar::secondary(theme),
        _ => progress_bar::primary(theme),
    }
}

fn surface(_: &iced::Theme) -> container::Style {
    container::Style {
        background: Some(iced::color!(0x31363F).into()),
        border: iced::border::rounded(12),
        ..Default::default()
    }
}

fn list_item(theme: &iced::Theme, state: button::Status, selected: bool) -> button::Style {
    let mut style = button::text(theme, state);
    style.text_color = iced::color!(0xEEEEEE);
    if selected || matches!(state, button::Status::Hovered) {
        style.background = Some(iced::color!(0x31363F).into());
    }
    if selected {
        style.border = iced::Border {
            color: iced::color!(0x76ABAE),
            width: 1.0,
            radius: 8.into(),
        };
    }
    style
}

fn nav(label: &'static str, tab: Tab, current: Tab) -> Element<'static, Message> {
    button(label)
        .padding([10, 14])
        .style(move |theme, state| list_item(theme, state, tab == current))
        .on_press(Message::Tab(tab))
        .into()
}
