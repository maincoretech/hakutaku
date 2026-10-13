mod icons;

use eframe::egui::{self, Align, Color32, RichText};
use hakutaku_core::{Availability, OpenPolicy, Package, ResourceBudget, SegmentInfo};
use hakutaku_pack::{
    AssetChange, Identity, PackOptions, PackProgress, PackReport, ReleasePlan, RuntimeKeyMaterial,
    is_hakutaku_key_file, pack_directory_with_progress, plan_directory, plan_directory_after_pack,
};
use icons::Icon;
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
        .with_min_inner_size([680.0, 480.0]);
    #[cfg(target_os = "macos")]
    let viewport = viewport.with_icon(egui::IconData::default());
    #[cfg(not(target_os = "macos"))]
    let viewport = viewport.with_icon(application_icon());
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

#[cfg(any(not(target_os = "macos"), test))]
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

struct Workspace {
    assets: String,
    release: String,
    identity: String,
    incremental: bool,
    development_cache: bool,
    release_sequence_floor: u64,
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
            development_cache: false,
            release_sequence_floor: 1,
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
        options.release_sequence_floor = self.release_sequence_floor;
        // Full builds always reread sources, even if a previous incremental build
        // opted into the development cache.
        options.development_cache = self.incremental && self.development_cache;
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
    change_counts: [u64; 4],
    plan: Option<ReleasePlan>,
    selected: Option<usize>,
    pending_replace: Option<Replacement>,
    visible: Vec<usize>,
    filter: Option<(String, bool)>,
}

impl Resources {
    fn clear_plan(&mut self) {
        self.plan = None;
        self.change_counts = [0; 4];
        self.selected = None;
        self.pending_replace = None;
        self.visible.clear();
        self.filter = None;
    }

    fn set_plan(&mut self, plan: ReleasePlan) {
        self.change_counts = [0; 4];
        for asset in &plan.assets {
            let index = match asset.change {
                AssetChange::Added => 0,
                AssetChange::Modified => 1,
                AssetChange::Removed => 2,
                AssetChange::Unchanged => 3,
            };
            self.change_counts[index] += 1;
        }
        self.plan = Some(plan);
        self.selected = None;
        self.filter = None;
    }

    fn update_filter(&mut self) {
        let filter = (self.search.to_ascii_lowercase(), self.show_unchanged);
        if self.filter.as_ref() == Some(&filter) {
            return;
        }
        self.visible.clear();
        if let Some(plan) = &self.plan {
            self.visible
                .extend(plan.assets.iter().enumerate().filter_map(|(index, asset)| {
                    ((filter.1 || asset.change != AssetChange::Unchanged)
                        && (filter.0.is_empty()
                            || asset.path.to_ascii_lowercase().contains(&filter.0)))
                    .then_some(index)
                }));
        }
        self.filter = Some(filter);
    }
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
                            self.resources.set_plan(plan);
                        }
                        Err(error) => self.status = format!("Change check failed: {error}"),
                    }
                }
                Message::Packed(result) => {
                    self.finish_work();
                    match result {
                        Ok((report, plan, summary)) => {
                            self.status = pack_status(&report);
                            self.release.last_report = Some(report);
                            self.release.summary = Some(summary);
                            self.resources.set_plan(plan);
                        }
                        Err(error) => self.status = format!("Build failed: {error}"),
                    }
                }
                Message::Verified(result) => {
                    self.finish_work();
                    match result {
                        Ok(summary) => {
                            self.status = format!("#{} verified", summary.sequence);
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
                            self.clear_workspace_results();
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
        self.resources.clear_plan();
        self.begin_work("Checking source changes…");
        self.spawn_worker(move || {
            let result = Identity::load(identity_path)
                .map_err(|error| error.to_string())
                .and_then(|identity| {
                    let keys = identity
                        .runtime_key_material()
                        .map_err(|error| error.to_string())?;
                    plan_directory(&options, &keys).map_err(|error| error.to_string())
                });
            let _ = sender.send(Message::Planned(result));
        });
    }

    fn clear_workspace_results(&mut self) {
        self.resources.clear_plan();
        self.release = Release::default();
        self.identity_info = None;
        self.status = "Ready to scan".into();
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
        self.spawn_worker(move || {
            let result = (|| {
                let identity = Identity::load(&identity_path).map_err(|error| error.to_string())?;
                let mut last_update = Instant::now();
                let mut last_phase = "";
                let report = pack_directory_with_progress(&options, &identity, |progress| {
                    if progress.phase != last_phase
                        || last_update.elapsed() >= Duration::from_millis(100)
                    {
                        last_phase = progress.phase;
                        last_update = Instant::now();
                        let _ = sender.send(Message::Progress(progress));
                    }
                })
                .map_err(|error| error.to_string())?;
                let keys = identity
                    .runtime_key_material()
                    .map_err(|error| error.to_string())?;
                let plan = plan_directory_after_pack(&options, &keys)
                    .map_err(|error| error.to_string())?;
                let summary = inspect_release(&options.output_directory, &keys, false)?;
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
        self.spawn_worker(move || {
            let result = Identity::load(identity_path)
                .map_err(|error| error.to_string())
                .and_then(|identity| {
                    let keys = identity
                        .runtime_key_material()
                        .map_err(|error| error.to_string())?;
                    inspect_release(&release, &keys, true)
                });
            let _ = sender.send(Message::Verified(result));
        });
    }

    fn start_identity_load(&mut self) {
        let path = PathBuf::from(&self.workspace.identity);
        let sender = self.sender.clone();
        self.begin_work("Loading publisher identity…");
        self.spawn_worker(move || {
            let result = identity_info(&path);
            let _ = sender.send(Message::IdentityLoaded(result));
        });
    }

    fn start_identity_create(&mut self, path: PathBuf) {
        let sender = self.sender.clone();
        self.begin_work("Creating publisher identity…");
        self.spawn_worker(move || {
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
        self.spawn_worker(move || {
            let display = target.display().to_string();
            let result = copy_new_file(&source, &target, true).map(|()| display);
            let _ = sender.send(Message::IdentityBackedUp(result));
        });
    }

    fn start_import(&mut self, sources: Vec<PathBuf>) {
        let directory = PathBuf::from(&self.workspace.assets);
        let sender = self.sender.clone();
        self.begin_work("Importing resources…");
        self.spawn_worker(move || {
            let result = import_resources(&sources, &directory);
            let _ = sender.send(Message::Imported(result));
        });
    }

    fn start_replace(&mut self, replacement: Replacement) {
        let sender = self.sender.clone();
        self.begin_work("Replacing resource…");
        self.spawn_worker(move || {
            let result = replace_file(&replacement.source, &replacement.target)
                .map(|()| replacement.logical_path);
            let _ = sender.send(Message::Replaced(result));
        });
    }

    fn spawn_worker(&mut self, work: impl FnOnce() + Send + 'static) {
        if let Err(error) = std::thread::Builder::new()
            .name("hakutaku-publisher".into())
            .spawn(work)
        {
            self.finish_work();
            self.status = format!("Could not start background work: {error}");
        }
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
    fn logic(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_messages();
        if self.busy {
            context.request_repaint_after(Duration::from_millis(100));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let context = ui.ctx().clone();
        egui::Panel::top("header")
            .frame(
                egui::Frame::side_top_panel(ui.style())
                    .inner_margin(egui::Margin::symmetric(12, 8)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.heading("Hakutaku");
                    ui.weak(env!("CARGO_PKG_VERSION"));
                    ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                        if ui.button("About").clicked() {
                            self.show_about = true;
                        }
                    });
                });
                ui.add_space(8.0);
                ui.add_enabled_ui(!self.busy, |ui| self.release_actions(ui));
            });
        self.status_bar(ui);
        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical()
                .id_salt("workspace")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 8.0;
                    ui.add_enabled_ui(!self.busy, |ui| {
                        self.workspace_paths(ui);
                        egui::CollapsingHeader::new("Key tools")
                            .show(ui, |ui| self.identity_tools(ui));
                        ui.add_space(4.0);
                        ui.horizontal(|ui| {
                            if ui.checkbox(&mut self.workspace.incremental, "Reuse blocks")
                                .on_hover_text("Reuse blocks from the last build. Off: rebuild everything.")
                                .changed()
                            {
                                self.resources.clear_plan();
                            }
                            ui.separator();
                            let cached = self.workspace.incremental && self.workspace.development_cache;
                            let (icon, label, color, hint) = if cached {
                                (Icon::Clock, "Dev cache", Color32::from_rgb(218, 178, 110),
                                 "Local builds only. Uses cached file metadata; skips full source checks.")
                            } else {
                                (Icon::Shield, "Strict", Color32::from_rgb(144, 192, 161),
                                 "Reads every source file and compares staged bytes before publishing.")
                            };
                            ui.horizontal(|ui| {
                                icons::show(ui, icon, 18.0, color);
                                ui.colored_label(color, label);
                            }).response.on_hover_text(hint);
                        });
                        egui::CollapsingHeader::new("Options")
                            .show(ui, |ui| self.advanced_options(ui));
                        self.release_details(ui);
                        ui.separator();
                        self.resources_page(ui);
                    });
                });
        });
        self.replace_confirmation(&context);
        self.about_window(&context);
    }
}

impl App {
    fn resources_page(&mut self, ui: &mut egui::Ui) {
        ui.strong("Files");
        if self.resources.plan.is_some() {
            resource_summary(ui, self.resources.change_counts);
        }
        ui.horizontal_wrapped(|ui| {
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
                egui::TextEdit::singleline(&mut self.resources.search).hint_text("Find a file…"),
            );
            ui.checkbox(&mut self.resources.show_unchanged, "Unchanged");
        });
        ui.separator();
        self.resources.update_filter();
        let has_plan = self.resources.plan.is_some();
        fade_visible(ui, "resource-plan", has_plan, |ui| {
            let Some(plan) = &self.resources.plan else {
                return;
            };
            let row_height = ui
                .text_style_height(&egui::TextStyle::Body)
                .max(ui.spacing().interact_size.y);
            egui::ScrollArea::vertical()
                .id_salt("resource-rows")
                .max_height(320.0)
                .show_rows(
                    ui,
                    row_height,
                    self.resources.visible.len() + 1,
                    |ui, rows| {
                        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
                        let path_width = (ui.available_width()
                            - 2.0 * (90.0 + ui.spacing().item_spacing.x))
                            .max(140.0);
                        egui::Grid::new("resource-list")
                            .striped(true)
                            .num_columns(3)
                            .min_col_width(90.0)
                            .min_row_height(row_height)
                            .start_row(rows.start)
                            .show(ui, |ui| {
                                for row in rows {
                                    if row == 0 {
                                        for label in ["File", "Size", "Change"] {
                                            ui.strong(label);
                                        }
                                    } else {
                                        let index = self.resources.visible[row - 1];
                                        let asset = &plan.assets[index];
                                        let selected = self.resources.selected == Some(index);
                                        if ui
                                            .add_sized(
                                                [path_width, row_height],
                                                egui::Button::selectable(
                                                    selected,
                                                    (&asset.path, egui::Atom::grow()),
                                                ),
                                            )
                                            .clicked()
                                        {
                                            self.resources.selected = Some(index);
                                        }
                                        ui.label(format_size(
                                            asset.source_len.or(asset.released_len).unwrap_or(0),
                                        ));
                                        ui.colored_label(
                                            change_color(asset.change),
                                            change_label(asset.change),
                                        )
                                        .on_hover_text(
                                            format!(
                                                "Access: {:?}\nInstall: {:?}",
                                                asset.access, asset.availability
                                            ),
                                        );
                                    }
                                    ui.end_row();
                                }
                            });
                    },
                );
        });
        if !has_plan {
            ui.horizontal(|ui| {
                icons::show(ui, Icon::Scan, 24.0, ui.visuals().weak_text_color());
                ui.weak("Scan to view files");
            });
        }
    }

    fn advanced_options(&mut self, ui: &mut egui::Ui) {
        ui.add_enabled_ui(self.workspace.incremental, |ui| {
            ui.checkbox(
                &mut self.workspace.development_cache,
                "Dev cache",
            ).on_hover_text("Local builds only. Skips source reads and staged byte comparison for cached files.");
        });
        if !self.workspace.incremental {
            ui.small("Full build · cache off");
        }
        let mut changed = false;
        ui.horizontal_wrapped(|ui| {
            ui.label("Compression").on_hover_text("Zstandard level");
            changed |= ui
                .add(egui::DragValue::new(&mut self.workspace.compression_level).range(-7..=22))
                .changed();
            ui.separator();
            ui.label("Segment MiB");
            changed |= ui
                .add(egui::DragValue::new(&mut self.workspace.segment_mib).range(1..=4096))
                .changed();
        });
        ui.horizontal(|ui| {
            ui.label("Start at #").on_hover_text("Lowest release number. For v1 migration, use an empty output folder and a number above the old release.");
            changed |= ui
                .add(
                    egui::DragValue::new(&mut self.workspace.release_sequence_floor)
                        .range(1..=u64::MAX),
                )
                .changed();
        });
        ui.horizontal(|ui| {
            ui.label("Optional assets").on_hover_text(
                "These paths may be installed later. Leave empty to require all files.",
            );
            changed |= ui
                .add_sized(
                    [ui.available_width(), 22.0],
                    egui::TextEdit::singleline(&mut self.workspace.deferred_prefixes)
                        .hint_text("voice/, bonus/"),
                )
                .changed();
        });
        if changed {
            self.resources.clear_plan();
        }
    }

    fn release_actions(&mut self, ui: &mut egui::Ui) {
        let width = (ui.available_width() - 28.0 - 4.0 * ui.spacing().item_spacing.x) / 3.0;
        ui.horizontal(|ui| {
            let ready = self.workspace.ready();
            let scan = ui
                .add_enabled_ui(ready, |ui| {
                    icons::button(ui, Icon::Scan, "Scan", width)
                        .on_hover_text("Compare source files with the current package")
                })
                .inner;
            if scan.clicked() {
                self.start_plan();
            }
            icons::show(ui, Icon::Arrow, 14.0, ui.visuals().weak_text_color());
            let build = ui
                .add_enabled_ui(ready && !self.busy, |ui| {
                    icons::button(ui, Icon::Package, "Build", width)
                        .on_hover_text("Build the package in the output folder")
                })
                .inner;
            if build.clicked() {
                self.start_pack();
            }
            icons::show(ui, Icon::Arrow, 14.0, ui.visuals().weak_text_color());
            let can_verify =
                !self.workspace.release.is_empty() && !self.workspace.identity.is_empty();
            let verify = ui
                .add_enabled_ui(can_verify && !self.busy, |ui| {
                    icons::button(ui, Icon::Shield, "Verify", width)
                        .on_hover_text("Check the package and all its segments")
                })
                .inner;
            if verify.clicked() {
                self.start_verify();
            }
        });
    }

    fn release_details(&self, ui: &mut egui::Ui) {
        if let Some(summary) = &self.release.summary {
            ui.horizontal(|ui| {
                icons::show(ui, Icon::Package, 18.0, ui.visuals().text_color());
                ui.label(format!("#{} · {} files", summary.sequence, summary.assets));
            });
        }
        if self.release.last_report.is_none()
            && self.release.summary.is_none()
            && self.resources.plan.is_none()
        {
            return;
        }
        egui::CollapsingHeader::new("Details").show(ui, |ui| {
            if let Some(plan) = &self.resources.plan {
                ui.strong("Source");
                ui.label(format!(
                    "{} total · {} changed",
                    format_size(plan.source_bytes),
                    format_size(plan.changed_source_bytes)
                ));
                if let Some(sequence) = plan.previous_release {
                    ui.small(format!("Against #{sequence}"));
                }
            }
            if let Some(report) = &self.release.last_report {
                ui.strong(format!("Build #{}", report.release_sequence));
                proportion_bar(
                    ui,
                    &[
                        (
                            report.reused_blocks as u64,
                            Color32::from_rgb(132, 163, 194),
                        ),
                        (report.new_blocks as u64, Color32::from_rgb(144, 192, 161)),
                    ],
                );
                ui.label(format!(
                    "{} reused + {} new blocks",
                    report.reused_blocks, report.new_blocks
                ));
                egui::Grid::new("build-report")
                    .striped(true)
                    .show(ui, |ui| {
                        metric(ui, "New segments", report.new_segments.to_string());
                        metric(ui, "Written", format_size(report.new_segment_bytes));
                    });
                ui.small("New data only")
                    .on_hover_text("Written counts new segments, not the total package size.");
            }
            if let Some(summary) = &self.release.summary {
                ui.strong("Package");
                release_summary(ui, summary);
            }
        });
    }

    fn identity_tools(&mut self, ui: &mut egui::Ui) {
        ui.small(
            RichText::new("Private key · do not ship").color(Color32::from_rgb(215, 178, 120)),
        );
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    !self.workspace.identity.is_empty(),
                    egui::Button::new("View"),
                )
                .clicked()
            {
                self.start_identity_load();
            }
            if ui.button("New…").clicked()
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
        fade_visible(ui, "identity-info", self.identity_info.is_some(), |ui| {
            let Some(info) = &self.identity_info else {
                return;
            };
            egui::Grid::new("identity-info-grid")
                .striped(true)
                .show(ui, |ui| {
                    metric(ui, "Project ID", info.project_id.clone());
                    metric(ui, "Public key", info.public_key.clone());
                });
            ui.add_space(8.0);
            ui.small("Keep an offline backup.");
        });
    }

    fn workspace_paths(&mut self, ui: &mut egui::Ui) {
        let mut changed = path_row(
            ui,
            "Assets",
            &mut self.workspace.assets,
            PathPicker::Directory,
            24.0,
        );
        changed |= path_row(
            ui,
            "Output",
            &mut self.workspace.release,
            PathPicker::Directory,
            24.0,
        );
        changed |= path_row(
            ui,
            "Key",
            &mut self.workspace.identity,
            PathPicker::Identity,
            24.0,
        );
        if changed {
            self.clear_workspace_results();
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

    fn status_bar(&self, ui: &mut egui::Ui) {
        egui::Panel::bottom("status")
            .frame(
                egui::Frame::side_top_panel(ui.style()).inner_margin(egui::Margin::symmetric(8, 6)),
            )
            .min_size(32.0)
            .show(ui, |ui| {
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
                ui.label("Game asset packer");
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
) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        let icon = match picker {
            PathPicker::Directory => Icon::Folder,
            PathPicker::Identity => Icon::Key,
        };
        icons::show(ui, icon, 20.0, ui.visuals().text_color());
        ui.add_sized([52.0, row_height], egui::Label::new(label));
        let width = (ui.available_width() - 34.0).max(60.0);
        changed |= ui
            .add_sized([width, row_height], egui::TextEdit::singleline(value))
            .changed();
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
                let selected = path.display().to_string();
                changed |= *value != selected;
                *value = selected;
            }
        }
    });
    changed
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

fn change_color(change: AssetChange) -> Color32 {
    match change {
        AssetChange::Added => Color32::from_rgb(144, 192, 161),
        AssetChange::Modified => Color32::from_rgb(218, 178, 110),
        AssetChange::Removed => Color32::from_rgb(207, 136, 136),
        AssetChange::Unchanged => Color32::from_gray(135),
    }
}

fn proportion_bar(ui: &mut egui::Ui, parts: &[(u64, Color32)]) {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 5.0), egui::Sense::hover());
    let total: u64 = parts.iter().map(|(n, _)| n).sum();
    ui.painter()
        .rect_filled(rect, 0.0, ui.visuals().extreme_bg_color);
    let mut x = rect.left();
    for &(count, color) in parts {
        let width = rect.width() * (count as f64 / total.max(1) as f64) as f32;
        ui.painter().rect_filled(
            egui::Rect::from_min_size(egui::pos2(x, rect.top()), egui::vec2(width, rect.height())),
            0.0,
            color,
        );
        x += width;
    }
}

fn resource_summary(ui: &mut egui::Ui, counts: [u64; 4]) {
    let states = [
        AssetChange::Added,
        AssetChange::Modified,
        AssetChange::Removed,
        AssetChange::Unchanged,
    ];
    let parts = std::array::from_fn::<_, 4, _>(|i| (counts[i], change_color(states[i])));
    proportion_bar(ui, &parts);
    ui.horizontal_wrapped(|ui| {
        for (i, (symbol, label)) in [
            ("+", "Added"),
            ("~", "Changed"),
            ("−", "Removed"),
            ("=", "Same"),
        ]
        .into_iter()
        .enumerate()
        {
            let color = if counts[i] == 0 {
                ui.visuals().weak_text_color()
            } else {
                parts[i].1
            };
            ui.colored_label(color, format!("{symbol} {} {label}", counts[i]));
            if i < 3 {
                ui.separator();
            }
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
        AssetChange::Added => "+ Added",
        AssetChange::Modified => "~ Changed",
        AssetChange::Unchanged => "= Same",
        AssetChange::Removed => "− Removed",
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
    format!("{source_files} files · {changed} changes")
}

fn pack_status(report: &PackReport) -> String {
    if report.changed {
        format!("Built #{}", report.release_sequence)
    } else {
        format!("#{} unchanged", report.release_sequence)
    }
}

fn inspect_release(
    release: &Path,
    keys: &RuntimeKeyMaterial,
    verify: bool,
) -> Result<ReleaseSummary, String> {
    let package = Package::open_directory(
        release.join("game.haku"),
        release.join("data"),
        keys.root_key(),
        keys.public_key,
        ResourceBudget::memory_constrained(),
        OpenPolicy::TrustFirstRelease,
    )
    .map_err(|error| error.to_string())?;
    if package.project_id() != keys.project_id {
        return Err(hakutaku_core::Error::ProjectMismatch.to_string());
    }
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
        if is_hakutaku_key_file(source).map_err(|error| error.to_string())? {
            return Err(format!(
                "Hakutaku key material cannot be imported as a resource: {}",
                source.display()
            ));
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
    if is_hakutaku_key_file(source).map_err(|error| error.to_string())? {
        return Err(format!(
            "Hakutaku key material cannot replace a resource: {}",
            source.display()
        ));
    }
    let parent = target
        .parent()
        .ok_or_else(|| format!("resource has no parent: {}", target.display()))?;
    let name = target
        .file_name()
        .ok_or_else(|| format!("resource has no file name: {}", target.display()))?
        .to_string_lossy();
    let temporary = parent.join(format!(".{name}.hakutaku-part-{}", std::process::id()));
    let permissions = target
        .metadata()
        .map_err(|error| error.to_string())?
        .permissions();
    copy_new_file(source, &temporary, false)?;
    if let Err(error) = std::fs::set_permissions(&temporary, permissions) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error.to_string());
    }
    if let Err(error) = std::fs::rename(&temporary, target) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error.to_string());
    }
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
    fn incremental_reuse_defaults_to_strict_validation() {
        let workspace = Workspace::default();
        let options = workspace.pack_options().unwrap();
        assert!(options.incremental);
        assert!(!options.development_cache);
    }

    #[test]
    fn development_cache_is_explicit_and_inactive_for_full_builds() {
        let mut workspace = Workspace {
            development_cache: true,
            ..Workspace::default()
        };
        assert!(workspace.pack_options().unwrap().development_cache);
        workspace.incremental = false;
        let options = workspace.pack_options().unwrap();
        assert!(!options.incremental);
        assert!(!options.development_cache);
        workspace.incremental = true;
        assert!(workspace.pack_options().unwrap().development_cache);
        workspace.development_cache = false;
        let options = workspace.pack_options().unwrap();
        assert!(options.incremental);
        assert!(!options.development_cache);
    }

    #[test]
    fn changing_workspace_discards_previous_results_and_replace_target() {
        let mut app = App::new();
        app.resources.set_plan(ReleasePlan::default());
        app.resources.selected = Some(0);
        app.resources.visible.push(0);
        app.resources.pending_replace = Some(Replacement {
            source: "old-source".into(),
            target: "old-target".into(),
            logical_path: "old-path".into(),
        });
        app.release.summary = Some(ReleaseSummary {
            sequence: 1,
            assets: 0,
            segments: vec![],
        });
        app.identity_info = Some(IdentityInfo {
            path: "old".into(),
            project_id: String::new(),
            public_key: String::new(),
        });
        app.clear_workspace_results();
        assert!(app.resources.plan.is_none());
        assert_eq!(app.resources.change_counts, [0; 4]);
        assert!(app.resources.selected.is_none());
        assert!(app.resources.visible.is_empty());
        assert!(app.resources.pending_replace.is_none());
        assert!(app.release.summary.is_none());
        assert!(app.identity_info.is_none());
    }

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
        let identity_path = source.join("renamed-resource.bin");
        Identity::generate().unwrap().save(&identity_path).unwrap();
        assert!(import_resources(std::slice::from_ref(&identity_path), &assets).is_err());
        assert!(replace_file(&identity_path, &target).is_err());
        assert_eq!(std::fs::read_dir(&assets).unwrap().count(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_replace_preserves_destination_and_cleans_temporary_file() {
        let root = scratch("replace-failure");
        let target = root.join("directory");
        std::fs::create_dir_all(&target).unwrap();
        let original = target.join("original.bin");
        std::fs::write(&original, b"original").unwrap();
        let source = root.join("source.bin");
        std::fs::write(&source, b"replacement").unwrap();
        assert!(replace_file(&source, &target).is_err());
        assert_eq!(std::fs::read(original).unwrap(), b"original");
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 2);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn resource_filter_preserves_asset_indices_and_refreshes_after_scan() {
        let mut resources = Resources::default();
        let plan = ReleasePlan {
            assets: [
                ("images/A.webp", AssetChange::Added),
                ("images/B.webp", AssetChange::Unchanged),
                ("script/main.txt", AssetChange::Modified),
            ]
            .into_iter()
            .map(|(path, change)| hakutaku_pack::PlannedAsset {
                path: path.into(),
                source_len: Some(1),
                released_len: None,
                access: hakutaku_core::AccessClass::Hot,
                availability: Availability::Required,
                change,
            })
            .collect(),
            ..ReleasePlan::default()
        };
        resources.set_plan(plan.clone());
        assert_eq!(resources.change_counts, [1, 1, 0, 1]);
        resources.update_filter();
        assert_eq!(resources.visible, [0, 2]);
        resources.selected = Some(2);
        resources.search = "WEBP".into();
        resources.update_filter();
        assert_eq!(resources.visible, [0]);
        assert_eq!(resources.selected, Some(2));
        resources.show_unchanged = true;
        resources.update_filter();
        assert_eq!(resources.visible, [0, 1]);
        let mut rescanned = plan;
        rescanned.assets.remove(0);
        resources.set_plan(rescanned);
        assert_eq!(resources.change_counts, [0, 1, 0, 1]);
        resources.update_filter();
        assert_eq!(resources.visible, [0]);
        assert_eq!(resources.selected, None);
        resources.search = "missing".into();
        resources.update_filter();
        assert!(resources.visible.is_empty());
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
