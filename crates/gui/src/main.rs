use egui::{Align, Color32, RichText};
use hakutaku_core::{Availability, Package, ResourceBudget, SegmentInfo};
use hakutaku_pack::{
    AssetChange, Identity, PackOptions, PackProgress, PackReport, ReleasePlan,
    pack_directory_with_progress, plan_directory,
};
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

fn main() {
    let viewport = egui::ViewportBuilder::default()
        .with_inner_size([860.0, 620.0])
        .with_resizable(true)
        .with_min_inner_size([680.0, 480.0])
        .with_icon(application_icon());
    let result = eframe::run_native(
        "Hakutaku",
        eframe::NativeOptions {
            viewport,
            ..Default::default()
        },
        Box::new(|creation| {
            configure_style(&creation.egui_ctx);
            Ok(Box::new(App::new()))
        }),
    );
    if let Err(error) = result {
        eprintln!("Hakutaku GUI failed: {error}");
    }
}

fn application_icon() -> egui::IconData {
    const SIZE: u32 = 256;
    egui::IconData {
        rgba: include_bytes!("../../../assets/icons/hakutaku-256.rgba").to_vec(),
        width: SIZE,
        height: SIZE,
    }
}

fn configure_style(context: &egui::Context) {
    let gray = Color32::from_gray;
    let mut visuals = egui::Visuals::dark();
    visuals.widgets.active.bg_fill = gray(90);
    visuals.widgets.hovered.bg_fill = gray(110);
    visuals.widgets.inactive.bg_fill = gray(60);
    visuals.selection.bg_fill = gray(100);
    visuals.selection.stroke = egui::Stroke::new(1.0_f32, gray(140));
    context.set_visuals(visuals);
    if let Some(font) = load_cjk_font() {
        let mut definitions = egui::FontDefinitions::default();
        definitions
            .font_data
            .insert("system-cjk".into(), egui::FontData::from_owned(font).into());
        for family in definitions.families.values_mut() {
            family.push("system-cjk".into());
        }
        context.set_fonts(definitions);
    }
}

fn load_cjk_font() -> Option<Vec<u8>> {
    [
        "/System/Library/Fonts/STHeiti Medium.ttc",
        "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
        "C:/Windows/Fonts/msyh.ttc",
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
    ]
    .into_iter()
    .find_map(|path| std::fs::read(path).ok().filter(|bytes| !bytes.is_empty()))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tab {
    Resources,
    Release,
    Identity,
}

struct Workspace {
    assets: String,
    release: String,
    identity: String,
    incremental: bool,
    compression_level: i32,
    segment_mib: u64,
    deferred_prefixes: String,
}

impl Default for Workspace {
    fn default() -> Self {
        Self {
            assets: String::new(),
            release: String::new(),
            identity: String::new(),
            incremental: true,
            compression_level: 3,
            segment_mib: 512,
            deferred_prefixes: String::new(),
        }
    }
}

impl Workspace {
    fn ready(&self) -> bool {
        !self.assets.is_empty() && !self.release.is_empty() && !self.identity.is_empty()
    }

    fn pack_options(&self) -> Result<PackOptions, String> {
        let mut options = PackOptions::new(&self.assets, &self.release);
        options.incremental = self.incremental;
        options.compression_level = self.compression_level;
        options.segment_target_bytes = self
            .segment_mib
            .checked_mul(1024 * 1024)
            .ok_or_else(|| "segment size overflow".to_owned())?;
        options.deferred_prefixes = self
            .deferred_prefixes
            .split([',', '\n'])
            .map(str::trim)
            .filter(|prefix| !prefix.is_empty())
            .map(str::to_owned)
            .collect();
        Ok(options)
    }
}

#[derive(Default)]
struct Resources {
    search: String,
    show_unchanged: bool,
    plan: Option<ReleasePlan>,
    selected: Option<usize>,
    pending_replace: Option<Replacement>,
}

#[derive(Clone)]
struct Replacement {
    source: PathBuf,
    target: PathBuf,
    logical_path: String,
}

#[derive(Default)]
struct Release {
    last_report: Option<PackReport>,
    summary: Option<ReleaseSummary>,
}

#[derive(Clone, Debug)]
struct ReleaseSummary {
    sequence: u64,
    assets: usize,
    segments: Vec<SegmentInfo>,
}

#[derive(Clone, Debug)]
struct IdentityInfo {
    path: String,
    project_id: String,
    public_key: String,
}

enum Message {
    Progress(PackProgress),
    Planned(Result<ReleasePlan, String>),
    Packed(Result<(PackReport, ReleasePlan, ReleaseSummary), String>),
    Verified(Result<ReleaseSummary, String>),
    IdentityLoaded(Result<IdentityInfo, String>),
    IdentityCreated(Result<IdentityInfo, String>),
    IdentityBackedUp(Result<String, String>),
    Imported(Result<usize, String>),
    Replaced(Result<String, String>),
}

struct App {
    tab: Tab,
    active_tab: Tab,
    tab_fade: f32,
    workspace: Workspace,
    resources: Resources,
    release: Release,
    identity_info: Option<IdentityInfo>,
    status: String,
    busy: bool,
    progress: Option<PackProgress>,
    started: Option<Instant>,
    show_about: bool,
    sender: Sender<Message>,
    receiver: Receiver<Message>,
}

impl App {
    fn new() -> Self {
        let (sender, receiver) = mpsc::channel();
        Self {
            tab: Tab::Resources,
            active_tab: Tab::Resources,
            tab_fade: 0.0,
            workspace: Workspace::default(),
            resources: Resources {
                show_unchanged: true,
                ..Default::default()
            },
            release: Release::default(),
            identity_info: None,
            status: "Ready".into(),
            busy: false,
            progress: None,
            started: None,
            show_about: false,
            sender,
            receiver,
        }
    }

    fn poll_messages(&mut self) {
        let mut refresh_resources = false;
        while let Ok(message) = self.receiver.try_recv() {
            match message {
                Message::Progress(progress) => {
                    self.status = progress.current_path.as_ref().map_or_else(
                        || progress.phase.into(),
                        |path| format!("{}: {path}", progress.phase),
                    );
                    self.progress = Some(progress);
                }
                Message::Planned(result) => {
                    self.finish_work();
                    match result {
                        Ok(plan) => {
                            self.status = plan_status(&plan);
                            self.resources.plan = Some(plan);
                            self.resources.selected = None;
                        }
                        Err(error) => self.status = format!("Scan failed: {error}"),
                    }
                }
                Message::Packed(result) => {
                    self.finish_work();
                    match result {
                        Ok((report, plan, summary)) => {
                            self.status = pack_status(&report);
                            self.release.last_report = Some(report);
                            self.release.summary = Some(summary);
                            self.resources.plan = Some(plan);
                            self.resources.selected = None;
                        }
                        Err(error) => self.status = format!("Build failed: {error}"),
                    }
                }
                Message::Verified(result) => {
                    self.finish_work();
                    match result {
                        Ok(summary) => {
                            self.status = format!("Release {} is valid", summary.sequence);
                            self.release.summary = Some(summary);
                        }
                        Err(error) => self.status = format!("Verification failed: {error}"),
                    }
                }
                Message::IdentityLoaded(result) => {
                    self.finish_work();
                    match result {
                        Ok(info) => {
                            self.status = "Publisher identity loaded".into();
                            self.identity_info = Some(info);
                        }
                        Err(error) => self.status = format!("Identity load failed: {error}"),
                    }
                }
                Message::IdentityCreated(result) => {
                    self.finish_work();
                    match result {
                        Ok(info) => {
                            self.workspace.identity.clone_from(&info.path);
                            self.status = "Publisher identity created; back it up now".into();
                            self.identity_info = Some(info);
                        }
                        Err(error) => self.status = format!("Identity creation failed: {error}"),
                    }
                }
                Message::IdentityBackedUp(result) => {
                    self.finish_work();
                    self.status = result.map_or_else(
                        |error| format!("Backup failed: {error}"),
                        |path| format!("Identity backed up to {path}"),
                    );
                }
                Message::Imported(result) => {
                    self.finish_work();
                    match result {
                        Ok(count) => {
                            self.status = format!("Imported {count} resource(s)");
                            refresh_resources = true;
                        }
                        Err(error) => self.status = format!("Import failed: {error}"),
                    }
                }
                Message::Replaced(result) => {
                    self.finish_work();
                    match result {
                        Ok(path) => {
                            self.status = format!("Replaced {path}");
                            refresh_resources = true;
                        }
                        Err(error) => self.status = format!("Replace failed: {error}"),
                    }
                }
            }
        }
        if refresh_resources && self.workspace.ready() {
            self.start_plan();
        }
    }

    fn start_plan(&mut self) {
        let options = match self.workspace.pack_options() {
            Ok(options) => options,
            Err(error) => {
                self.status = error;
                return;
            }
        };
        let identity_path = self.workspace.identity.clone();
        let sender = self.sender.clone();
        self.begin_work("Scanning resources…");
        std::thread::spawn(move || {
            let result = Identity::load(identity_path)
                .map_err(|error| error.to_string())
                .and_then(|identity| {
                    plan_directory(&options, &identity).map_err(|error| error.to_string())
                });
            let _ = sender.send(Message::Planned(result));
        });
    }

    fn start_pack(&mut self) {
        let options = match self.workspace.pack_options() {
            Ok(options) => options,
            Err(error) => {
                self.status = error;
                return;
            }
        };
        let identity_path = self.workspace.identity.clone();
        let sender = self.sender.clone();
        self.begin_work("Starting release build…");
        std::thread::spawn(move || {
            let result = (|| {
                let identity = Identity::load(&identity_path).map_err(|error| error.to_string())?;
                let report = pack_directory_with_progress(&options, &identity, |progress| {
                    let _ = sender.send(Message::Progress(progress));
                })
                .map_err(|error| error.to_string())?;
                let plan =
                    plan_directory(&options, &identity).map_err(|error| error.to_string())?;
                let summary = inspect_release(&options.output_directory, &identity, false)?;
                Ok((report, plan, summary))
            })();
            let _ = sender.send(Message::Packed(result));
        });
    }

    fn start_verify(&mut self) {
        let release = PathBuf::from(&self.workspace.release);
        let identity_path = self.workspace.identity.clone();
        let sender = self.sender.clone();
        self.begin_work("Verifying release…");
        std::thread::spawn(move || {
            let result = Identity::load(identity_path)
                .map_err(|error| error.to_string())
                .and_then(|identity| inspect_release(&release, &identity, true));
            let _ = sender.send(Message::Verified(result));
        });
    }

    fn start_identity_load(&mut self) {
        let path = PathBuf::from(&self.workspace.identity);
        let sender = self.sender.clone();
        self.begin_work("Loading publisher identity…");
        std::thread::spawn(move || {
            let result = identity_info(&path);
            let _ = sender.send(Message::IdentityLoaded(result));
        });
    }

    fn start_identity_create(&mut self, path: PathBuf) {
        let sender = self.sender.clone();
        self.begin_work("Creating publisher identity…");
        std::thread::spawn(move || {
            let result = Identity::generate()
                .and_then(|identity| identity.save(&path))
                .map_err(|error| error.to_string())
                .and_then(|()| identity_info(&path));
            let _ = sender.send(Message::IdentityCreated(result));
        });
    }

    fn start_identity_backup(&mut self, target: PathBuf) {
        let source = PathBuf::from(&self.workspace.identity);
        let sender = self.sender.clone();
        self.begin_work("Backing up publisher identity…");
        std::thread::spawn(move || {
            let display = target.display().to_string();
            let result = copy_new_file(&source, &target, true).map(|()| display);
            let _ = sender.send(Message::IdentityBackedUp(result));
        });
    }

    fn start_import(&mut self, sources: Vec<PathBuf>) {
        let directory = PathBuf::from(&self.workspace.assets);
        let sender = self.sender.clone();
        self.begin_work("Importing resources…");
        std::thread::spawn(move || {
            let result = import_resources(&sources, &directory);
            let _ = sender.send(Message::Imported(result));
        });
    }

    fn start_replace(&mut self, replacement: Replacement) {
        let sender = self.sender.clone();
        self.begin_work("Replacing resource…");
        std::thread::spawn(move || {
            let result = replace_file(&replacement.source, &replacement.target)
                .map(|()| replacement.logical_path);
            let _ = sender.send(Message::Replaced(result));
        });
    }

    fn begin_work(&mut self, status: &str) {
        self.busy = true;
        self.status = status.into();
        self.started = Some(Instant::now());
    }

    fn finish_work(&mut self) {
        self.busy = false;
        self.progress = None;
    }
}

impl eframe::App for App {
    fn update(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_messages();
        if self.busy {
            context.request_repaint_after(Duration::from_millis(100));
        }

        egui::TopBottomPanel::top("tabs")
            .frame(
                egui::Frame::side_top_panel(&context.style()).inner_margin(egui::Margin {
                    left: 8,
                    right: 8,
                    top: 6,
                    bottom: 4,
                }),
            )
            .min_height(28.0)
            .show(context, |ui| {
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.tab, Tab::Resources, " Resources ");
                    ui.selectable_value(&mut self.tab, Tab::Release, " Release ");
                    ui.selectable_value(&mut self.tab, Tab::Identity, " Identity ");
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        if ui.button("About").clicked() {
                            self.show_about = true;
                        }
                    });
                });
            });

        if self.active_tab != self.tab {
            self.active_tab = self.tab;
            self.tab_fade = 0.0;
        }
        if self.tab_fade < 1.0 {
            let delta = context.input(|input| input.stable_dt).min(0.05);
            self.tab_fade = (self.tab_fade + delta * 8.0).min(1.0);
            context.request_repaint();
        }

        egui::CentralPanel::default().show(context, |ui| {
            ui.multiply_opacity(self.tab_fade);
            ui.add_enabled_ui(!self.busy, |ui| match self.tab {
                Tab::Resources => self.resources_page(ui),
                Tab::Release => self.release_page(ui),
                Tab::Identity => self.identity_page(ui),
            });
        });
        self.status_bar(context);
        self.replace_confirmation(context);
        self.about_window(context);
    }
}

impl App {
    fn resources_page(&mut self, ui: &mut egui::Ui) {
        ui.spacing_mut().item_spacing.y = 8.0;
        ui.heading("Game Resources");
        self.workspace_paths(ui, true);
        ui.horizontal(|ui| {
            if ui
                .add_enabled(self.workspace.ready(), egui::Button::new("Scan"))
                .clicked()
            {
                self.start_plan();
            }
            if ui
                .add_enabled(
                    !self.workspace.assets.is_empty(),
                    egui::Button::new("Import…"),
                )
                .clicked()
                && let Some(files) = rfd::FileDialog::new().pick_files()
            {
                self.start_import(files);
            }
            let selected = self.selected_source();
            if ui
                .add_enabled(selected.is_some(), egui::Button::new("Replace…"))
                .clicked()
                && let Some((logical_path, target)) = selected.clone()
                && let Some(source) = rfd::FileDialog::new().pick_file()
            {
                self.resources.pending_replace = Some(Replacement {
                    source,
                    target,
                    logical_path,
                });
            }
            if ui
                .add_enabled(selected.is_some(), egui::Button::new("Reveal"))
                .clicked()
                && let Some((_, path)) = selected
                && let Err(error) = reveal_file(&path)
            {
                self.status = format!("Reveal failed: {error}");
            }
            ui.separator();
            ui.add_sized(
                [220.0, 22.0],
                egui::TextEdit::singleline(&mut self.resources.search)
                    .hint_text("Filter resources…"),
            );
            ui.checkbox(&mut self.resources.show_unchanged, "Show unchanged");
        });
        ui.separator();
        let has_plan = self.resources.plan.is_some();
        fade_visible(ui, "resource-plan", has_plan, |ui| {
            let Some(plan) = &self.resources.plan else {
                return;
            };
            resource_summary(ui, plan);
            ui.add_space(4.0);
            let query = self.resources.search.to_ascii_lowercase();
            egui::ScrollArea::vertical().show(ui, |ui| {
                egui::Grid::new("resource-list")
                    .striped(true)
                    .num_columns(5)
                    .show(ui, |ui| {
                        ui.strong("Path");
                        ui.strong("Size");
                        ui.strong("Access");
                        ui.strong("Install");
                        ui.strong("State");
                        ui.end_row();
                        for (index, asset) in plan.assets.iter().enumerate() {
                            if (!self.resources.show_unchanged
                                && asset.change == AssetChange::Unchanged)
                                || (!query.is_empty()
                                    && !asset.path.to_ascii_lowercase().contains(&query))
                            {
                                continue;
                            }
                            let selected = self.resources.selected == Some(index);
                            if ui.selectable_label(selected, &asset.path).clicked() {
                                self.resources.selected = Some(index);
                            }
                            ui.label(format_size(
                                asset.source_len.or(asset.released_len).unwrap_or(0),
                            ));
                            ui.label(format!("{:?}", asset.access));
                            ui.label(format!("{:?}", asset.availability));
                            ui.label(change_label(asset.change));
                            ui.end_row();
                        }
                    });
            });
        });
        if !has_plan {
            ui.label("Open an asset directory and scan it to review the next release.");
        }
    }

    fn release_page(&mut self, ui: &mut egui::Ui) {
        ui.spacing_mut().item_spacing.y = 8.0;
        ui.heading("Release Management");
        self.workspace_paths(ui, true);
        ui.horizontal(|ui| {
            ui.label("Options:");
            ui.checkbox(&mut self.workspace.incremental, "Reuse current release");
            ui.separator();
            ui.label("zstd");
            ui.add(egui::DragValue::new(&mut self.workspace.compression_level).range(-7..=22));
            ui.separator();
            ui.label("Segment MiB");
            ui.add(egui::DragValue::new(&mut self.workspace.segment_mib).range(1..=4096));
        });
        ui.horizontal(|ui| {
            let ready = self.workspace.ready();
            if ui
                .add_enabled(ready, egui::Button::new("Preview"))
                .clicked()
            {
                self.start_plan();
            }
            if ui
                .add_enabled(ready, egui::Button::new("Build Release"))
                .clicked()
            {
                self.start_pack();
            }
            let can_verify =
                !self.workspace.release.is_empty() && !self.workspace.identity.is_empty();
            if ui
                .add_enabled(can_verify, egui::Button::new("Verify Release"))
                .clicked()
            {
                self.start_verify();
            }
        });
        ui.separator();
        fade_visible(ui, "release-preview", self.resources.plan.is_some(), |ui| {
            let Some(plan) = &self.resources.plan else {
                return;
            };
            ui.heading("Build Preview");
            resource_summary(ui, plan);
        });
        fade_visible(
            ui,
            "release-report",
            self.release.last_report.is_some(),
            |ui| {
                let Some(report) = &self.release.last_report else {
                    return;
                };
                ui.add_space(8.0);
                ui.heading("Last Build");
                egui::Grid::new("build-report")
                    .striped(true)
                    .show(ui, |ui| {
                        metric(ui, "Release", report.release_sequence.to_string());
                        metric(ui, "Files", report.file_count.to_string());
                        metric(ui, "Reused blocks", report.reused_blocks.to_string());
                        metric(ui, "New blocks", report.new_blocks.to_string());
                        metric(ui, "New segments", report.new_segments.to_string());
                        metric(ui, "Written", format_size(report.new_segment_bytes));
                    });
            },
        );
        fade_visible(
            ui,
            "release-summary",
            self.release.summary.is_some(),
            |ui| {
                let Some(summary) = &self.release.summary else {
                    return;
                };
                ui.add_space(8.0);
                ui.heading("Active Release");
                release_summary(ui, summary);
            },
        );
    }

    fn identity_page(&mut self, ui: &mut egui::Ui) {
        ui.spacing_mut().item_spacing.y = 8.0;
        ui.heading("Publisher Identity");
        path_row(
            ui,
            "Identity:",
            &mut self.workspace.identity,
            PathPicker::Identity,
            22.0,
        );
        ui.small(
            RichText::new("Contains the signing key and content root key. Never ship it.")
                .color(Color32::from_rgb(215, 178, 120)),
        );
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    !self.workspace.identity.is_empty(),
                    egui::Button::new("Inspect"),
                )
                .clicked()
            {
                self.start_identity_load();
            }
            if ui.button("Create New…").clicked()
                && let Some(path) = rfd::FileDialog::new()
                    .set_file_name("publisher.hakutaku-key")
                    .save_file()
            {
                self.start_identity_create(path);
            }
            if ui
                .add_enabled(
                    !self.workspace.identity.is_empty(),
                    egui::Button::new("Backup…"),
                )
                .clicked()
                && let Some(path) = rfd::FileDialog::new()
                    .set_file_name("publisher.hakutaku-key")
                    .save_file()
            {
                self.start_identity_backup(path);
            }
        });
        ui.separator();
        fade_visible(ui, "identity-info", self.identity_info.is_some(), |ui| {
            let Some(info) = &self.identity_info else {
                return;
            };
            egui::Grid::new("identity-info-grid")
                .striped(true)
                .show(ui, |ui| {
                    metric(ui, "File", info.path.clone());
                    metric(ui, "Project ID", info.project_id.clone());
                    metric(ui, "Public key", info.public_key.clone());
                });
            ui.add_space(8.0);
            ui.label("Keep at least one offline backup. Losing this file breaks future updates.");
        });
    }

    fn workspace_paths(&mut self, ui: &mut egui::Ui, deferred: bool) {
        path_row(
            ui,
            "Assets:",
            &mut self.workspace.assets,
            PathPicker::Directory,
            22.0,
        );
        path_row(
            ui,
            "Release:",
            &mut self.workspace.release,
            PathPicker::Directory,
            22.0,
        );
        path_row(
            ui,
            "Identity:",
            &mut self.workspace.identity,
            PathPicker::Identity,
            22.0,
        );
        if deferred {
            ui.horizontal(|ui| {
                ui.label("Deferred:");
                ui.add_sized(
                    [ui.available_width(), 22.0],
                    egui::TextEdit::singleline(&mut self.workspace.deferred_prefixes)
                        .hint_text("optional directory prefixes, comma-separated"),
                );
            });
        }
    }

    fn selected_source(&self) -> Option<(String, PathBuf)> {
        let asset = self
            .resources
            .selected
            .and_then(|index| self.resources.plan.as_ref()?.assets.get(index))?;
        asset.source_len?;
        Some((
            asset.path.clone(),
            Path::new(&self.workspace.assets).join(&asset.path),
        ))
    }

    fn status_bar(&self, context: &egui::Context) {
        egui::TopBottomPanel::bottom("status")
            .frame(
                egui::Frame::side_top_panel(&context.style())
                    .inner_margin(egui::Margin::symmetric(8, 6)),
            )
            .min_height(32.0)
            .show(context, |ui| {
                ui.horizontal(|ui| {
                    if self.busy
                        && let Some(progress) = &self.progress
                        && progress.total_bytes > 0
                    {
                        let fraction =
                            progress.completed_bytes as f32 / progress.total_bytes as f32;
                        ui.add(
                            egui::ProgressBar::new(fraction.clamp(0.0, 1.0))
                                .desired_width(ui.available_width() - 60.0)
                                .text(format!("{:.0}%", fraction * 100.0)),
                        );
                    } else {
                        ui.label(&self.status);
                    }
                    if self.busy
                        && let Some(started) = self.started
                    {
                        ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                            ui.label(format!("{:.1}s", started.elapsed().as_secs_f64()));
                        });
                    }
                });
            });
    }

    fn replace_confirmation(&mut self, context: &egui::Context) {
        let Some(replacement) = self.resources.pending_replace.clone() else {
            return;
        };
        egui::Window::new("Replace Resource")
            .collapsible(false)
            .resizable(false)
            .show(context, |ui| {
                ui.label(format!("Replace {}?", replacement.logical_path));
                ui.label("The existing source file will be replaced atomically.");
                ui.horizontal(|ui| {
                    if ui.button("Cancel").clicked() {
                        self.resources.pending_replace = None;
                    }
                    if ui.button("Replace").clicked() {
                        self.resources.pending_replace = None;
                        self.start_replace(replacement);
                    }
                });
            });
    }

    fn about_window(&mut self, context: &egui::Context) {
        let mut show = self.show_about;
        let mut close_clicked = false;
        egui::Window::new("About Hakutaku")
            .title_bar(false)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .default_width(260.0)
            .default_height(0.0)
            .open(&mut show)
            .show(context, |ui| {
                ui.label("Hakutaku");
                ui.label(format!("Version {}", env!("CARGO_PKG_VERSION")));
                ui.hyperlink("https://github.com/maincoretech/hakutaku");
                ui.label("Authenticated resource and release manager for offline games");
                ui.add_space(10.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::BOTTOM), |ui| {
                    if ui.button("Close").clicked() {
                        close_clicked = true;
                    }
                });
            });
        if close_clicked {
            show = false;
        }
        self.show_about = show;
    }
}

#[derive(Clone, Copy)]
enum PathPicker {
    Directory,
    Identity,
}

fn path_row(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut String,
    picker: PathPicker,
    row_height: f32,
) {
    ui.horizontal(|ui| {
        ui.label(label);
        let width = (ui.available_width() - 34.0).max(60.0);
        ui.add_sized([width, row_height], egui::TextEdit::singleline(value));
        if ui
            .add(egui::Button::new("…").min_size(egui::vec2(0.0, row_height)))
            .clicked()
        {
            let path = match picker {
                PathPicker::Directory => rfd::FileDialog::new().pick_folder(),
                PathPicker::Identity => rfd::FileDialog::new()
                    .add_filter("Hakutaku Identity", &["hakutaku-key"])
                    .pick_file(),
            };
            if let Some(path) = path {
                *value = path.display().to_string();
            }
        }
    });
}

fn fade_visible(
    ui: &mut egui::Ui,
    id: &'static str,
    visible: bool,
    contents: impl FnOnce(&mut egui::Ui),
) {
    let opacity = ui.ctx().animate_bool(egui::Id::new(("fade", id)), visible);
    if visible || opacity > 0.0 {
        if opacity < 1.0 {
            ui.ctx().request_repaint();
        }
        ui.scope(|ui| {
            ui.multiply_opacity(opacity);
            contents(ui);
        });
    }
}

fn resource_summary(ui: &mut egui::Ui, plan: &ReleasePlan) {
    let count = |state| {
        plan.assets
            .iter()
            .filter(|asset| asset.change == state)
            .count()
    };
    ui.horizontal_wrapped(|ui| {
        ui.label(format!(
            "{} source files",
            plan.assets.len() - count(AssetChange::Removed)
        ));
        ui.separator();
        ui.label(format_size(plan.source_bytes));
        ui.separator();
        ui.label(format!(
            "{} added · {} modified · {} removed · {} unchanged",
            count(AssetChange::Added),
            count(AssetChange::Modified),
            count(AssetChange::Removed),
            count(AssetChange::Unchanged)
        ));
        ui.separator();
        ui.label(format!(
            "{} changed source",
            format_size(plan.changed_source_bytes)
        ));
        if let Some(sequence) = plan.previous_release {
            ui.separator();
            ui.label(format!("compared with release {sequence}"));
        }
    });
}

fn release_summary(ui: &mut egui::Ui, summary: &ReleaseSummary) {
    let required = summary
        .segments
        .iter()
        .filter(|segment| segment.availability == Availability::Required);
    let required_count = required.clone().count();
    let required_bytes = required.map(|segment| segment.len).sum();
    let deferred = summary
        .segments
        .iter()
        .filter(|segment| segment.availability == Availability::Deferred);
    let deferred_count = deferred.clone().count();
    let deferred_bytes = deferred.map(|segment| segment.len).sum();
    egui::Grid::new("release-summary")
        .striped(true)
        .show(ui, |ui| {
            metric(ui, "Release", summary.sequence.to_string());
            metric(ui, "Assets", summary.assets.to_string());
            metric(
                ui,
                "Required",
                format!(
                    "{required_count} segment(s), {}",
                    format_size(required_bytes)
                ),
            );
            metric(
                ui,
                "Deferred",
                format!(
                    "{deferred_count} segment(s), {}",
                    format_size(deferred_bytes)
                ),
            );
        });
}

fn metric(ui: &mut egui::Ui, label: &str, value: String) {
    ui.label(label);
    ui.strong(value);
    ui.end_row();
}

fn change_label(change: AssetChange) -> &'static str {
    match change {
        AssetChange::Added => "Added",
        AssetChange::Modified => "Modified",
        AssetChange::Unchanged => "Unchanged",
        AssetChange::Removed => "Removed",
    }
}

fn plan_status(plan: &ReleasePlan) -> String {
    let source_files = plan
        .assets
        .iter()
        .filter(|asset| asset.change != AssetChange::Removed)
        .count();
    let changed = plan
        .assets
        .iter()
        .filter(|asset| asset.change != AssetChange::Unchanged)
        .count();
    format!("Scanned {source_files} source files; {changed} change(s)")
}

fn pack_status(report: &PackReport) -> String {
    if report.changed {
        format!(
            "Release {} built: {} reused, {} new blocks, {} new segment(s)",
            report.release_sequence, report.reused_blocks, report.new_blocks, report.new_segments
        )
    } else {
        format!(
            "No changes; release {} remains active",
            report.release_sequence
        )
    }
}

fn inspect_release(
    release: &Path,
    identity: &Identity,
    verify: bool,
) -> Result<ReleaseSummary, String> {
    let package = Package::open_directory(
        release.join("game.haku"),
        release.join("data"),
        identity.root_key(),
        identity.public_key(),
        ResourceBudget::memory_constrained(),
    )
    .map_err(|error| error.to_string())?;
    if verify {
        package
            .verify_segments()
            .map_err(|error| error.to_string())?;
    }
    Ok(ReleaseSummary {
        sequence: package.release_sequence(),
        assets: package
            .list_assets()
            .map_err(|error| error.to_string())?
            .len(),
        segments: package.list_segments().map_err(|error| error.to_string())?,
    })
}

fn identity_info(path: &Path) -> Result<IdentityInfo, String> {
    let identity = Identity::load(path).map_err(|error| error.to_string())?;
    Ok(IdentityInfo {
        path: path.display().to_string(),
        project_id: encode_hex(&identity.project_id().0),
        public_key: encode_hex(&identity.public_key()),
    })
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

fn import_resources(sources: &[PathBuf], directory: &Path) -> Result<usize, String> {
    if !directory.is_dir() {
        return Err(format!(
            "asset directory does not exist: {}",
            directory.display()
        ));
    }
    let mut targets = Vec::with_capacity(sources.len());
    let mut names = HashSet::with_capacity(sources.len());
    for source in sources {
        if !source.is_file() {
            return Err(format!("not a regular file: {}", source.display()));
        }
        let name = source
            .file_name()
            .ok_or_else(|| format!("file has no name: {}", source.display()))?;
        if !names.insert(name.to_owned()) {
            return Err(format!("duplicate file name: {}", name.to_string_lossy()));
        }
        let target = directory.join(name);
        if target.exists() {
            return Err(format!("resource already exists: {}", target.display()));
        }
        targets.push((source, target));
    }
    let mut imported = Vec::with_capacity(targets.len());
    for (source, target) in targets {
        if let Err(error) = copy_new_file(source, &target, false) {
            for path in imported {
                let _ = std::fs::remove_file(path);
            }
            return Err(error);
        }
        imported.push(target);
    }
    Ok(sources.len())
}

fn copy_new_file(source: &Path, target: &Path, private: bool) -> Result<(), String> {
    let mut source = File::open(source).map_err(|error| error.to_string())?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(if private { 0o600 } else { 0o666 });
    }
    let mut target_file = options.open(target).map_err(|error| error.to_string())?;
    let result = (|| {
        std::io::copy(&mut source, &mut target_file).map_err(|error| error.to_string())?;
        target_file.flush().map_err(|error| error.to_string())?;
        target_file.sync_all().map_err(|error| error.to_string())
    })();
    drop(target_file);
    if result.is_err() {
        let _ = std::fs::remove_file(target);
    }
    result
}

fn replace_file(source: &Path, target: &Path) -> Result<(), String> {
    let parent = target
        .parent()
        .ok_or_else(|| format!("resource has no parent: {}", target.display()))?;
    let name = target
        .file_name()
        .ok_or_else(|| format!("resource has no file name: {}", target.display()))?
        .to_string_lossy();
    let temporary = parent.join(format!(".{name}.hakutaku-part-{}", std::process::id()));
    let backup = parent.join(format!(".{name}.hakutaku-backup-{}", std::process::id()));
    let permissions = target
        .metadata()
        .map_err(|error| error.to_string())?
        .permissions();
    copy_new_file(source, &temporary, false)?;
    if let Err(error) = std::fs::set_permissions(&temporary, permissions) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error.to_string());
    }
    if let Err(error) = std::fs::rename(target, &backup) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error.to_string());
    }
    if let Err(error) = std::fs::rename(&temporary, target) {
        let restore = std::fs::rename(&backup, target);
        let _ = std::fs::remove_file(&temporary);
        return match restore {
            Ok(()) => Err(error.to_string()),
            Err(restore_error) => Err(format!(
                "{error}; restoring {} also failed: {restore_error}",
                target.display()
            )),
        };
    }
    std::fs::remove_file(backup).map_err(|error| error.to_string())?;
    Ok(())
}

fn reveal_file(path: &Path) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = Command::new("open");
        command.arg("-R").arg(path);
        command
    };
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = Command::new("explorer");
        command.arg(format!("/select,{}", path.display()));
        command
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = {
        let mut command = Command::new("xdg-open");
        command.arg(path.parent().unwrap_or(path));
        command
    };
    command
        .spawn()
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn format_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_application_icon_is_valid_rgba() {
        let icon = application_icon();
        assert_eq!(icon.width, 256);
        assert_eq!(icon.height, 256);
        assert_eq!(icon.rgba.len(), 256 * 256 * 4);
        assert_eq!(icon.rgba[3], 0, "canvas corner must be transparent");
        let center_alpha = ((128 * 256 + 128) * 4 + 3) as usize;
        assert_eq!(icon.rgba[center_alpha], 255, "icon center must be opaque");
    }

    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "hakutaku-gui-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ))
    }

    #[test]
    fn resource_import_never_overwrites_and_replace_is_atomic() {
        let root = scratch("resources");
        let source = root.join("source");
        let assets = root.join("assets");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&assets).unwrap();
        let first = source.join("first.bin");
        let replacement = source.join("replacement.bin");
        std::fs::write(&first, b"first").unwrap();
        std::fs::write(&replacement, b"replacement").unwrap();

        assert_eq!(
            import_resources(std::slice::from_ref(&first), &assets).unwrap(),
            1
        );
        assert!(import_resources(std::slice::from_ref(&first), &assets).is_err());
        let target = assets.join("first.bin");
        assert_eq!(std::fs::read(&target).unwrap(), b"first");
        replace_file(&replacement, &target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"replacement");
        assert_eq!(std::fs::read_dir(&assets).unwrap().count(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn private_identity_backup_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let root = scratch("private");
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("source.key");
        let backup = root.join("backup.key");
        std::fs::write(&source, b"secret").unwrap();
        copy_new_file(&source, &backup, true).unwrap();
        assert_eq!(
            backup.metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
