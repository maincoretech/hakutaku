use egui::{Align, Color32, RichText};
use hakutaku_core::{AssetInfo, Package, ResourceBudget};
use hakutaku_pack::{
    Identity, PackOptions, PackProgress, PackReport, pack_directory_with_progress,
};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

fn main() {
    let viewport = egui::ViewportBuilder::default()
        .with_inner_size([720.0, 500.0])
        .with_resizable(true)
        .with_min_inner_size([520.0, 360.0]);
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
    Pack,
    Browse,
    Bench,
}

struct PackForm {
    input: String,
    output: String,
    identity: String,
    incremental: bool,
    compression_level: i32,
    segment_mib: u64,
    deferred_prefixes: String,
}

impl Default for PackForm {
    fn default() -> Self {
        Self {
            input: String::new(),
            output: String::new(),
            identity: String::new(),
            incremental: true,
            compression_level: 3,
            segment_mib: 512,
            deferred_prefixes: String::new(),
        }
    }
}

#[derive(Default)]
struct BrowseForm {
    release: String,
    identity: String,
    search: String,
    assets: Vec<AssetInfo>,
    package: Option<Package>,
    selected: HashSet<usize>,
}

#[derive(Default)]
struct BenchForm {
    release: String,
    identity: String,
    result: Option<BenchResult>,
}

#[derive(Clone, Debug)]
struct BenchResult {
    open_ms: f64,
    sequential_mib_s: f64,
    random_iops: f64,
    bytes: u64,
    requests: u64,
}

enum Message {
    Progress(PackProgress),
    Packed(std::result::Result<PackReport, String>),
    Loaded(std::result::Result<(Package, Vec<AssetInfo>, usize), String>),
    Extracted(std::result::Result<usize, String>),
    Benchmarked(std::result::Result<BenchResult, String>),
    IdentityCreated(std::result::Result<String, String>),
}

struct App {
    tab: Tab,
    active_tab: Tab,
    tab_fade: f32,
    pack: PackForm,
    browse: BrowseForm,
    bench: BenchForm,
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
            tab: Tab::Pack,
            active_tab: Tab::Pack,
            tab_fade: 1.0,
            pack: PackForm::default(),
            browse: BrowseForm::default(),
            bench: BenchForm::default(),
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
        while let Ok(message) = self.receiver.try_recv() {
            match message {
                Message::Progress(progress) => {
                    self.status = progress.current_path.as_ref().map_or_else(
                        || progress.phase.into(),
                        |path| format!("{}: {path}", progress.phase),
                    );
                    self.progress = Some(progress);
                }
                Message::Packed(result) => {
                    self.busy = false;
                    self.progress = None;
                    self.status = match result {
                        Ok(report) if report.changed => format!(
                            "Release {}: {} reused, {} new blocks, {} new segment(s)",
                            report.release_sequence,
                            report.reused_blocks,
                            report.new_blocks,
                            report.new_segments
                        ),
                        Ok(report) => format!(
                            "No changes; release {} remains active",
                            report.release_sequence
                        ),
                        Err(error) => format!("Pack failed: {error}"),
                    };
                }
                Message::Loaded(result) => {
                    self.busy = false;
                    match result {
                        Ok((package, assets, deferred_segments)) => {
                            self.status = format!(
                                "Loaded release {} with {} assets, {} deferred segment(s)",
                                package.release_sequence(),
                                assets.len(),
                                deferred_segments,
                            );
                            self.browse.package = Some(package);
                            self.browse.assets = assets;
                            self.browse.selected.clear();
                        }
                        Err(error) => self.status = format!("Load failed: {error}"),
                    }
                }
                Message::Extracted(result) => {
                    self.busy = false;
                    self.status = result.map_or_else(
                        |error| format!("Extract failed: {error}"),
                        |count| format!("Extracted {count} asset(s)"),
                    );
                }
                Message::Benchmarked(result) => {
                    self.busy = false;
                    match result {
                        Ok(result) => {
                            self.status = "Benchmark complete".into();
                            self.bench.result = Some(result);
                        }
                        Err(error) => self.status = format!("Benchmark failed: {error}"),
                    }
                }
                Message::IdentityCreated(result) => {
                    self.busy = false;
                    self.status = result.map_or_else(
                        |error| format!("Identity creation failed: {error}"),
                        |path| {
                            self.pack.identity.clone_from(&path);
                            format!("Created publisher identity: {path}")
                        },
                    );
                }
            }
        }
    }

    fn start_pack(&mut self) {
        let form = PackForm {
            input: self.pack.input.clone(),
            output: self.pack.output.clone(),
            identity: self.pack.identity.clone(),
            incremental: self.pack.incremental,
            compression_level: self.pack.compression_level,
            segment_mib: self.pack.segment_mib,
            deferred_prefixes: self.pack.deferred_prefixes.clone(),
        };
        let sender = self.sender.clone();
        self.begin_work("Starting pack…");
        std::thread::spawn(move || {
            let result = (|| {
                let identity = Identity::load(&form.identity).map_err(|error| error.to_string())?;
                let mut options = PackOptions::new(&form.input, &form.output);
                options.incremental = form.incremental;
                options.compression_level = form.compression_level;
                options.segment_target_bytes = form
                    .segment_mib
                    .checked_mul(1024 * 1024)
                    .ok_or_else(|| "segment size overflow".to_owned())?;
                options.deferred_prefixes = form
                    .deferred_prefixes
                    .split([',', '\n'])
                    .map(str::trim)
                    .filter(|prefix| !prefix.is_empty())
                    .map(str::to_owned)
                    .collect();
                pack_directory_with_progress(&options, &identity, |progress| {
                    let _ = sender.send(Message::Progress(progress));
                })
                .map_err(|error| error.to_string())
            })();
            let _ = sender.send(Message::Packed(result));
        });
    }

    fn start_load(&mut self) {
        let release = self.browse.release.clone();
        let identity = self.browse.identity.clone();
        let sender = self.sender.clone();
        self.begin_work("Opening snapshot…");
        std::thread::spawn(move || {
            let result = open_release(&release, &identity).and_then(|package| {
                let assets = package.list_assets().map_err(|error| error.to_string())?;
                let deferred_segments = package
                    .list_segments()
                    .map_err(|error| error.to_string())?
                    .into_iter()
                    .filter(|segment| segment.availability == hakutaku_core::Availability::Deferred)
                    .count();
                Ok((package, assets, deferred_segments))
            });
            let _ = sender.send(Message::Loaded(result));
        });
    }

    fn start_extract(&mut self, output: PathBuf) {
        let Some(package) = self.browse.package.clone() else {
            return;
        };
        let paths: Vec<String> = if self.browse.selected.is_empty() {
            self.browse
                .assets
                .iter()
                .map(|asset| asset.path.clone())
                .collect()
        } else {
            self.browse
                .selected
                .iter()
                .filter_map(|index| self.browse.assets.get(*index))
                .map(|asset| asset.path.clone())
                .collect()
        };
        let sender = self.sender.clone();
        self.begin_work("Extracting…");
        std::thread::spawn(move || {
            let result =
                extract_assets(&package, &paths, &output).map_err(|error| error.to_string());
            let _ = sender.send(Message::Extracted(result));
        });
    }

    fn start_bench(&mut self) {
        let release = self.bench.release.clone();
        let identity = self.bench.identity.clone();
        let sender = self.sender.clone();
        self.begin_work("Benchmarking runtime reads…");
        self.bench.result = None;
        std::thread::spawn(move || {
            let result = benchmark_release(&release, &identity);
            let _ = sender.send(Message::Benchmarked(result));
        });
    }

    fn create_identity(&mut self, path: PathBuf) {
        let sender = self.sender.clone();
        self.begin_work("Creating publisher identity…");
        std::thread::spawn(move || {
            let display = path.display().to_string();
            let result = Identity::generate()
                .and_then(|identity| identity.save(&path))
                .map(|()| display)
                .map_err(|error| error.to_string());
            let _ = sender.send(Message::IdentityCreated(result));
        });
    }

    fn begin_work(&mut self, status: &str) {
        self.busy = true;
        self.status = status.into();
        self.started = Some(Instant::now());
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
                    ui.selectable_value(&mut self.tab, Tab::Pack, "  Pack  ");
                    ui.selectable_value(&mut self.tab, Tab::Browse, " Browse ");
                    ui.selectable_value(&mut self.tab, Tab::Bench, " Bench ");
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
                Tab::Pack => self.pack_page(ui),
                Tab::Browse => self.browse_page(ui),
                Tab::Bench => self.bench_page(ui),
            });
        });

        egui::TopBottomPanel::bottom("status")
            .frame(
                egui::Frame::side_top_panel(&context.style())
                    .inner_margin(egui::Margin::symmetric(8, 6)),
            )
            .min_height(32.0)
            .show(context, |ui| {
                ui.horizontal(|ui| {
                    ui.scope(|ui| {
                        ui.multiply_opacity(self.tab_fade);
                        match self.tab {
                            Tab::Pack => {
                                let ready = !self.pack.input.is_empty()
                                    && !self.pack.output.is_empty()
                                    && !self.pack.identity.is_empty();
                                if ui
                                    .add_enabled(ready && !self.busy, egui::Button::new("Pack"))
                                    .clicked()
                                {
                                    self.start_pack();
                                }
                            }
                            Tab::Browse => {
                                let ready = !self.browse.release.is_empty()
                                    && !self.browse.identity.is_empty();
                                if ui
                                    .add_enabled(ready && !self.busy, egui::Button::new("Load"))
                                    .clicked()
                                {
                                    self.start_load();
                                }
                            }
                            Tab::Bench => {
                                let ready = !self.bench.release.is_empty()
                                    && !self.bench.identity.is_empty();
                                if ui
                                    .add_enabled(ready && !self.busy, egui::Button::new("Run"))
                                    .clicked()
                                {
                                    self.start_bench();
                                }
                            }
                        }
                    });
                    ui.separator();
                    if self.busy
                        && let Some(progress) = &self.progress
                        && progress.total_bytes > 0
                    {
                        let fraction =
                            progress.completed_bytes as f32 / progress.total_bytes as f32;
                        ui.add(
                            egui::ProgressBar::new(fraction.clamp(0.0, 1.0))
                                .desired_width(ui.available_width())
                                .text(format!("{:.0}%", fraction * 100.0)),
                        );
                    } else {
                        ui.label(&self.status);
                        if let Some(started) = self.started.filter(|_| self.busy) {
                            ui.label(format!("{:.1}s", started.elapsed().as_secs_f64()));
                        }
                    }
                });
            });

        let mut show_about = self.show_about;
        egui::Window::new("About Hakutaku")
            .open(&mut show_about)
            .resizable(false)
            .collapsible(false)
            .show(context, |ui| {
                ui.heading("Hakutaku");
                ui.label(format!("Version {}", env!("CARGO_PKG_VERSION")));
                ui.label("Authenticated random-access resources for offline games");
                ui.hyperlink("https://github.com/maincoretech/hakutaku");
                ui.add_space(8.0);
                ui.label("AES-256-GCM · Ed25519 · BLAKE3 · zstd");
            });
        self.show_about = show_about;
    }
}

impl App {
    fn pack_page(&mut self, ui: &mut egui::Ui) {
        ui.spacing_mut().item_spacing.y = 8.0;
        ui.heading("Pack Release");
        let row_height = 22.0;
        path_row(
            ui,
            "Assets:",
            &mut self.pack.input,
            PathPicker::Directory,
            row_height,
        );
        path_row(
            ui,
            "Release:",
            &mut self.pack.output,
            PathPicker::Directory,
            row_height,
        );
        ui.horizontal(|ui| {
            ui.label("Identity:");
            let width = (ui.available_width() - 72.0).max(60.0);
            ui.add_sized(
                [width, row_height],
                egui::TextEdit::singleline(&mut self.pack.identity)
                    .hint_text("publisher.hakutaku-key"),
            );
            if ui.add(row_button("…", row_height)).clicked()
                && let Some(path) = rfd::FileDialog::new()
                    .add_filter("Hakutaku Identity", &["hakutaku-key"])
                    .pick_file()
            {
                self.pack.identity = path.display().to_string();
            }
            if ui.add(row_button("New", row_height)).clicked()
                && let Some(path) = rfd::FileDialog::new()
                    .set_file_name("publisher.hakutaku-key")
                    .save_file()
            {
                self.create_identity(path);
            }
        });
        ui.small(
            RichText::new("The identity contains the signing key and must never be shipped.")
                .color(Color32::from_rgb(215, 178, 120)),
        );
        ui.horizontal(|ui| {
            ui.label("Deferred:");
            ui.add_sized(
                [ui.available_width(), row_height],
                egui::TextEdit::singleline(&mut self.pack.deferred_prefixes)
                    .hint_text("optional prefixes, comma-separated"),
            );
        });
        ui.small("Deferred prefixes are isolated into on-demand segments.");
        ui.horizontal(|ui| {
            ui.label("Options:");
            ui.checkbox(&mut self.pack.incremental, "Reuse current release");
            ui.separator();
            ui.label("zstd");
            ui.add(egui::DragValue::new(&mut self.pack.compression_level).range(-7..=22));
            ui.separator();
            ui.label("Segment MiB");
            ui.add(egui::DragValue::new(&mut self.pack.segment_mib).range(1..=4096));
        });
    }

    fn browse_page(&mut self, ui: &mut egui::Ui) {
        ui.spacing_mut().item_spacing.y = 4.0;
        ui.heading("Browse Release");
        let row_height = 22.0;
        path_row(
            ui,
            "Release:",
            &mut self.browse.release,
            PathPicker::Directory,
            row_height,
        );
        path_row(
            ui,
            "Identity:",
            &mut self.browse.identity,
            PathPicker::Identity,
            row_height,
        );
        ui.horizontal(|ui| {
            ui.add_sized(
                [240.0, row_height],
                egui::TextEdit::singleline(&mut self.browse.search).hint_text("Filter assets…"),
            );
            let label = if self.browse.selected.is_empty() {
                "Extract All".to_owned()
            } else {
                format!("Extract Selected ({})", self.browse.selected.len())
            };
            if ui
                .add_enabled(self.browse.package.is_some(), egui::Button::new(label))
                .clicked()
                && let Some(directory) = rfd::FileDialog::new().pick_folder()
            {
                self.start_extract(directory);
            }
        });
        ui.separator();
        let query = self.browse.search.to_ascii_lowercase();
        egui::ScrollArea::vertical().show(ui, |ui| {
            egui::Grid::new("asset-list")
                .striped(true)
                .num_columns(3)
                .show(ui, |ui| {
                    ui.strong("Path");
                    ui.strong("Size");
                    ui.strong("Class");
                    ui.end_row();
                    for (index, asset) in self.browse.assets.iter().enumerate() {
                        if !query.is_empty() && !asset.path.to_ascii_lowercase().contains(&query) {
                            continue;
                        }
                        let selected = self.browse.selected.contains(&index);
                        if ui.selectable_label(selected, &asset.path).clicked() {
                            if selected {
                                self.browse.selected.remove(&index);
                            } else {
                                self.browse.selected.insert(index);
                            }
                        }
                        ui.label(format_size(asset.len));
                        ui.label(format!("{:?}", asset.access));
                        ui.end_row();
                    }
                });
        });
    }

    fn bench_page(&mut self, ui: &mut egui::Ui) {
        ui.spacing_mut().item_spacing.y = 8.0;
        ui.heading("Runtime Benchmark");
        let row_height = 22.0;
        path_row(
            ui,
            "Release:",
            &mut self.bench.release,
            PathPicker::Directory,
            row_height,
        );
        path_row(
            ui,
            "Identity:",
            &mut self.bench.identity,
            PathPicker::Identity,
            row_height,
        );
        ui.label("Authenticated reads through the same core used by the engine.");
        if let Some(result) = &self.bench.result {
            ui.add_space(16.0);
            egui::Grid::new("bench-result")
                .striped(true)
                .show(ui, |ui| {
                    metric(ui, "Open + signature", format!("{:.2} ms", result.open_ms));
                    metric(
                        ui,
                        "Sequential read",
                        format!("{:.1} MiB/s", result.sequential_mib_s),
                    );
                    metric(
                        ui,
                        "Random 4 KiB",
                        format!("{:.0} IOPS", result.random_iops),
                    );
                    metric(ui, "Bytes read", format_size(result.bytes));
                    metric(ui, "Random requests", result.requests.to_string());
                });
        }
    }
}

#[derive(Clone, Copy)]
enum PathPicker {
    Directory,
    Identity,
}

fn row_button(text: impl Into<egui::WidgetText>, height: f32) -> egui::Button<'static> {
    egui::Button::new(text).min_size(egui::vec2(0.0, height))
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
        if ui.add(row_button("…", row_height)).clicked() {
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

fn metric(ui: &mut egui::Ui, label: &str, value: String) {
    ui.label(label);
    ui.strong(value);
    ui.end_row();
}

fn open_release(release: &str, identity_path: &str) -> Result<Package, String> {
    let identity = Identity::load(identity_path).map_err(|error| error.to_string())?;
    Package::open_directory(
        Path::new(release).join("game.haku"),
        Path::new(release).join("data"),
        identity.root_key(),
        identity.public_key(),
        ResourceBudget::default(),
    )
    .map_err(|error| error.to_string())
}

fn extract_assets(package: &Package, paths: &[String], output: &Path) -> Result<usize, String> {
    for path in paths {
        let target = output.join(path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        let mut source = package
            .asset(path)
            .map_err(|error| error.to_string())?
            .cursor();
        let mut destination = std::fs::File::create(target).map_err(|error| error.to_string())?;
        std::io::copy(&mut source, &mut destination).map_err(|error| error.to_string())?;
    }
    Ok(paths.len())
}

fn benchmark_release(release: &str, identity_path: &str) -> Result<BenchResult, String> {
    let opened = Instant::now();
    let package = open_release(release, identity_path)?;
    let open_ms = opened.elapsed().as_secs_f64() * 1000.0;
    let assets = package.list_assets().map_err(|error| error.to_string())?;

    package.trim();
    let sequential_start = Instant::now();
    let mut bytes = 0_u64;
    let mut buffer = vec![0_u8; 256 * 1024];
    for info in &assets {
        let mut source = package
            .asset(&info.path)
            .map_err(|error| error.to_string())?
            .cursor();
        loop {
            let read =
                std::io::Read::read(&mut source, &mut buffer).map_err(|error| error.to_string())?;
            if read == 0 {
                break;
            }
            bytes = bytes.saturating_add(read as u64);
        }
    }
    let sequential_seconds = sequential_start.elapsed().as_secs_f64().max(f64::EPSILON);
    let sequential_mib_s = bytes as f64 / (1024.0 * 1024.0) / sequential_seconds;

    package.trim();
    let non_empty: Vec<_> = assets.iter().filter(|asset| asset.len > 0).collect();
    let requests = if non_empty.is_empty() { 0 } else { 2_000 };
    let mut buffer = [0_u8; 4096];
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    let random_start = Instant::now();
    for request in 0..requests {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let info = non_empty[(state as usize) % non_empty.len()];
        let readable = info.len.min(buffer.len() as u64);
        let maximum_offset = info.len - readable;
        let offset = if maximum_offset == 0 {
            0
        } else {
            state % (maximum_offset + 1)
        };
        package
            .asset(&info.path)
            .and_then(|asset| asset.read_at(offset, &mut buffer[..readable as usize]))
            .map_err(|error| error.to_string())?;
        if request % 32 == 0 {
            package.trim();
        }
    }
    let random_seconds = random_start.elapsed().as_secs_f64().max(f64::EPSILON);
    Ok(BenchResult {
        open_ms,
        sequential_mib_s,
        random_iops: requests as f64 / random_seconds,
        bytes,
        requests,
    })
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
