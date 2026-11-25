use eframe::egui;
use reqwest::blocking::{Client, Response};
use reqwest::header::{HeaderName, CONTENT_RANGE, ETAG, RANGE};
use reqwest::StatusCode;
use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicU8, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

// TODO(2단계): 화면·다운로드 엔진에 연결하면 allow 를 뗀다
#[allow(dead_code)]
mod s3;

const REPO: &str = "bluelang1/url-downloader";

/// 분할 구간 하나의 최소 크기. 이보다 작은 파일은 분할하지 않는다
const MIN_CHUNK: u64 = 1024 * 1024;
const BUF: usize = 64 * 1024;
/// 구간 하나가 끊겼을 때 자동 재시도 횟수
const RETRIES: usize = 3;

#[derive(Clone)]
enum Status {
    Waiting,
    Downloading { done: u64, total: Option<u64>, parts: usize },
    Finished,
    Failed(String),
    /// 사용자가 멈춤. resumable 이면 .part/.meta 가 남아 있어 이어받기 가능,
    /// 아니면(Range 미지원 서버) 다시 받을 때 처음부터 받는다
    Paused { done: u64, total: Option<u64>, resumable: bool },
    /// 사용자가 취소. 받던 파일은 지움
    Cancelled,
}

/// 다운로드 스레드에 보내는 신호
const STOP_NONE: u8 = 0;
const STOP_PAUSE: u8 = 1;
const STOP_CANCEL: u8 = 2;
/// 멈춤 신호로 끝났음을 나타내는 오류 문자열
const STOPPED: &str = "\u{0}stopped";
/// 저장할 이름의 파일이 이미 있을 때의 오류
const EXISTS: &str = "같은 이름의 파일이 이미 있습니다";

struct Item {
    line: usize,
    url: String,
    name: String,
    /// 이 항목을 저장하는 폴더 (시작할 때 정해지고 바뀌지 않는다)
    dir: PathBuf,
    status: Status,
    selected: bool,
    stop: Arc<AtomicU8>,
    /// 바이트/초 (지수 이동 평균)
    speed: f32,
    /// 속도 계산용 마지막 표본 (시각, 받은 바이트)
    sample: Option<(Instant, u64)>,
    /// 세대 번호. (재)시작할 때마다 1 올리고, 큐에 든 옛 세대 작업은 워커가 건너뛴다
    gen: Arc<AtomicU64>,
}

impl Item {
    fn new(line: usize, url: String, name: String, dir: PathBuf) -> Self {
        Self {
            line,
            url,
            name,
            dir,
            status: Status::Waiting,
            selected: false,
            stop: Arc::new(AtomicU8::new(STOP_NONE)),
            speed: 0.0,
            sample: None,
            gen: Arc::new(AtomicU64::new(0)),
        }
    }

    fn dest(&self) -> PathBuf {
        self.dir.join(&self.name)
    }

    fn is_unfinished(&self) -> bool {
        matches!(self.status, Status::Waiting | Status::Downloading { .. } | Status::Paused { .. })
    }
}

type Shared = Arc<Mutex<Vec<Item>>>;
/// 작업 큐: (항목 번호, 넣을 때의 세대 번호)
type Queue = Arc<Mutex<VecDeque<(usize, u64)>>>;

struct App {
    input: String,
    folder: Option<PathBuf>,
    items: Shared,
    /// 한 번에 받는 파일 수
    concurrent: usize,
    /// 파일 하나를 몇 개 연결로 나눠 받을지
    segments: usize,
    view: View,
    /// 도움말에서 목차를 눌러 이동할 절 번호
    help_jump: Option<usize>,
    help_query: String,
    update: Arc<Mutex<UpdateState>>,
    /// 개발용: 몇 프레임 그린 뒤 화면을 파일로 저장하고 종료 (URL_DOWNLOADER_SCREENSHOT=경로.ppm)
    screenshot: Option<(PathBuf, u32)>,
    /// 직전 프레임에 다운로드가 돌고 있었는지 (완료 알림용)
    was_running: bool,
    last_update_check: Instant,
    /// 마지막으로 저장한 설정 (바뀌었을 때만 다시 저장)
    saved_config: String,
    autostart: bool,
    theme: Theme,
    /// 워커 풀: 대기 작업 큐와 지금 돌고 있는 워커 수 (동시 파일 한도를 지킨다)
    queue: Queue,
    active_workers: Arc<AtomicUsize>,
}

/// 화면 테마. 메뉴 오른쪽 버튼으로 데이 → 나이트 → 시스템 순으로 바뀐다
#[derive(Clone, Copy, PartialEq, Debug)]
enum Theme {
    Light,
    Dark,
    System,
}

impl Theme {
    fn key(self) -> &'static str {
        match self {
            Theme::Light => "light",
            Theme::Dark => "dark",
            Theme::System => "system",
        }
    }

    fn from_key(s: &str) -> Option<Self> {
        match s {
            "light" => Some(Theme::Light),
            "dark" => Some(Theme::Dark),
            "system" => Some(Theme::System),
            _ => None,
        }
    }

    fn next(self) -> Self {
        match self {
            Theme::Light => Theme::Dark,
            Theme::Dark => Theme::System,
            Theme::System => Theme::Light,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Theme::Light => "☀ 데이",
            Theme::Dark => "🌙 나이트",
            Theme::System => "🖥 시스템",
        }
    }

    fn preference(self) -> egui::ThemePreference {
        match self {
            Theme::Light => egui::ThemePreference::Light,
            Theme::Dark => egui::ThemePreference::Dark,
            Theme::System => egui::ThemePreference::System,
        }
    }
}

/// 상태 색. 다크 테마의 밝은 색은 흰 바탕에서 안 보이므로 라이트 테마에선 진한 색을 쓴다
#[derive(Clone, Copy)]
enum Tone {
    Ok,
    Bad,
    Info,
    Warn,
    Muted,
    Off,
}

/// 진행바 채움 색. 진행바 글자는 테마의 selection 글자색(다크: 밝은 하늘색, 라이트: 진한 남색)이고
/// 채워지지 않은 부분 위에도 그려지므로, 라이트 모드에서는 연한 채움 색을 써서 진한 글자가 읽히게 한다
fn bar_fill(ui: &egui::Ui, t: Tone) -> egui::Color32 {
    use egui::Color32 as C;
    match (ui.visuals().dark_mode, t) {
        (true, Tone::Ok) => C::from_rgb(60, 160, 90),
        (true, _) => C::from_rgb(160, 140, 40),
        (false, Tone::Ok) => C::from_rgb(150, 215, 160),
        (false, _) => C::from_rgb(240, 205, 100),
    }
}

fn tone(ui: &egui::Ui, t: Tone) -> egui::Color32 {
    use egui::Color32 as C;
    if ui.visuals().dark_mode {
        match t {
            Tone::Ok => C::LIGHT_GREEN,
            Tone::Bad => C::LIGHT_RED,
            Tone::Info => C::LIGHT_BLUE,
            Tone::Warn => C::LIGHT_YELLOW,
            Tone::Muted => C::GRAY,
            Tone::Off => C::DARK_GRAY,
        }
    } else {
        match t {
            Tone::Ok => C::from_rgb(20, 130, 50),
            Tone::Bad => C::from_rgb(200, 40, 40),
            Tone::Info => C::from_rgb(30, 90, 200),
            Tone::Warn => C::from_rgb(160, 110, 0),
            Tone::Muted => C::from_rgb(110, 110, 110),
            Tone::Off => C::from_rgb(150, 150, 150),
        }
    }
}

/// 메인 영역에 보여줄 화면
#[derive(Clone, Copy, PartialEq)]
enum View {
    Download,
    Help,
    About,
}

/// 업데이트 확인·적용 진행 상태
#[derive(Clone)]
enum UpdateState {
    Checking,
    UpToDate,
    Available { version: String, url: String },
    Downloading { done: u64, total: Option<u64> },
    Installing,
    Error(String),
}

impl Default for App {
    fn default() -> Self {
        Self {
            input: String::new(),
            folder: None,
            items: Default::default(),
            concurrent: 3,
            segments: 4,
            view: View::Download,
            help_jump: None,
            help_query: String::new(),
            screenshot: std::env::var("URL_DOWNLOADER_SCREENSHOT").ok().map(|p| (PathBuf::from(p), 0)),
            was_running: false,
            last_update_check: Instant::now(),
            saved_config: String::new(),
            autostart: false,
            theme: Theme::System,
            update: Arc::new(Mutex::new(UpdateState::Checking)),
            queue: Default::default(),
            active_workers: Default::default(),
        }
    }
}

const REPO_URL: &str = "https://github.com/bluelang1/url-downloader";
const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() -> eframe::Result {
    // 터미널용: --version, --self-update <zip-url> (업데이트 교체 로직을 GUI 없이 검증)
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("--version") => {
            println!("url_downloader {VERSION}");
            return Ok(());
        }
        Some("--check-update") => {
            match fetch_latest_release() {
                Ok(Some((v, url))) => println!("available: v{v} {url}"),
                Ok(None) => println!("up to date ({VERSION})"),
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            }
            return Ok(());
        }
        Some("--self-update") => {
            let url = args.get(2).expect("--self-update <zip-url>");
            match install_update(url, &|s| {
                if let UpdateState::Installing = s {
                    eprintln!("installing…");
                }
            }) {
                Ok(p) => println!("updated: {}", p.display()),
                Err(e) => {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            }
            return Ok(());
        }
        _ => {}
    }
    let icon = eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon-1024.png"))
        .expect("assets/icon-1024.png 는 유효한 PNG 여야 한다");
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([720.0, 580.0])
            .with_icon(icon),
        ..Default::default()
    };
    eframe::run_native(
        &format!("URL Downloader (v{VERSION})"),
        options,
        Box::new(|cc| {
            setup_fonts(&cc.egui_ctx);
            let mut app = App::default();
            app.load_config();
            cc.egui_ctx.set_theme(app.theme.preference());
            // 개발용: 입력 미리 채우기 / 첫 프레임에 자동 시작 (화면 검증)
            if let Ok(text) = std::env::var("URL_DOWNLOADER_INPUT") {
                app.input = text.replace("\\n", "\n").replace("\\t", "\t");
            }
            app.autostart = std::env::var("URL_DOWNLOADER_AUTOSTART").is_ok();
            app.view = match std::env::var("URL_DOWNLOADER_START_VIEW").as_deref() {
                Ok("help") => View::Help,
                Ok("about") => View::About,
                _ => View::Download,
            };
            app.check_update();
            Ok(Box::new(app))
        }),
    )
}

// macOS 기본 한글 폰트 로드 (없으면 egui 기본 폰트 사용)
fn setup_fonts(ctx: &egui::Context) {
    let candidates = [
        "/System/Library/Fonts/AppleSDGothicNeo.ttc",
        "/System/Library/Fonts/Supplemental/AppleGothic.ttf",
    ];
    for path in candidates {
        if let Ok(bytes) = std::fs::read(path) {
            let mut fonts = egui::FontDefinitions::default();
            fonts
                .font_data
                .insert("korean".into(), egui::FontData::from_owned(bytes).into());
            for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                fonts
                    .families
                    .entry(family)
                    .or_default()
                    .insert(0, "korean".into());
            }
            ctx.set_fonts(fonts);
            return;
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.debug_screenshot(ctx);
        let running = self.is_running();
        if running {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        } else {
            // 매시간 업데이트 재확인이 멈춰 있지 않도록 가끔 깨운다
            ctx.request_repaint_after(Duration::from_secs(60));
        }
        self.notify_if_finished(running);
        self.was_running = running;
        self.hourly_update_check();
        self.accept_dropped_files(ctx);
        self.save_config_if_changed();
        if std::mem::take(&mut self.autostart) {
            if let Some(dir) = self.folder.clone() {
                let urls = parse_urls(&self.input);
                self.start(urls, dir);
            }
        }

        egui::TopBottomPanel::top("menu").show(ctx, |ui| {
            egui::menu::bar(ui, |ui| {
                if ui.selectable_label(self.view == View::Download, "다운로드").clicked() {
                    self.view = View::Download;
                }
                ui.menu_button("정보", |ui| {
                    if ui.button("URL Downloader 정보").clicked() {
                        self.view = View::About;
                        ui.close_menu();
                    }
                    if ui.button("GitHub 저장소 열기").clicked() {
                        ctx.open_url(egui::OpenUrl::new_tab(REPO_URL));
                        ui.close_menu();
                    }
                });
                ui.menu_button("도움말", |ui| {
                    if ui.button("사용법 전체 보기   (F1)").clicked() {
                        self.view = View::Help;
                        self.help_jump = Some(0);
                        ui.close_menu();
                    }
                    ui.separator();
                    for (i, sec) in HELP.iter().enumerate() {
                        if ui.button(format!("{}. {}", i + 1, sec.title)).clicked() {
                            self.view = View::Help;
                            self.help_jump = Some(i);
                            ui.close_menu();
                        }
                    }
                });
                // 오른쪽 끝: 테마 순환 버튼 (데이 → 나이트 → 시스템)
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let next = self.theme.next();
                    if ui
                        .button(self.theme.label())
                        .on_hover_text(format!("화면 테마 (누르면 {} 로 바뀝니다)", next.label()))
                        .clicked()
                    {
                        self.theme = next;
                        ctx.set_theme(next.preference());
                    }
                });
            });
        });
        // 단축키: F1 도움말, Esc 로 다운로드 화면 복귀
        if ctx.input(|i| i.key_pressed(egui::Key::F1)) {
            self.view = View::Help;
        }
        if self.view != View::Download && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.view = View::Download;
        }

        match self.view {
            View::Help => {
                egui::CentralPanel::default().show(ctx, |ui| self.help_view(ui));
                return;
            }
            View::About => {
                egui::CentralPanel::default().show(ctx, |ui| self.about_view(ui));
                return;
            }
            View::Download => {}
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("URL Downloader");
                ui.label(egui::RichText::new(format!("(v{VERSION})")).color(tone(ui, Tone::Muted)).size(15.0));
                ui.add_space(6.0);
                self.update_bar(ui);
            });
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("링크 목록 (한 줄에 하나)").strong());
                ui.label(
                    egui::RichText::new("형식: URL  또는  파일명<Tab>URL  /  파일명 | URL   (확장자는 원본 그대로)")
                        .color(tone(ui, Tone::Muted))
                        .small(),
                );
            });
            ui.add_space(4.0);

            let mono = egui::TextStyle::Monospace;
            let row_h = ui.text_style_height(&mono);
            let line_count = self.input.split('\n').count().max(1);
            egui::ScrollArea::vertical()
                .id_salt("input")
                .max_height(200.0)
                .show(ui, |ui| {
                    ui.horizontal_top(|ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        // 줄 번호 거터
                        let gutter: String = (1..=line_count)
                            .map(|n| format!("{n:>3}"))
                            .collect::<Vec<_>>()
                            .join("\n");
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(gutter).text_style(mono.clone()).color(tone(ui, Tone::Muted)),
                            )
                            .selectable(false),
                        );
                        ui.add_sized(
                            [ui.available_width(), (line_count.max(8)) as f32 * row_h + 8.0],
                            egui::TextEdit::multiline(&mut self.input)
                                .font(mono.clone())
                                .hint_text("https://...  또는  파일명<Tab>https://..."),
                        );
                    });
                });

            ui.add_space(8.0);
            // 끝나지 않은 항목(대기·진행·일시정지)이 있으면 폴더를 못 바꾸고 새로 시작하지도 못한다:
            // 그 항목들은 처음 폴더에 이어서 받고, 새 목록은 이전 목록을 통째로 바꾸기 때문이다
            let unfinished = self.items.lock().unwrap().iter().any(Item::is_unfinished);
            ui.horizontal(|ui| {
                let urls = parse_urls(&self.input);
                let mut btn = ui.add_enabled(
                    !unfinished && !urls.is_empty(),
                    egui::Button::new(format!("다운로드 ({}개)", urls.len())),
                );
                if unfinished {
                    btn = btn.on_disabled_hover_text("진행 중이거나 일시정지된 항목이 있습니다. 끝내거나 취소한 뒤 시작하세요");
                }
                if btn.clicked() {
                    // 기억해 둔 폴더가 있으면 바로 시작, 없으면 고른다
                    let dir = match self.folder.clone().filter(|d| d.is_dir()) {
                        Some(d) => Some(d),
                        None => pick_folder(),
                    };
                    if let Some(dir) = dir {
                        self.folder = Some(dir.clone());
                        self.start(urls, dir);
                    }
                }
                // 일시정지 항목까지 목록에서 지우면 [이어받기] 버튼이 사라지므로 미완료 항목이 있으면 막는다
                let clear = ui.add_enabled(!unfinished, egui::Button::new("지우기"));
                if clear.on_disabled_hover_text("진행 중이거나 일시정지된 항목이 있습니다").clicked() {
                    self.input.clear();
                    self.queue.lock().unwrap().clear();
                    self.items.lock().unwrap().clear();
                }
                ui.separator();
                ui.label("동시 파일");
                ui.add_enabled(!running, egui::DragValue::new(&mut self.concurrent).range(1..=16))
                    .on_hover_text("한 번에 받는 파일 수");
                ui.label("파일당 분할");
                ui.add_enabled(!running, egui::DragValue::new(&mut self.segments).range(1..=16))
                    .on_hover_text("파일 하나를 몇 개 연결로 나눠 받을지\n(서버가 Range 를 지원하고 1MB 이상일 때만 적용)");
            });
            const FOLDER_LOCKED: &str = "끝나지 않은 항목(대기·진행·일시정지)이 있어 저장 폴더를 바꿀 수 없습니다.\n모두 끝나거나 취소한 뒤에 바꾸세요.";
            ui.horizontal(|ui| {
                ui.label("저장 폴더:");
                match self.folder.clone() {
                    Some(dir) => {
                        ui.label(egui::RichText::new(shorten(&dir.display().to_string(), 48)).color(tone(ui, Tone::Muted)))
                            .on_hover_text(format!("{}\n다음 다운로드도 이 폴더에 저장합니다", dir.display()));
                        if ui
                            .add_enabled(!unfinished, egui::Button::new("변경…"))
                            .on_disabled_hover_text(FOLDER_LOCKED)
                            .clicked()
                        {
                            if let Some(d) = pick_folder() {
                                self.folder = Some(d);
                            }
                        }
                        if ui.button("열기").clicked() {
                            let _ = std::process::Command::new("open").arg(&dir).spawn();
                        }
                    }
                    None => {
                        ui.label(egui::RichText::new("(다운로드를 누르면 고릅니다)").color(tone(ui, Tone::Muted)));
                        if ui
                            .add_enabled(!unfinished, egui::Button::new("선택…"))
                            .on_disabled_hover_text(FOLDER_LOCKED)
                            .clicked()
                        {
                            if let Some(d) = pick_folder() {
                                self.folder = Some(d);
                            }
                        }
                    }
                }
            });

            ui.add_space(8.0);
            ui.separator();

            let mut retry = false;
            let mut resume: Vec<usize> = Vec::new();
            {
                let mut items = self.items.lock().unwrap();
                let total = items.len();
                let count = |f: &dyn Fn(&Status) -> bool| items.iter().filter(|i| f(&i.status)).count();
                let finished = count(&|s| matches!(s, Status::Finished));
                let failed = count(&|s| matches!(s, Status::Failed(_)));
                let paused = count(&|s| matches!(s, Status::Paused { .. }));
                let cancelled = count(&|s| matches!(s, Status::Cancelled));
                let selected = items.iter().filter(|i| i.selected).count();
                if total > 0 {
                    let (text, color) = if running {
                        (format!("진행 중  {} / {}", finished + failed, total), tone(ui, Tone::Info))
                    } else if finished == total {
                        (format!("✅ 완료  {} / {}", finished, total), tone(ui, Tone::Ok))
                    } else {
                        let mut parts = vec![format!("완료 {finished}")];
                        if failed > 0 {
                            parts.push(format!("실패 {failed}"));
                        }
                        if paused > 0 {
                            parts.push(format!("일시정지 {paused}"));
                        }
                        if cancelled > 0 {
                            parts.push(format!("취소 {cancelled}"));
                        }
                        let color = if failed > 0 { tone(ui, Tone::Bad) } else { tone(ui, Tone::Warn) };
                        (format!("⚠ {}  (전체 {total})", parts.join(" · ")), color)
                    };
                    ui.horizontal(|ui| {
                        let mut all = selected == total;
                        if ui.add_enabled(!running, egui::Checkbox::without_text(&mut all)).changed() {
                            for i in items.iter_mut() {
                                i.selected = all;
                            }
                        }
                        ui.colored_label(color, egui::RichText::new(text).strong().size(16.0));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if (running || paused > 0)
                                && ui.button("모두 취소").on_hover_text("받던 파일과 일시정지한 파일을 지우고 멈춥니다").clicked()
                            {
                                cancel_all(&mut items);
                            }
                            if running {
                                if ui.button("모두 일시정지").on_hover_text("받은 만큼 남겨 두고 멈춥니다").clicked() {
                                    pause_all(&mut items);
                                }
                            } else {
                                if ui
                                    .add_enabled(selected > 0, egui::Button::new(format!("선택한 것만 다시 받기 ({selected}개)")))
                                    .clicked()
                                {
                                    retry = true;
                                }
                                if ui
                                    .add_enabled(failed + paused > 0, egui::Button::new(format!("실패·일시정지 다시 받기 ({}개)", failed + paused)))
                                    .clicked()
                                {
                                    for i in items.iter_mut() {
                                        i.selected = matches!(i.status, Status::Failed(_) | Status::Paused { .. });
                                    }
                                    retry = true;
                                }
                            }
                        });
                    });
                    ui.add_space(4.0);
                }
            }
            if retry {
                self.retry_selected();
            }

            let mut items = self.items.lock().unwrap();
            egui::ScrollArea::vertical().id_salt("list").show(ui, |ui| {
                for (idx, item) in items.iter_mut().enumerate() {
                    update_speed(item);
                    let (mark, color) = match &item.status {
                        Status::Waiting => ("⏳", tone(ui, Tone::Muted)),
                        Status::Downloading { .. } => ("⬇", tone(ui, Tone::Info)),
                        Status::Finished => ("✅", tone(ui, Tone::Ok)),
                        Status::Failed(_) => ("❌", tone(ui, Tone::Bad)),
                        Status::Paused { .. } => ("⏸", tone(ui, Tone::Warn)),
                        Status::Cancelled => ("✖", tone(ui, Tone::Off)),
                    };
                    ui.horizontal(|ui| {
                        ui.set_width(ui.available_width());
                        ui.add_enabled(!running, egui::Checkbox::without_text(&mut item.selected));
                        ui.label(
                            egui::RichText::new(format!("{:>3}", item.line))
                                .text_style(mono.clone())
                                .strong(),
                        );
                        ui.colored_label(color, mark);
                        if matches!(item.status, Status::Finished) {
                            // 완료된 파일은 눌러서 Finder 에서 보여준다
                            if ui.link(egui::RichText::new(&item.name).strong()).on_hover_text("Finder 에서 보기").clicked() {
                                let _ = std::process::Command::new("open").arg("-R").arg(item.dest()).spawn();
                            }
                        } else {
                            ui.label(egui::RichText::new(&item.name).strong());
                        }
                        ui.label(
                            egui::RichText::new(shorten(&item.url, 50))
                                .color(tone(ui, Tone::Muted))
                                .small(),
                        )
                        .on_hover_text(&item.url);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            match item.status {
                                Status::Waiting | Status::Downloading { .. } => {
                                    if ui.small_button("취소").on_hover_text("받던 파일을 지우고 멈춥니다").clicked() {
                                        cancel_item(item);
                                    }
                                    if ui.small_button("일시정지").on_hover_text("받은 만큼 남겨 두고 멈춥니다 (나중에 이어받기)").clicked() {
                                        pause_item(item);
                                    }
                                }
                                Status::Paused { resumable, .. } => {
                                    if ui.small_button("취소").on_hover_text("받던 파일을 지웁니다").clicked() {
                                        cancel_item(item);
                                    }
                                    let (label, tip) = if resumable {
                                        ("이어받기", "받은 곳부터 이어서 받습니다")
                                    } else {
                                        ("다시 받기", "이 서버는 이어받기를 지원하지 않아 처음부터 다시 받습니다")
                                    };
                                    if ui.small_button(label).on_hover_text(tip).clicked() {
                                        resume.push(idx);
                                    }
                                }
                                _ => {}
                            }
                        });
                    });
                    ui.horizontal(|ui| {
                        ui.add_space(54.0);
                        match &item.status {
                            Status::Downloading { done, total, parts } => {
                                let mut info = String::new();
                                if item.speed > 1.0 {
                                    info += &format!("  ·  {}/s", fmt_bytes(item.speed as u64));
                                    if let Some(t) = total {
                                        if *t > *done {
                                            info += &format!("  ·  남은 {}", fmt_secs(((*t - *done) as f32 / item.speed) as u64));
                                        }
                                    }
                                }
                                if *parts > 1 {
                                    info += &format!("  ·  {parts}분할");
                                }
                                let bar = match total {
                                    Some(t) if *t > 0 => egui::ProgressBar::new(*done as f32 / *t as f32)
                                        .text(format!("{} / {}{info}", fmt_bytes(*done), fmt_bytes(*t))),
                                    _ => egui::ProgressBar::new(0.0)
                                        .animate(true)
                                        .text(format!("{}{info}", fmt_bytes(*done))),
                                };
                                ui.add(bar);
                            }
                            Status::Finished => {
                                ui.add(egui::ProgressBar::new(1.0).fill(bar_fill(ui, Tone::Ok)).text("완료"));
                            }
                            Status::Failed(e) => {
                                ui.colored_label(tone(ui, Tone::Bad), e);
                            }
                            Status::Paused { done, total, resumable } => {
                                let (frac, text) = match total {
                                    _ if !*resumable => (0.0, "일시정지 (이어받기 불가, 처음부터 다시 받음)".to_string()),
                                    Some(t) if *t > 0 => (*done as f32 / *t as f32, format!("일시정지  {} / {}", fmt_bytes(*done), fmt_bytes(*t))),
                                    _ => (0.0, format!("일시정지  {}", fmt_bytes(*done))),
                                };
                                ui.add(egui::ProgressBar::new(frac).fill(bar_fill(ui, Tone::Warn)).text(text));
                            }
                            Status::Cancelled => {
                                ui.label(egui::RichText::new("취소됨").color(tone(ui, Tone::Off)));
                            }
                            Status::Waiting => {
                                ui.add(egui::ProgressBar::new(0.0).text("대기"));
                            }
                        }
                    });
                    ui.add_space(6.0);
                }
            });
            drop(items);
            if !resume.is_empty() {
                self.spawn_workers(&resume);
            }
            self.drop_overlay(ctx);
        });
    }
}

/// ~/Library/Application Support/URL Downloader/config.json
fn config_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join("Library/Application Support/URL Downloader/config.json"))
}

/// macOS 알림 센터에 띄운다 (osascript)
fn notify(title: &str, msg: &str) {
    let script = format!(
        "display notification \"{}\" with title \"{}\"",
        msg.replace('"', "'"),
        title.replace('"', "'")
    );
    let _ = std::process::Command::new("osascript").arg("-e").arg(script).spawn();
}

fn pick_folder() -> Option<PathBuf> {
    rfd::FileDialog::new().set_title("저장할 폴더 선택").pick_folder()
}

/// 0.5초마다 표본을 찍어 속도를 지수 이동 평균으로 갱신한다
fn update_speed(item: &mut Item) {
    let Status::Downloading { done, .. } = item.status else {
        item.sample = None;
        item.speed = 0.0;
        return;
    };
    let now = Instant::now();
    match item.sample {
        None => item.sample = Some((now, done)),
        Some((t, d)) => {
            let dt = now.duration_since(t).as_secs_f32();
            if dt >= 0.5 {
                let inst = done.saturating_sub(d) as f32 / dt;
                item.speed = if item.speed <= 0.0 { inst } else { item.speed * 0.7 + inst * 0.3 };
                item.sample = Some((now, done));
            }
        }
    }
}

fn fmt_secs(s: u64) -> String {
    if s >= 3600 {
        format!("{}시간 {}분", s / 3600, (s % 3600) / 60)
    } else if s >= 60 {
        format!("{}분 {}초", s / 60, s % 60)
    } else {
        format!("{s}초")
    }
}

/// 도움말 한 절
struct Section {
    title: &'static str,
    blocks: &'static [Block],
}

enum Block {
    Text(&'static str),
    Bullets(&'static [&'static str]),
    /// 번호가 붙는 순서 목록
    Steps(&'static [&'static str]),
    Code(&'static str),
}
use Block::{Bullets, Code, Steps, Text};

const HELP: &[Section] = &[
    Section {
        title: "시작하기",
        blocks: &[
            Text("URL 목록을 붙여넣고 저장할 폴더만 고르면 한꺼번에 내려받는 프로그램입니다. 큰 파일은 여러 연결로 나눠 받고, 끊기면 받은 곳부터 이어서 받습니다."),
            Steps(&[
                "위쪽 입력창에 링크를 한 줄에 하나씩 붙여넣습니다.",
                "필요하면 [동시 파일] / [파일당 분할] 숫자를 조정합니다.",
                "[다운로드 (N개)] 버튼을 누르고 저장할 폴더를 고릅니다.",
                "아래 목록에서 진행 상황을 확인합니다. 끝나면 [폴더 열기] 로 바로 열 수 있습니다.",
            ]),
            Text("버튼의 N 은 입력창에서 URL 로 인식된 줄 수입니다. http:// 또는 https:// 로 시작하지 않는 줄은 무시됩니다."),
            Bullets(&[
                "F1: 이 도움말 열기   ·   Esc: 다운로드 화면으로 돌아가기",
                "도움말·정보 화면을 보는 동안에도 다운로드는 뒤에서 계속 진행됩니다.",
                "왼쪽 검색창에 단어를 넣으면 그 단어가 들어간 절과 줄만 보여줍니다.",
            ]),
        ],
    },
    Section {
        title: "입력 형식",
        blocks: &[
            Text("한 줄에 항목 하나입니다. URL 만 쓰거나, 저장할 파일명을 함께 쓸 수 있습니다."),
            Code("https://example.com/files/photo.jpg\n여행 사진<Tab>https://example.com/files/photo.jpg\n여행 사진 | https://example.com/files/photo.jpg\nhttps://example.com/files/photo.jpg<Tab>여행 사진"),
            Bullets(&[
                "URL 만 있으면 URL 의 원본 파일명(photo.jpg)으로 저장합니다.",
                "파일명과 URL 사이 구분자는 탭, 공백, 세로줄(|), 쉼표(,) 중 아무거나 됩니다.",
                "파일명과 URL 의 순서는 바뀌어도 됩니다. http(s):// 가 있는 쪽을 URL 로 봅니다.",
                "파일명에는 확장자를 쓰지 마세요. 원본 URL 의 확장자(.jpg 등)가 자동으로 붙습니다.",
                "스프레드시트에서 '파일명, URL' 두 열을 복사해 붙이면 탭으로 구분돼 그대로 동작합니다.",
                "파일명에 쓸 수 없는 문자  / \\ : * ? \" < > |  는 밑줄(_)로 바뀝니다.",
                "빈 줄이나 URL 이 없는 줄은 건너뜁니다. 왼쪽 줄 번호가 목록의 번호와 같아서 어느 줄인지 찾기 쉽습니다.",
                "URL 목록이 든 텍스트 파일(.txt, .csv 등)을 창에 끌어다 놓으면 http:// 또는 https:// 가 들어간 줄만 입력창 끝에 추가됩니다. 5 MB 가 넘거나 텍스트가 아닌 파일은 무시합니다.",
            ]),
        ],
    },
    Section {
        title: "다운로드 설정",
        blocks: &[
            Bullets(&[
                "동시 파일 (기본 3): 한 번에 받는 파일 수입니다. 나머지는 '대기' 상태로 줄을 서 있다가 앞 파일이 끝나면 시작합니다.",
                "파일당 분할 (기본 4): 파일 하나를 몇 개 연결로 나눠 받을지입니다. 서버가 Range 요청을 지원하고 파일이 1 MB 이상일 때만 적용됩니다.",
                "실제로 열리는 연결 수는 두 값의 곱입니다(기본 3 × 4 = 12). 서버가 연결 수를 제한하거나 속도가 오히려 떨어지면 분할 수를 1~2 로 낮추세요.",
                "두 값은 다운로드가 진행 중일 때는 바꿀 수 없습니다. 다시 받기에도 현재 값이 적용됩니다.",
                "저장 폴더: 한 번 고르면 기억해 두고 다음부터는 [다운로드] 를 누르면 바로 그 폴더에 받습니다. 바꾸려면 [변경…] 을 누르세요.",
                "목록에 끝나지 않은 항목(대기·진행·일시정지)이 있으면 [변경…] 이 꺼집니다. 그 항목들은 처음 고른 폴더에 이어서 받기 때문입니다. 모두 끝나거나 취소하면 다시 켜집니다.",
                "기억한 폴더가 지금 없으면(외장 드라이브를 뺀 경우 등) [다운로드] 를 누를 때 폴더를 다시 고르게 합니다. 기억한 값은 지우지 않습니다.",
                "동시 파일·분할 수·저장 폴더·테마는 앱을 껐다 켜도 유지됩니다 (~/Library/Application Support/URL Downloader/config.json).",
            ]),
        ],
    },
    Section {
        title: "진행 상황 보기",
        blocks: &[
            Bullets(&[
                "⏳ 대기: 차례를 기다리는 중",
                "⬇ 진행: 받는 중. 진행바에 받은 크기 / 전체 크기, 현재 속도(MB/s), 남은 시간, 분할 중이면 'N분할' 이 표시됩니다. 속도는 0.5초마다 재고 평균을 내므로 처음 1초 정도는 비어 있습니다.",
                "✅ 완료: 최종 파일명으로 저장됨. 파일명을 누르면 Finder 에서 그 파일을 보여줍니다.",
                "❌ 실패: 사유가 빨간 글씨로 표시되고 체크박스가 자동으로 켜집니다.",
                "⏸ 일시정지: 사용자가 멈춘 상태. 받은 만큼 남아 있어 [이어받기] 로 이어받을 수 있습니다. Range 를 지원하지 않는 서버였다면 '일시정지 (이어받기 불가, 처음부터 다시 받음)' 으로 표시되고 버튼은 [다시 받기] 가 됩니다.",
                "✖ 취소됨: 사용자가 취소해 받던 파일을 지운 상태. 체크해서 [선택한 것만 다시 받기] 로 처음부터 다시 받을 수 있습니다.",
            ]),
            Text("목록 위의 요약 줄에는 전체 / 완료 / 실패 / 일시정지 / 취소 개수가 표시됩니다. 전체 크기를 알려주지 않는 서버는 진행바 대신 받은 크기만 움직입니다."),
            Text("대기·진행·일시정지 항목이 하나도 남지 않고 완료나 실패가 하나라도 있으면 macOS 알림이 뜹니다 (완료·실패 개수). 취소한 항목이 섞여 있어도 알리고, 일시정지한 항목이 남아 있으면 알리지 않습니다."),
        ],
    },
    Section {
        title: "일시정지 · 취소 · 다시 받기",
        blocks: &[
            Bullets(&[
                "각 항목 오른쪽의 [일시정지] 는 그 파일만 멈추고, [취소] 는 받던 파일을 지우고 멈춥니다. 대기 중인 항목도 누르는 즉시 일시정지·취소로 바뀝니다.",
                "요약 줄의 [모두 일시정지] 는 진행·대기 중인 전부를 멈춥니다. [모두 취소] 는 이미 일시정지한 항목까지 포함해 받던 파일을 모두 지웁니다.",
                "[이어받기] 를 여러 개 눌러도 '동시 파일' 수를 넘겨 받지 않습니다. 넘치는 항목은 대기했다가 차례가 되면 받습니다.",
                "일시정지는 받은 만큼을 .part/.meta 로 남겨 두므로 [이어받기] 를 누르거나 [실패·일시정지 다시 받기] 로 받은 곳부터 이어집니다. 취소는 받던 파일을 지웁니다.",
                "[실패·일시정지 다시 받기]: 실패했거나 일시정지된 항목을 모두 골라 다시 받습니다.",
                "[선택한 것만 다시 받기]: 체크박스를 켠 항목만 다시 받습니다. 맨 위 체크박스로 전체 선택/해제할 수 있습니다.",
                "다시 받기·이어받기는 저장 폴더를 그 뒤에 바꿨더라도 항목을 처음 받던 폴더에 저장합니다. 이어받기 기록이 있으면 받은 곳부터 이어서 받습니다(다음 절 참고).",
                "[지우기] 는 입력창과 목록을 비웁니다. 이미 받은 파일은 지우지 않습니다.",
            ]),
        ],
    },
    Section {
        title: "이어받기와 분할 다운로드",
        blocks: &[
            Text("서버가 HTTP Range 를 지원하면 파일을 구간으로 나눠 동시에 받고, 받은 만큼을 기록해 두었다가 끊겨도 이어서 받습니다."),
            Bullets(&[
                "받는 동안 저장 폴더에 '<파일명>.part'(본문)와 '<파일명>.meta'(구간별 진행 기록)가 생깁니다. 완료되면 .part 가 최종 이름으로 바뀌고 .meta 는 지워집니다.",
                "연결이 끊기면 구간별로 받은 지점부터 2초 간격으로 3회 자동 재시도합니다. 연결은 15초, 응답이 멈춘 채로는 30초가 지나면 끊긴 것으로 봅니다. 재시도를 기다리는 중에도 일시정지·취소는 바로 먹습니다.",
                "그래도 실패하면 .part 와 .meta 를 남겨 둡니다. [다시 받기] 를 누르거나, 앱을 껐다 켠 뒤 같은 URL 을 같은 폴더에 다시 받으면 받은 곳부터 이어집니다.",
                "서버의 파일 크기나 ETag 가 달라졌으면 안전하게 처음부터 다시 받습니다.",
                "진행 기록은 1초마다 저장되므로 강제 종료 시 최대 1초치는 다시 받습니다.",
                "Range 를 지원하지 않는 서버는 한 줄기로 처음부터 받습니다. 이 경우 분할과 이어받기는 되지 않습니다.",
                "1 MB 미만 파일은 분할하지 않습니다.",
            ]),
        ],
    },
    Section {
        title: "파일명 규칙",
        blocks: &[
            Bullets(&[
                "파일명을 지정하지 않으면 URL 경로의 마지막 부분을 씁니다. %20 같은 인코딩은 풀어서 저장합니다(photo%20one.jpg → photo one.jpg).",
                "URL 에 파일명이 없으면(예: https://example.com/) 'file_<줄번호>' 로 저장합니다.",
                "지정한 파일명 + 원본 확장자로 저장합니다. 원본 URL 에 확장자가 없으면 확장자 없이 저장됩니다.",
                "같은 이름의 파일이 폴더에 이미 있거나 목록 안에서 겹치면 '이름 (1).jpg', '이름 (2).jpg' 처럼 번호를 붙입니다. .tar.gz 같은 복합 확장자는 '이름 (1).tar.gz' 가 됩니다.",
                "받기 시작할 때 같은 이름의 파일(이 앱이 받지 않은 파일)이 이미 있으면 덮어쓰지 않고 바로 '같은 이름의 파일이 이미 있습니다' 로 실패합니다. 받는 동안 새로 생긴 경우에도 마지막에 같은 이유로 실패하고, 받은 내용은 .part 로 남습니다.",
                "✅ 완료된 항목을 체크해 [선택한 것만 다시 받기] 를 누르면 그 항목이 받았던 파일을 지우고 처음부터 다시 받습니다(덮어쓰기).",
            ]),
        ],
    },
    Section {
        title: "업데이트",
        blocks: &[
            Bullets(&[
                "앱을 켤 때 GitHub 릴리스를 확인해 새 버전이 있으면 제목 'URL Downloader (v현재버전)' 옆에 [⬆ 업데이트 vX.Y.Z] 버튼이 나타납니다.",
                "버튼을 누르면 새 버전을 받아 지금 실행 중인 .app 을 같은 자리에서 교체하고 자동으로 다시 시작합니다. 진행 중인 다운로드는 끊기므로 끝난 뒤에 누르세요.",
                "[정보 → URL Downloader 정보] 화면에서 [업데이트 확인] 으로 언제든 다시 확인할 수 있고, 켜 둔 채로 두면 1시간마다 자동으로 다시 확인합니다.",
                "cargo run 처럼 .app 번들이 아닌 상태로 실행 중이면 업데이트 버튼은 동작하지 않습니다.",
                "교체에 실패하면 기존 앱을 그대로 되돌리고 상단에 ⚠ 표시가 뜹니다. 마우스를 올리면 사유가 보입니다.",
            ]),
        ],
    },
    Section {
        title: "테마",
        blocks: &[
            Bullets(&[
                "메뉴 막대 오른쪽 끝 버튼으로 화면 테마를 바꿉니다. 버튼 글자는 지금 테마입니다.",
                "누를 때마다 ☀ 데이 → 🌙 나이트 → 🖥 시스템 순으로 바뀝니다. 시스템은 macOS 의 라이트/다크 설정을 따라갑니다.",
                "고른 테마는 기억해 두었다가 다음 실행 때 그대로 적용합니다 (처음 기본값은 시스템).",
            ]),
        ],
    },
    Section {
        title: "문제 해결",
        blocks: &[
            Bullets(&[
                "HTTP 403 / 404: 주소가 틀렸거나 로그인·쿠키가 필요한 링크입니다. 브라우저에서 직접 열리는 주소인지 확인하세요.",
                "HTTP 429 / 503: 서버가 요청을 제한하고 있습니다. 동시 파일 수와 분할 수를 줄이고 잠시 뒤 다시 받으세요.",
                "'HTTP 200 (Range 거부)': 서버가 처음엔 Range 를 받아 주다가 중간에 거부한 경우입니다. 분할 수를 1 로 놓고 다시 받으세요.",
                "'연결 끊김': 3회 재시도 후에도 끊긴 상태입니다. 네트워크를 확인하고 [실패·일시정지 다시 받기] 를 누르면 이어서 받습니다.",
                "폴더에 .part / .meta 파일이 남아 있다면 받다 만 흔적입니다. 다시 받을 계획이 없으면 지워도 됩니다.",
                "버그 제보나 제안은 [정보 → GitHub 저장소 열기] 의 Issues 에 남겨 주세요.",
            ]),
        ],
    },
];

impl App {
    fn notify_if_finished(&self, running: bool) {
        if !(self.was_running && !running) {
            return;
        }
        if let Some(msg) = finish_message(&self.items.lock().unwrap()) {
            notify("URL Downloader", &msg);
        }
    }

    fn hourly_update_check(&mut self) {
        if self.last_update_check.elapsed() < Duration::from_secs(3600) {
            return;
        }
        let busy = matches!(*self.update.lock().unwrap(), UpdateState::Downloading { .. } | UpdateState::Installing);
        if !busy {
            self.check_update();
        }
        self.last_update_check = Instant::now();
    }

    /// 창에 떨어뜨린 텍스트 파일(.txt/.csv 등)에서 URL 이 든 줄만 입력창에 덧붙인다
    fn accept_dropped_files(&mut self, ctx: &egui::Context) {
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect());
        for path in dropped {
            let Some(text) = url_lines_from_file(&path) else { continue };
            if !self.input.is_empty() && !self.input.ends_with('\n') {
                self.input.push('\n');
            }
            self.input.push_str(&text);
            self.input.push('\n');
            self.view = View::Download;
        }
    }

    fn drop_overlay(&self, ctx: &egui::Context) {
        if !ctx.input(|i| !i.raw.hovered_files.is_empty()) {
            return;
        }
        egui::Area::new(egui::Id::new("drop_overlay"))
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .order(egui::Order::Foreground)
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style())
                    .fill(egui::Color32::from_rgba_premultiplied(30, 90, 160, 230))
                    .inner_margin(24.0)
                    .show(ui, |ui| {
                        ui.label(egui::RichText::new("📄 여기에 놓으면 파일의 URL 목록을 입력창에 추가합니다").size(18.0).color(egui::Color32::WHITE));
                    });
            });
    }

    fn config_json(&self) -> String {
        serde_json::json!({
            "folder": self.folder.as_ref().map(|p| p.display().to_string()),
            "concurrent": self.concurrent,
            "segments": self.segments,
            "theme": self.theme.key(),
        })
        .to_string()
    }

    fn load_config(&mut self) {
        let Some(path) = config_path() else { return };
        let Ok(text) = std::fs::read_to_string(&path) else { return };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else { return };
        // 폴더가 지금 없어도(외장 드라이브 분리 등) 값은 버리지 않는다. 다운로드를 누를 때 없으면 다시 고르게 한다
        if let Some(f) = v["folder"].as_str().filter(|f| !f.is_empty()) {
            self.folder = Some(PathBuf::from(f));
        }
        if let Some(n) = v["concurrent"].as_u64() {
            self.concurrent = (n as usize).clamp(1, 16);
        }
        if let Some(n) = v["segments"].as_u64() {
            self.segments = (n as usize).clamp(1, 16);
        }
        if let Some(t) = v["theme"].as_str().and_then(Theme::from_key) {
            self.theme = t;
        }
        self.saved_config = self.config_json();
    }

    fn save_config_if_changed(&mut self) {
        let now = self.config_json();
        if now == self.saved_config {
            return;
        }
        if let Some(path) = config_path() {
            let _ = write_atomic(&path, &now);
        }
        self.saved_config = now;
    }

    fn debug_screenshot(&mut self, ctx: &egui::Context) {
        let Some((path, frames)) = self.screenshot.as_mut() else { return };
        *frames += 1;
        ctx.request_repaint();
        let at: u32 = std::env::var("URL_DOWNLOADER_SCREENSHOT_FRAMES").ok().and_then(|v| v.parse().ok()).unwrap_or(5);
        if *frames == at {
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot);
        }
        let shot = ctx.input(|i| {
            i.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(img) = shot {
            let [w, h] = img.size;
            let mut out = format!("P6\n{w} {h}\n255\n").into_bytes();
            for px in &img.pixels {
                out.extend_from_slice(&[px.r(), px.g(), px.b()]);
            }
            std::fs::write(path, out).expect("screenshot write");
            std::process::exit(0);
        }
    }

    fn back_bar(&mut self, ui: &mut egui::Ui, title: &str) {
        ui.horizontal(|ui| {
            if ui.button("← 다운로드 화면으로 (Esc)").clicked() {
                self.view = View::Download;
            }
            ui.add_space(8.0);
            ui.heading(title);
            if self.is_running() {
                ui.label(egui::RichText::new("· 다운로드는 뒤에서 계속 진행 중").color(tone(ui, Tone::Info)).small());
            }
        });
        ui.separator();
    }

    /// 도움말 화면: 왼쪽 목차 + 오른쪽 본문 (검색어가 있으면 맞는 줄만)
    fn help_view(&mut self, ui: &mut egui::Ui) {
        self.back_bar(ui, "도움말");
        let query = self.help_query.trim().to_lowercase();
        let hit = |t: &str| query.is_empty() || t.to_lowercase().contains(&query);

        egui::SidePanel::left("help_toc")
            .resizable(false)
            .exact_width(190.0)
            .show_inside(ui, |ui| {
                ui.add_space(6.0);
                ui.add(
                    egui::TextEdit::singleline(&mut self.help_query)
                        .hint_text("🔍 검색")
                        .desired_width(f32::INFINITY),
                );
                ui.add_space(8.0);
                ui.strong("목차");
                ui.add_space(2.0);
                for (i, sec) in HELP.iter().enumerate() {
                    if ui.link(format!("{}. {}", i + 1, sec.title)).clicked() {
                        self.help_query.clear();
                        self.help_jump = Some(i);
                    }
                }
            });

        egui::CentralPanel::default().show_inside(ui, |ui| {
            egui::ScrollArea::vertical().id_salt("help_body").show(ui, |ui| {
                let mut shown = 0;
                for (i, sec) in HELP.iter().enumerate() {
                    // 검색 중이면 절 제목이나 본문 어딘가가 맞는 절만 보여준다
                    let sec_hit = hit(sec.title)
                        || sec.blocks.iter().any(|b| match b {
                            Text(t) | Code(t) => hit(t),
                            Bullets(items) | Steps(items) => items.iter().any(|it| hit(it)),
                        });
                    if !sec_hit {
                        continue;
                    }
                    shown += 1;
                    let heading = ui.heading(format!("{}. {}", i + 1, sec.title));
                    if self.help_jump == Some(i) {
                        heading.scroll_to_me(Some(egui::Align::TOP));
                        self.help_jump = None;
                    }
                    ui.add_space(6.0);
                    for block in sec.blocks {
                        match block {
                            Text(t) => {
                                if hit(t) || hit(sec.title) {
                                    ui.label(*t);
                                }
                            }
                            Bullets(items) => {
                                for it in items.iter().filter(|it| hit(it) || hit(sec.title)) {
                                    ui.horizontal_wrapped(|ui| {
                                        ui.label("•");
                                        ui.label(*it);
                                    });
                                }
                            }
                            Steps(items) => {
                                for (n, it) in items.iter().enumerate().filter(|(_, it)| hit(it) || hit(sec.title)) {
                                    ui.horizontal_wrapped(|ui| {
                                        ui.label(egui::RichText::new(format!("{}.", n + 1)).strong());
                                        ui.label(*it);
                                    });
                                }
                            }
                            Code(c) => {
                                if hit(c) || hit(sec.title) {
                                    egui::Frame::group(ui.style()).show(ui, |ui| {
                                        ui.label(egui::RichText::new(*c).monospace());
                                    });
                                }
                            }
                        }
                        ui.add_space(6.0);
                    }
                    ui.add_space(16.0);
                }
                if shown == 0 {
                    ui.label(egui::RichText::new(format!("'{}' 에 맞는 내용이 없습니다", self.help_query)).color(tone(ui, Tone::Muted)));
                }
            });
        });
    }

    fn about_view(&mut self, ui: &mut egui::Ui) {
        self.back_bar(ui, "URL Downloader 정보");
        ui.add_space(24.0);
        ui.vertical_centered(|ui| {
            ui.heading(egui::RichText::new("URL Downloader").size(26.0));
            ui.label(format!("버전 {VERSION}"));
            ui.add_space(6.0);
            let state = self.update.lock().unwrap().clone();
            match state {
                UpdateState::Checking => {
                    ui.label(egui::RichText::new("업데이트 확인 중…").color(tone(ui, Tone::Muted)));
                }
                UpdateState::UpToDate => {
                    ui.label(egui::RichText::new("최신 버전입니다").color(tone(ui, Tone::Ok)));
                }
                UpdateState::Available { version, .. } => {
                    ui.label(egui::RichText::new(format!("새 버전 v{version} 이 있습니다. 다운로드 화면 제목 옆 버튼으로 업데이트하세요.")).color(tone(ui, Tone::Info)));
                }
                UpdateState::Error(e) => {
                    ui.label(egui::RichText::new(format!("업데이트 확인 실패: {e}")).color(tone(ui, Tone::Muted)).small());
                }
                UpdateState::Downloading { .. } | UpdateState::Installing => {
                    ui.label(egui::RichText::new("업데이트 진행 중…").color(tone(ui, Tone::Info)));
                }
            }
            if ui.button("업데이트 확인").clicked() {
                self.check_update();
            }
            ui.add_space(16.0);
            ui.label("URL 목록을 붙여넣고 폴더만 고르면 한꺼번에 내려받는 프로그램.");
            ui.label("분할 다운로드와 이어받기를 지원합니다.");
            ui.add_space(12.0);
            ui.hyperlink_to("GitHub: bluelang1/url-downloader", REPO_URL);
            ui.hyperlink_to("버그 제보 · 기능 제안 (Issues)", format!("{REPO_URL}/issues"));
            ui.add_space(12.0);
            ui.label(egui::RichText::new("MIT License  ·  © 2026 bluelang1").color(tone(ui, Tone::Muted)));
            ui.label(
                egui::RichText::new("Rust · egui/eframe · reqwest · rfd")
                    .color(tone(ui, Tone::Muted))
                    .small(),
            );
        });
    }

    fn is_running(&self) -> bool {
        self.items.lock().unwrap().iter().any(|i| {
            matches!(i.status, Status::Waiting | Status::Downloading { .. })
        })
    }

    fn start(&mut self, urls: Vec<Entry>, dir: PathBuf) {
        // 옛 목록의 큐 작업이 새 목록의 같은 번호를 가리키지 않도록 비운다
        self.queue.lock().unwrap().clear();
        let mut names: Vec<String> = Vec::new();
        let items: Vec<Item> = urls
            .iter()
            .map(|e| {
                let name = unique_name(&dir, &target_name(e), &e.url, &mut names);
                Item::new(e.line, e.url.clone(), name, dir.clone())
            })
            .collect();
        *self.items.lock().unwrap() = items;
        self.spawn_workers(&(0..urls.len()).collect::<Vec<_>>());
    }

    /// 선택된 항목만 각자 처음 폴더에 다시 받는다 (.meta 가 남아 있으면 이어받기)
    fn retry_selected(&mut self) {
        let idxs: Vec<usize> = {
            let mut items = self.items.lock().unwrap();
            let idxs: Vec<usize> = items
                .iter()
                .enumerate()
                .filter(|(_, i)| i.selected)
                .map(|(i, _)| i)
                .collect();
            for &i in &idxs {
                items[i].selected = false;
            }
            idxs
        };
        self.spawn_workers(&idxs);
    }

    /// 항목들을 큐에 넣고, 워커가 `concurrent` 개보다 적게 돌고 있으면 모자란 만큼만 더 띄운다
    fn spawn_workers(&self, idxs: &[usize]) {
        enqueue_jobs(&self.items, &self.queue, &self.active_workers, idxs, self.concurrent, self.segments);
    }
}

/// idxs 를 대기 상태로 큐에 넣는다. 항목마다 세대 번호를 올리므로 큐에 남아 있던 옛 작업은 워커가 건너뛴다.
/// 돌고 있는 워커가 `concurrent` 개보다 적으면 모자란 만큼(큐 길이 이내)만 새로 띄운다.
fn enqueue_jobs(shared: &Shared, queue: &Queue, active: &Arc<AtomicUsize>, idxs: &[usize], concurrent: usize, segments: usize) {
    // 큐 잠금을 쥔 채로 넣고 센다. 워커는 큐가 빈 것을 본 순간 같은 잠금 안에서 active 를 내리므로
    // "워커가 막 끝나려는데 새 작업이 들어와 아무도 안 받는" 경우가 없다.
    let mut q = queue.lock().unwrap();
    {
        let mut items = shared.lock().unwrap();
        for &i in idxs {
            let Some(item) = items.get_mut(i) else { continue };
            let gen = item.gen.fetch_add(1, Relaxed) + 1;
            // 완료 항목을 다시 받으면 그 항목이 만든 최종 파일을 지우고 처음부터 받는다(덮어쓰기).
            // 그 밖의 경우 dest 에 있는 파일은 남의 것이므로 download_file 이 손대지 않고 실패한다.
            if matches!(item.status, Status::Finished) {
                let _ = std::fs::remove_file(item.dest());
            }
            item.status = Status::Waiting;
            item.stop.store(STOP_NONE, Relaxed);
            q.push_back((i, gen));
        }
    }
    let want = concurrent.max(1).saturating_sub(active.load(Relaxed)).min(q.len());
    let segments = segments.max(1);
    for _ in 0..want {
        active.fetch_add(1, Relaxed);
        let (shared, queue, active) = (Arc::clone(shared), Arc::clone(queue), Arc::clone(active));
        thread::spawn(move || worker(shared, queue, active, segments));
    }
}

/// 워커 수(active)를 한 번만 내리는 가드. 정상 종료는 큐 잠금 안에서 `release` 로 내리고,
/// 워커가 panic 으로 풀리면 Drop 이 대신 내려 한도가 영영 차 있는 일이 없게 한다
struct ActiveGuard {
    active: Arc<AtomicUsize>,
    armed: bool,
}

impl ActiveGuard {
    fn release(&mut self) {
        if std::mem::replace(&mut self.armed, false) {
            self.active.fetch_sub(1, Relaxed);
        }
    }
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.release();
    }
}

/// 큐에서 하나씩 꺼내 받는다. 큐가 비면 active 를 내리고 끝난다
fn worker(shared: Shared, queue: Queue, active: Arc<AtomicUsize>, segments: usize) {
    let mut guard = ActiveGuard { active, armed: true };
    loop {
        let next = {
            let mut q = queue.lock().unwrap();
            let next = q.pop_front();
            if next.is_none() {
                guard.release();
            }
            next
        };
        let Some((idx, gen)) = next else { return };
        if claim(&shared, idx, gen) {
            download(Arc::clone(&shared), idx, segments);
        }
    }
}

/// 큐에서 꺼낸 작업이 아직 그 세대의 대기 상태일 때만 '받는 중'으로 바꾸고 true.
/// 대기 중에 일시정지·취소됐거나 다시 큐에 들어간(세대가 바뀐) 작업은 건너뛴다.
fn claim(shared: &Shared, idx: usize, gen: u64) -> bool {
    let mut items = shared.lock().unwrap();
    match items.get_mut(idx) {
        Some(it) if it.gen.load(Relaxed) == gen && matches!(it.status, Status::Waiting) => {
            it.status = Status::Downloading { done: 0, total: None, parts: 1 };
            true
        }
        _ => false,
    }
}

/// 받다 만 파일(.part/.meta)을 지운다
fn remove_partial(dest: &Path) {
    let _ = std::fs::remove_file(sidecar(dest, "part"));
    let _ = std::fs::remove_file(sidecar(dest, "meta"));
}

/// 디스크에 남은 .part/.meta 로 일시정지 상태를 만든다 (돌고 있는 스레드가 없는 항목용)
fn paused_from_disk(dest: &Path, url: &str) -> Status {
    match load_meta(&sidecar(dest, "meta"), url) {
        Some(m) => Status::Paused {
            done: m.segs.iter().map(|(s, e, d)| (*d).min(e - s)).sum(),
            total: Some(m.total),
            resumable: true,
        },
        // .part 만 있고 진행 기록이 없으면 이어받을 수 없다. 둘 다 없으면 처음부터 받으면 되므로 잃을 것이 없다
        None => Status::Paused { done: 0, total: None, resumable: !sidecar(dest, "part").exists() },
    }
}

/// 항목 하나 일시정지. 받는 중이면 스레드에 신호를 보내고, 대기 중이면 바로 일시정지로 바꾼다
/// 이미 들어온 신호(취소)는 덮어쓰지 않는다. 취소는 일시정지보다 강해서 cancel_item 은 그냥 덮어쓴다
fn pause_item(item: &mut Item) {
    match item.status {
        Status::Downloading { .. } => {
            let _ = item.stop.compare_exchange(STOP_NONE, STOP_PAUSE, Relaxed, Relaxed);
        }
        Status::Waiting => {
            let _ = item.stop.compare_exchange(STOP_NONE, STOP_PAUSE, Relaxed, Relaxed);
            item.status = paused_from_disk(&item.dest(), &item.url);
        }
        _ => {}
    }
}

/// 항목 하나 취소. 받는 중이면 스레드에 신호를 보내고(스레드가 파일을 지움),
/// 대기·일시정지 중이면 돌고 있는 스레드가 없으므로 여기서 바로 파일을 지우고 취소로 바꾼다
fn cancel_item(item: &mut Item) {
    match item.status {
        Status::Downloading { .. } => item.stop.store(STOP_CANCEL, Relaxed),
        Status::Waiting | Status::Paused { .. } => {
            item.stop.store(STOP_CANCEL, Relaxed);
            remove_partial(&item.dest());
            item.status = Status::Cancelled;
        }
        _ => {}
    }
}

fn pause_all(items: &mut [Item]) {
    items.iter_mut().for_each(pause_item);
}

fn cancel_all(items: &mut [Item]) {
    items.iter_mut().for_each(cancel_item);
}

/// 완료 알림 문구. 대기·진행·일시정지가 하나도 없고 완료+실패가 1개 이상일 때만 (취소가 섞여 있어도)
fn finish_message(items: &[Item]) -> Option<String> {
    if items.iter().any(Item::is_unfinished) {
        return None;
    }
    let finished = items.iter().filter(|i| matches!(i.status, Status::Finished)).count();
    let failed = items.iter().filter(|i| matches!(i.status, Status::Failed(_))).count();
    match (finished, failed) {
        (0, 0) => None,
        (f, 0) => Some(format!("{f}개 파일 다운로드 완료")),
        (f, e) => Some(format!("완료 {f}개, 실패 {e}개")),
    }
}

/// 드롭 파일 한도
const DROP_MAX_BYTES: u64 = 5 * 1024 * 1024;

/// 드롭한 파일에서 http(s):// 가 든 줄만 뽑는다. 5MB 초과·NUL 바이트(바이너리)·UTF-8 아님·URL 줄 없음이면 None
fn url_lines_from_file(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > DROP_MAX_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    if bytes.contains(&0) {
        return None;
    }
    let text = String::from_utf8(bytes).ok()?;
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim_end)
        .filter(|l| l.contains("http://") || l.contains("https://"))
        .collect();
    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    }
}

/// 같은 폴더의 임시 파일에 쓴 뒤 rename 으로 바꿔 넣는다 (쓰는 도중 꺼져도 기존 파일이 깨지지 않음)
fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = sidecar(path, "tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

impl App {
    /// 타이틀 옆: 업데이트 버튼/진행 상태 (새 버전이 없으면 아무것도 안 그림)
    fn update_bar(&mut self, ui: &mut egui::Ui) {
        let state = self.update.lock().unwrap().clone();
        match state {
            UpdateState::Available { version, url } => {
                let btn = egui::Button::new(
                    egui::RichText::new(format!("⬆ 업데이트 v{version}")).color(egui::Color32::WHITE),
                )
                .fill(egui::Color32::from_rgb(40, 120, 200));
                if ui.add(btn).on_hover_text("새 버전을 받아 설치하고 다시 시작합니다").clicked() {
                    self.apply_update(url);
                }
            }
            UpdateState::Downloading { done, total } => {
                let text = match total {
                    Some(t) if t > 0 => format!("업데이트 받는 중 {} / {}", fmt_bytes(done), fmt_bytes(t)),
                    _ => format!("업데이트 받는 중 {}", fmt_bytes(done)),
                };
                ui.add(egui::ProgressBar::new(total.map(|t| done as f32 / t.max(1) as f32).unwrap_or(0.0)).desired_width(220.0).text(text));
                ui.ctx().request_repaint_after(Duration::from_millis(100));
            }
            UpdateState::Installing => {
                ui.spinner();
                ui.label("설치 중…");
                ui.ctx().request_repaint_after(Duration::from_millis(100));
            }
            UpdateState::Error(e) => {
                ui.label(egui::RichText::new("⚠").color(tone(ui, Tone::Bad))).on_hover_text(e);
            }
            UpdateState::Checking | UpdateState::UpToDate => {}
        }
    }

    /// GitHub 최신 릴리스를 조회해 현재 버전보다 새 것이면 Available 로 바꾼다.
    /// 이미 Available 이면 확인하는 동안에도 버튼을 유지하고, 확인이 실패해도 그대로 둔다.
    fn check_update(&self) {
        let state = Arc::clone(&self.update);
        {
            let mut s = state.lock().unwrap();
            match *s {
                UpdateState::Available { .. } | UpdateState::Downloading { .. } | UpdateState::Installing => {}
                _ => *s = UpdateState::Checking,
            }
        }
        thread::spawn(move || {
            let result = fetch_latest_release();
            let mut s = state.lock().unwrap();
            if let Some(next) = next_update_state(&s, result) {
                *s = next;
            }
        });
    }

    /// 새 버전 zip 을 받아 현재 .app 번들을 교체하고 다시 시작한다
    fn apply_update(&self, url: String) {
        let state = Arc::clone(&self.update);
        thread::spawn(move || {
            let set = |s: UpdateState| *state.lock().unwrap() = s;
            match install_update(&url, &set) {
                Ok(bundle) => {
                    let _ = std::process::Command::new("open").arg("-n").arg(&bundle).spawn();
                    thread::sleep(Duration::from_millis(500));
                    std::process::exit(0);
                }
                Err(e) => set(UpdateState::Error(format!("업데이트 실패: {e}"))),
            }
        });
    }
}

/// 업데이트 확인 결과를 반영한 새 상태. 바꿀 필요가 없으면 None
/// - 설치가 진행 중이면 건드리지 않는다
/// - 이미 Available 인데 확인이 실패하면 기존 버튼을 유지한다
fn next_update_state(prev: &UpdateState, result: Result<Option<(String, String)>, String>) -> Option<UpdateState> {
    if matches!(prev, UpdateState::Downloading { .. } | UpdateState::Installing) {
        return None;
    }
    let next = match result {
        Ok(Some((version, url))) => UpdateState::Available { version, url },
        Ok(None) => UpdateState::UpToDate,
        Err(_) if matches!(prev, UpdateState::Available { .. }) => return None,
        Err(e) => UpdateState::Error(e),
    };
    let same = match (prev, &next) {
        (UpdateState::Available { version: a, url: u }, UpdateState::Available { version: b, url: v }) => a == b && u == v,
        (UpdateState::UpToDate, UpdateState::UpToDate) => true,
        _ => false,
    };
    if same {
        None
    } else {
        Some(next)
    }
}

fn download(shared: Shared, idx: usize, segments: usize) {
    let (url, dest, stop) = {
        let items = shared.lock().unwrap();
        (items[idx].url.clone(), items[idx].dest(), Arc::clone(&items[idx].stop))
    };
    let set = |s: Status| shared.lock().unwrap()[idx].status = s;

    // 큐에서 기다리는 동안 멈춤 신호가 왔으면 시작하지 않는다
    let result = match stop.load(Relaxed) {
        STOP_NONE => download_file(&url, &dest, segments, &stop, &set),
        _ => Err(STOPPED.to_string()),
    };
    match result {
        Ok(()) => set(Status::Finished),
        Err(e) if e == STOPPED => {
            let mut items = shared.lock().unwrap();
            let (done, total) = match items[idx].status {
                Status::Downloading { done, total, .. } => (done, total),
                _ => (0, None),
            };
            if stop.load(Relaxed) == STOP_CANCEL {
                remove_partial(&dest);
                items[idx].status = Status::Cancelled;
            } else if done == 0 {
                items[idx].status = paused_from_disk(&dest, &url);
            } else {
                // .meta 가 없으면 Range 미지원 경로로 받던 중이라 이어받을 수 없다
                let resumable = sidecar(&dest, "meta").exists();
                items[idx].status = Status::Paused { done, total, resumable };
            }
        }
        Err(e) => {
            let mut items = shared.lock().unwrap();
            items[idx].status = Status::Failed(e);
            items[idx].selected = true;
        }
    }
}

/// `dest` 옆에 `<dest>.part`(받는 중인 본문)와 `<dest>.meta`(구간별 진행 상황)를 두고 받는다.
/// 서버가 Range 를 지원하면 분할·이어받기, 아니면 처음부터 한 줄기로 받는다.
fn download_file(url: &str, dest: &Path, segments: usize, stop: &AtomicU8, set: &(dyn Fn(Status) + Sync)) -> Result<(), String> {
    // 이 앱이 받지 않은 같은 이름 파일이 있으면 받기 전에 바로 실패한다 (완료 항목 다시 받기는 enqueue_jobs 가 먼저 지운다)
    if dest.exists() {
        return Err(EXISTS.to_string());
    }
    // blocking Client 에는 read_timeout 이 없다. blocking 의 timeout 은 응답 헤더 대기와
    // 본문 read 한 번마다 따로 적용되므로(전체 다운로드 시간 제한이 아님) 사실상 읽기 타임아웃이다.
    // 짧을수록 연결이 멈췄을 때 일시정지·취소가 빨리 먹는다. 느린 서버는 구간별 재시도(RETRIES)가 견딘다.
    let client = Client::builder()
        .user_agent("Mozilla/5.0")
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?;
    let part = sidecar(dest, "part");
    let meta = sidecar(dest, "meta");

    // 1바이트만 요청해 Range 지원 여부와 전체 크기를 알아낸다
    let probe = client
        .get(url)
        .header(RANGE, "bytes=0-0")
        .send()
        .map_err(|e| e.to_string())?;
    if !probe.status().is_success() {
        return Err(format!("HTTP {}", probe.status()));
    }

    if probe.status() == StatusCode::PARTIAL_CONTENT {
        let total = header(&probe, CONTENT_RANGE)
            .rsplit('/')
            .next()
            .and_then(|t| t.parse::<u64>().ok())
            .filter(|t| *t > 0);
        let etag = header(&probe, ETAG);
        drop(probe);
        match total {
            Some(total) => download_ranged(&client, url, &part, &meta, total, &etag, segments, stop, set)?,
            None => {
                // 206 인데 전체 크기를 안 알려주는 서버: 일반 GET 으로 받는다
                let resp = client.get(url).send().map_err(|e| e.to_string())?;
                if !resp.status().is_success() {
                    return Err(format!("HTTP {}", resp.status()));
                }
                // 이어받을 수 없는 경로: 옛 진행 기록이 남아 '이어받기 가능'으로 보이지 않게 지운다
                let _ = std::fs::remove_file(&meta);
                stream_whole(resp, &part, stop, set)?;
            }
        }
    } else {
        // Range 무시(200): 이 응답이 곧 전체 본문
        let _ = std::fs::remove_file(&meta);
        stream_whole(probe, &part, stop, set)?;
    }
    // 안전망: 받는 동안 같은 이름 파일이 생겼으면(다른 프로그램 등) 덮어쓰지 않는다
    if dest.exists() {
        return Err(EXISTS.to_string());
    }
    std::fs::rename(&part, dest).map_err(|e| e.to_string())
}

fn sidecar(dest: &Path, ext: &str) -> PathBuf {
    let name = dest.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    dest.with_file_name(format!("{name}.{ext}"))
}

fn header(resp: &Response, key: HeaderName) -> String {
    resp.headers()
        .get(key)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

/// Range 미지원: 응답 본문을 처음부터 끝까지 한 줄기로 받는다 (이어받기 불가)
fn stream_whole(mut resp: Response, part: &Path, stop: &AtomicU8, set: &(dyn Fn(Status) + Sync)) -> Result<(), String> {
    let total = resp.content_length();
    set(Status::Downloading { done: 0, total, parts: 1 });
    let mut file = File::create(part).map_err(|e| e.to_string())?;
    let mut buf = [0u8; BUF];
    let mut done = 0u64;
    loop {
        let n = resp.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        done += n as u64;
        set(Status::Downloading { done, total, parts: 1 });
        if stop.load(Relaxed) != STOP_NONE {
            return Err(STOPPED.to_string());
        }
    }
    Ok(())
}

/// 받을 구간 [start, end) 와 그 안에서 받은 바이트 수
struct Seg {
    start: u64,
    end: u64,
    done: AtomicU64,
}

struct Meta {
    total: u64,
    etag: String,
    segs: Vec<(u64, u64, u64)>,
}

#[allow(clippy::too_many_arguments)]
fn download_ranged(
    client: &Client,
    url: &str,
    part: &Path,
    meta_path: &Path,
    total: u64,
    etag: &str,
    segments: usize,
    stop: &AtomicU8,
    set: &(dyn Fn(Status) + Sync),
) -> Result<(), String> {
    // 같은 파일(URL·크기·ETag 일치)의 진행 기록이 남아 있으면 거기서부터 이어받는다
    let resumed = load_meta(meta_path, url).filter(|m| m.total == total && m.etag == etag && part.exists());
    let is_resume = resumed.is_some();
    let segs: Vec<Seg> = match resumed {
        Some(m) => m
            .segs
            .into_iter()
            .map(|(start, end, done)| Seg { start, end, done: AtomicU64::new(done.min(end - start)) })
            .collect(),
        None => {
            let n = (segments as u64).clamp(1, (total / MIN_CHUNK).max(1));
            let chunk = total.div_ceil(n);
            (0..n)
                .map(|i| Seg { start: i * chunk, end: ((i + 1) * chunk).min(total), done: AtomicU64::new(0) })
                .filter(|s| s.start < s.end)
                .collect()
        }
    };
    let file = if is_resume {
        OpenOptions::new().write(true).open(part).map_err(|e| e.to_string())?
    } else {
        let f = File::create(part).map_err(|e| e.to_string())?;
        f.set_len(total).map_err(|e| e.to_string())?;
        f
    };

    let parts = segs.len();
    let progress = || {
        let done = segs.iter().map(|s| s.done.load(Relaxed)).sum();
        set(Status::Downloading { done, total: Some(total), parts });
    };
    let save = || save_meta(meta_path, total, etag, url, &segs);
    let last_save = Mutex::new(Instant::now());
    let tick = || {
        progress();
        let mut last = last_save.lock().unwrap();
        if last.elapsed() > Duration::from_secs(1) {
            *last = Instant::now();
            let _ = save();
        }
    };
    progress();
    save()?;

    let (file, tick) = (&file, &tick);
    let errors: Vec<String> = thread::scope(|sc| {
        let handles: Vec<_> = segs
            .iter()
            .map(|seg| sc.spawn(move || fetch_segment(client, url, file, seg, stop, tick)))
            .collect();
        handles
            .into_iter()
            .filter_map(|h| h.join().unwrap_or_else(|_| Err("스레드 오류".into())).err())
            .collect()
    });
    if !errors.is_empty() {
        let _ = save();
        // 멈춤 신호가 원인이면 그걸 우선 보고한다 (다른 구간의 '연결 끊김' 보다)
        let e = errors.iter().find(|e| *e == STOPPED).cloned().unwrap_or_else(|| errors[0].clone());
        return Err(e);
    }
    let _ = std::fs::remove_file(meta_path);
    Ok(())
}

/// 구간 하나를 받는다. 끊기면 받은 곳부터 다시 요청한다
fn fetch_segment(client: &Client, url: &str, file: &File, seg: &Seg, stop: &AtomicU8, tick: &(dyn Fn() + Sync)) -> Result<(), String> {
    let len = seg.end - seg.start;
    let mut last_err = String::new();
    for attempt in 0..RETRIES {
        if seg.done.load(Relaxed) >= len {
            return Ok(());
        }
        if stop.load(Relaxed) != STOP_NONE {
            return Err(STOPPED.to_string());
        }
        if attempt > 0 {
            // 2초 쉬되 100ms 마다 멈춤 신호를 본다
            for _ in 0..20 {
                if stop.load(Relaxed) != STOP_NONE {
                    return Err(STOPPED.to_string());
                }
                thread::sleep(Duration::from_millis(100));
            }
        }
        match fetch_once(client, url, file, seg, stop, tick) {
            Ok(()) => return Ok(()),
            Err(e) if e == STOPPED => return Err(e),
            Err(e) => last_err = e,
        }
    }
    Err(last_err)
}

fn fetch_once(client: &Client, url: &str, file: &File, seg: &Seg, stop: &AtomicU8, tick: &(dyn Fn() + Sync)) -> Result<(), String> {
    let len = seg.end - seg.start;
    let mut done = seg.done.load(Relaxed);
    let mut resp = client
        .get(url)
        .header(RANGE, format!("bytes={}-{}", seg.start + done, seg.end - 1))
        .send()
        .map_err(|e| e.to_string())?;
    if resp.status() != StatusCode::PARTIAL_CONTENT {
        return Err(format!("HTTP {} (Range 거부)", resp.status()));
    }
    let mut buf = [0u8; BUF];
    loop {
        let n = resp.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        // 서버가 구간보다 더 보내도 넘어서 쓰지 않는다
        let n = (n as u64).min(len - done) as usize;
        file.write_all_at(&buf[..n], seg.start + done).map_err(|e| e.to_string())?;
        done += n as u64;
        seg.done.store(done, Relaxed);
        tick();
        if done >= len {
            break;
        }
        if stop.load(Relaxed) != STOP_NONE {
            return Err(STOPPED.to_string());
        }
    }
    if done < len {
        return Err(format!("연결 끊김 ({} / {})", fmt_bytes(done), fmt_bytes(len)));
    }
    Ok(())
}

/// .meta 형식: 전체 크기 / ETag / URL / 구간마다 "시작 끝 받은수" 한 줄
fn save_meta(path: &Path, total: u64, etag: &str, url: &str, segs: &[Seg]) -> Result<(), String> {
    let mut s = format!("{total}\n{etag}\n{url}\n");
    for seg in segs {
        s += &format!("{} {} {}\n", seg.start, seg.end, seg.done.load(Relaxed));
    }
    let tmp = path.with_extension("meta.tmp");
    std::fs::write(&tmp, s)
        .and_then(|_| std::fs::rename(&tmp, path))
        .map_err(|e| e.to_string())
}

/// .meta 를 읽는다. 다른 URL 의 진행 기록이면 None (이어받지 않고 처음부터 받게 한다).
/// URL 줄이 없는 옛 형식(셋째 줄부터 바로 구간)도 읽는다. 이때는 URL 을 확인할 수 없어 크기·ETag 로만 판단한다.
fn load_meta(path: &Path, url: &str) -> Option<Meta> {
    let s = std::fs::read_to_string(path).ok()?;
    let mut lines = s.lines().peekable();
    let total = lines.next()?.trim().parse().ok()?;
    let etag = lines.next()?.trim().to_string();
    let is_seg = |l: &str| l.split_whitespace().map(|x| x.parse::<u64>().ok()).collect::<Option<Vec<_>>>().is_some_and(|v| v.len() == 3);
    let saved_url = match lines.peek() {
        Some(l) if !is_seg(l) => Some(lines.next()?.trim().to_string()),
        _ => None,
    };
    if saved_url.as_deref().is_some_and(|u| u != url) {
        return None;
    }
    let mut segs = Vec::new();
    for l in lines {
        let v: Vec<u64> = l.split_whitespace().map(|x| x.parse().ok()).collect::<Option<_>>()?;
        if v.len() != 3 || v[0] >= v[1] {
            return None;
        }
        segs.push((v[0], v[1], v[2]));
    }
    if segs.is_empty() {
        None
    } else {
        Some(Meta { total, etag, segs })
    }
}

/// (새 버전, zip 다운로드 URL). 현재 버전 이하이면 None
fn fetch_latest_release() -> Result<Option<(String, String)>, String> {
    let client = Client::builder()
        .user_agent(format!("url-downloader/{VERSION}"))
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .get(
            std::env::var("URL_DOWNLOADER_UPDATE_API")
                .unwrap_or_else(|_| format!("https://api.github.com/repos/{REPO}/releases/latest")),
        )
        .header("Accept", "application/vnd.github+json")
        .send()
        .map_err(|e| e.to_string())?;
    if resp.status() == StatusCode::NOT_FOUND {
        return Ok(None); // 아직 릴리스가 없음
    }
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    let body = resp.text().map_err(|e| e.to_string())?;
    let json: serde_json::Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
    let tag = json["tag_name"].as_str().unwrap_or("");
    let latest = tag.trim_start_matches('v').to_string();
    if parse_version(&latest) <= parse_version(VERSION) {
        return Ok(None);
    }
    let url = json["assets"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|a| a["name"].as_str().is_some_and(|n| n.ends_with(".zip")))
        .find_map(|a| a["browser_download_url"].as_str().map(str::to_string))
        .ok_or_else(|| format!("v{latest} 릴리스에 zip 파일이 없습니다"))?;
    Ok(Some((latest, url)))
}

fn parse_version(v: &str) -> (u64, u64, u64) {
    let mut it = v.split(['.', '-', '+']).map(|p| p.parse::<u64>().unwrap_or(0));
    (it.next().unwrap_or(0), it.next().unwrap_or(0), it.next().unwrap_or(0))
}

/// 실행 중인 바이너리가 들어 있는 .app 번들 경로
fn current_bundle() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    // <Bundle>.app/Contents/MacOS/<exe>
    exe.ancestors()
        .nth(3)
        .filter(|p| p.extension().is_some_and(|e| e == "app"))
        .map(Path::to_path_buf)
        .ok_or_else(|| "앱 번들(.app)로 실행 중일 때만 업데이트할 수 있습니다".to_string())
}

/// zip 을 받아 풀고 현재 번들을 새 번들로 바꾼다. 성공하면 새 번들 경로를 돌려준다
fn install_update(url: &str, set: &dyn Fn(UpdateState)) -> Result<PathBuf, String> {
    let bundle = current_bundle()?;
    let work = std::env::temp_dir().join(format!("url-downloader-update-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).map_err(|e| e.to_string())?;
    let zip = work.join("update.zip");

    set(UpdateState::Downloading { done: 0, total: None });
    let client = Client::builder().user_agent(format!("url-downloader/{VERSION}")).build().map_err(|e| e.to_string())?;
    let mut resp = client.get(url).send().map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    let total = resp.content_length();
    let mut file = File::create(&zip).map_err(|e| e.to_string())?;
    let mut buf = [0u8; BUF];
    let mut done = 0u64;
    loop {
        let n = resp.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        done += n as u64;
        set(UpdateState::Downloading { done, total });
    }
    drop(file);

    set(UpdateState::Installing);
    let extracted = work.join("extracted");
    run("ditto", &["-x", "-k", &zip.to_string_lossy(), &extracted.to_string_lossy()])?;
    let new_app = std::fs::read_dir(&extracted)
        .map_err(|e| e.to_string())?
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|e| e == "app"))
        .ok_or("zip 안에 .app 이 없습니다")?;

    // 기존 번들을 옆으로 치우고 새 번들을 같은 자리에 복사한 뒤 치운 것을 지운다
    let backup = bundle.with_extension("app.old");
    let _ = std::fs::remove_dir_all(&backup);
    std::fs::rename(&bundle, &backup).map_err(|e| format!("기존 앱을 옮기지 못했습니다: {e}"))?;
    if let Err(e) = run("ditto", &[&new_app.to_string_lossy(), &bundle.to_string_lossy()]) {
        let _ = std::fs::rename(&backup, &bundle); // 실패하면 원복
        return Err(e);
    }
    let _ = std::fs::remove_dir_all(&backup);
    let _ = std::fs::remove_dir_all(&work);
    Ok(bundle)
}

fn run(cmd: &str, args: &[&str]) -> Result<(), String> {
    let out = std::process::Command::new(cmd).args(args).output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!("{cmd} 실패: {}", String::from_utf8_lossy(&out.stderr).trim()))
    }
}

/// 입력 한 줄에서 뽑아낸 항목
#[derive(Debug, PartialEq, Clone)]
struct Entry {
    /// 입력창 줄 번호(1부터)
    line: usize,
    url: String,
    /// 사용자가 지정한 파일명(확장자 없음). 없으면 URL 의 원본 파일명 사용
    name: Option<String>,
}

/// 한 줄 형식:  `URL`  |  `이름<Tab>URL`  |  `이름 | URL`  |  `URL<Tab>이름`
/// URL 은 공백·탭·`|` 를 포함하지 않으므로 http(s):// 위치로 이름과 URL 을 나눈다.
fn parse_urls(input: &str) -> Vec<Entry> {
    input
        .lines()
        .enumerate()
        .filter_map(|(i, l)| parse_line(l).map(|(url, name)| Entry { line: i + 1, url, name }))
        .collect()
}

fn parse_line(line: &str) -> Option<(String, Option<String>)> {
    let line = line.trim();
    let pos = line.find("https://").or_else(|| line.find("http://"))?;
    let is_sep = |c: char| c.is_whitespace() || c == '|' || c == ',';
    let before = line[..pos].trim_matches(is_sep);
    // 구분자 없이 이름이 URL 에 바로 붙은 경우("abchttps://")는 URL 로 보지 않는다
    if pos > 0 && !line[..pos].ends_with(is_sep) {
        return None;
    }
    let rest = &line[pos..];
    let url_end = rest.find(is_sep).unwrap_or(rest.len());
    let url = rest[..url_end].to_string();
    let after = rest[url_end..].trim_matches(is_sep);
    let name = if !before.is_empty() { before } else { after };
    let name = sanitize(name);
    Some((url, if name.is_empty() { None } else { Some(name) }))
}

/// 저장 파일명: 사용자 이름 + URL 원본 확장자, 또는 URL 원본 파일명
fn target_name(e: &Entry) -> String {
    let original = file_name_from_url(&e.url, e.line);
    match &e.name {
        Some(n) => format!("{n}{}", extension_of(&original)),
        None => original,
    }
}

/// ".tar.gz" 같은 복합 확장자를 포함한 확장자 (점 포함). 없으면 빈 문자열
fn extension_of(name: &str) -> &str {
    let lower = name.to_ascii_lowercase();
    for double in [".tar.gz", ".tar.xz", ".tar.bz2", ".tar.zst", ".tar.lz"] {
        if lower.ends_with(double) && name.len() > double.len() {
            return &name[name.len() - double.len()..];
        }
    }
    match name.rfind('.') {
        Some(p) if p > 0 => &name[p..],
        _ => "",
    }
}

fn shorten(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max / 2 - 1).collect();
        let tail: String = s.chars().skip(n - (max / 2 - 1)).collect();
        format!("{head}…{tail}")
    }
}

fn file_name_from_url(url: &str, idx: usize) -> String {
    let path = url.split(['?', '#']).next().unwrap_or("");
    let last = path.rsplit('/').next().unwrap_or("");
    let decoded = percent_encoding::percent_decode_str(last)
        .decode_utf8_lossy()
        .to_string();
    let cleaned = sanitize(&decoded);
    if cleaned.is_empty() {
        format!("file_{idx}")
    } else {
        cleaned
    }
}

pub(crate) fn sanitize(name: &str) -> String {
    name.trim()
        .chars()
        .map(|c| if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') { '_' } else { c })
        .collect()
}

/// 새 목록 항목의 저장 이름을 고른다. 최종 파일이 이미 있으면 번호를 붙인다.
/// 받다 만 <이름>.part 가 있을 때, <이름>.meta 가 같은 URL 의 기록(또는 URL 줄이 없는 옛 형식)이면 그 이름을 그대로 써서
/// 이어받는다(앱을 껐다 켠 뒤 같은 URL 을 다시 받는 경우). .meta 가 없거나 다른 URL 이면 남의 .part 를 자르지 않도록 피한다.
fn unique_name(dir: &Path, name: &str, url: &str, used: &mut Vec<String>) -> String {
    // 복합 확장자(.tar.gz 등)를 통째로 뒤에 둔다: "x (1).tar.gz"
    let ext = extension_of(name);
    let stem = &name[..name.len() - ext.len()];
    let mut candidate = name.to_string();
    let mut n = 1;
    let taken = |c: &str| {
        let dest = dir.join(c);
        dest.exists() || (sidecar(&dest, "part").exists() && load_meta(&sidecar(&dest, "meta"), url).is_none())
    };
    while used.contains(&candidate) || taken(&candidate) {
        candidate = format!("{stem} ({n}){ext}");
        n += 1;
    }
    used.push(candidate.clone());
    candidate
}

fn fmt_bytes(b: u64) -> String {
    const KB: f64 = 1024.0;
    let b = b as f64;
    if b >= KB * KB * KB {
        format!("{:.2} GB", b / KB / KB / KB)
    } else if b >= KB * KB {
        format!("{:.1} MB", b / KB / KB)
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{b:.0} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;
    use std::net::{Shutdown, TcpListener, TcpStream};
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ud_test_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn item(url: &str, name: &str, dir: &Path) -> Shared {
        Arc::new(Mutex::new(vec![Item::new(1, url.into(), name.into(), dir.to_path_buf())]))
    }

    #[test]
    fn pause_keeps_part_and_resumes_cancel_removes() {
        let data = big_data();
        let (url, _fails) = serve_slow(Arc::clone(&data), 0, 20);
        let dir = tmp("pause");
        let shared = item(&url, "big.bin", &dir);
        // 받기 시작해 조금 진행되면 일시정지 신호를 보낸다
        let stop = Arc::clone(&shared.lock().unwrap()[0].stop);
        let watcher = Arc::clone(&shared);
        let w_stop = Arc::clone(&stop);
        thread::spawn(move || loop {
            if let Status::Downloading { done, .. } = watcher.lock().unwrap()[0].status {
                if done > 100_000 {
                    w_stop.store(STOP_PAUSE, Relaxed);
                    break;
                }
            }
            thread::sleep(Duration::from_millis(5));
        });
        download(Arc::clone(&shared), 0, 4);
        let paused_done = match shared.lock().unwrap()[0].status {
            Status::Paused { done, .. } => done,
            _ => panic!("일시정지 상태여야 한다"),
        };
        assert!(paused_done > 100_000 && paused_done < data.len() as u64, "중간에 멈춰야 한다: {paused_done}");
        assert!(dir.join("big.bin.part").exists() && dir.join("big.bin.meta").exists());
        // 이어받기 → 완료, 내용 일치
        stop.store(STOP_NONE, Relaxed);
        download(Arc::clone(&shared), 0, 4);
        assert!(matches!(shared.lock().unwrap()[0].status, Status::Finished));
        assert_eq!(std::fs::read(dir.join("big.bin")).unwrap(), *data);

        // 취소: .part/.meta 가 지워진다
        let dir2 = tmp("cancel");
        let shared2 = item(&url, "big.bin", &dir2);
        shared2.lock().unwrap()[0].stop.store(STOP_CANCEL, Relaxed);
        download(Arc::clone(&shared2), 0, 4);
        assert!(matches!(shared2.lock().unwrap()[0].status, Status::Cancelled));
        assert!(!dir2.join("big.bin.part").exists());
        assert!(!dir2.join("big.bin.meta").exists());
        assert!(!dir2.join("big.bin").exists());
    }

    #[test]
    fn names() {
        assert_eq!(file_name_from_url("https://a.com/x/y/photo%20one.jpg?x=1", 0), "photo one.jpg");
        assert_eq!(file_name_from_url("https://a.com/", 3), "file_3");
        let e = |line, url: &str, name: Option<&str>| Entry { line, url: url.into(), name: name.map(Into::into) };
        assert_eq!(parse_urls("https://a\n\nfoo\nhttps://b"), vec![e(1, "https://a", None), e(4, "https://b", None)]);
        assert_eq!(parse_urls("내 사진\thttps://a.com/x.jpg"), vec![e(1, "https://a.com/x.jpg", Some("내 사진"))]);
        assert_eq!(parse_urls("내 사진 | https://a.com/x.jpg"), vec![e(1, "https://a.com/x.jpg", Some("내 사진"))]);
        assert_eq!(parse_urls("https://a.com/x.jpg\t내 사진"), vec![e(1, "https://a.com/x.jpg", Some("내 사진"))]);
        assert_eq!(parse_urls("a:b https://a.com/x.jpg"), vec![e(1, "https://a.com/x.jpg", Some("a_b"))]);
        assert_eq!(parse_urls("abchttps://a.com/x.jpg"), vec![]);
        assert_eq!(target_name(&e(1, "https://a.com/x/photo.JPG?x=1", Some("사진"))), "사진.JPG");
        assert_eq!(target_name(&e(1, "https://a.com/x/noext", Some("사진"))), "사진");
        assert_eq!(target_name(&e(1, "https://a.com/rust-1.80.0.tar.gz", Some("툴체인"))), "툴체인.tar.gz");
        assert_eq!(target_name(&e(1, "https://a.com/a.TAR.XZ", Some("x"))), "x.TAR.XZ");
        assert_eq!(target_name(&e(2, "https://a.com/x/photo.jpg", None)), "photo.jpg");
        let mut used = vec![];
        let d = std::env::temp_dir();
        assert_eq!(unique_name(&d, "a.jpg", "http://x/a.jpg", &mut used), "a.jpg");
        assert_eq!(unique_name(&d, "a.jpg", "http://x/a.jpg", &mut used), "a (1).jpg");
    }

    // T3: 복합 확장자는 통째로 뒤에 둔다
    #[test]
    fn unique_name_keeps_compound_extension() {
        let d = tmp("uniq");
        std::fs::write(d.join("x.tar.gz"), b"").unwrap();
        let mut used = vec![];
        assert_eq!(unique_name(&d, "x.tar.gz", "http://x/x.tar.gz", &mut used), "x (1).tar.gz");
        assert_eq!(unique_name(&d, "x.tar.gz", "http://x/x.tar.gz", &mut used), "x (2).tar.gz");
        assert_eq!(unique_name(&d, "noext", "http://x/noext", &mut used), "noext");
        assert_eq!(unique_name(&d, "noext", "http://x/noext", &mut used), "noext (1)");
    }

    #[test]
    fn version_compare() {
        assert!(parse_version("0.3.0") > parse_version("0.2.1"));
        assert!(parse_version("1.0.0") > parse_version("0.99.99"));
        assert_eq!(parse_version("0.2.1"), parse_version("0.2.1"));
        assert!(parse_version("0.2.1-beta") <= parse_version("0.2.1"));
    }

    #[test]
    fn failed_download_is_auto_selected() {
        let shared = item("https://www.rust-lang.org/this-does-not-exist-404", "x", &tmp("404"));
        download(Arc::clone(&shared), 0, 4);
        let items = shared.lock().unwrap();
        assert!(matches!(items[0].status, Status::Failed(_)));
        assert!(items[0].selected);
    }

    #[test]
    fn downloads_real_file() {
        let dir = tmp("real");
        let shared = item("https://www.rust-lang.org/static/images/rust-logo-blk.svg", "logo.svg", &dir);
        download(Arc::clone(&shared), 0, 4);
        let st = shared.lock().unwrap()[0].status.clone();
        assert!(matches!(st, Status::Finished), "status not finished");
        assert!(std::fs::metadata(dir.join("logo.svg")).unwrap().len() > 0);
        assert!(!dir.join("logo.svg.part").exists());
        assert!(!dir.join("logo.svg.meta").exists());
    }

    // ---- Range 를 지원하는 작은 테스트 서버. `fails` 가 0 보다 크면 본문 1000 바이트 뒤에 연결을 끊는다 ----
    fn serve(data: Arc<Vec<u8>>, fails: usize) -> (String, Arc<AtomicUsize>) {
        serve_slow(data, fails, 0)
    }

    /// `chunk_delay_ms` > 0 이면 64KB 마다 그만큼 쉬며 보낸다 (일시정지 테스트용)
    fn serve_slow(data: Arc<Vec<u8>>, fails: usize, chunk_delay_ms: u64) -> (String, Arc<AtomicUsize>) {
        serve_opts(data, fails, chunk_delay_ms, true)
    }

    /// `ranges` 가 false 면 Range 헤더를 무시하고 늘 200 으로 전체를 보낸다
    fn serve_opts(data: Arc<Vec<u8>>, fails: usize, chunk_delay_ms: u64, ranges: bool) -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let fails = Arc::new(AtomicUsize::new(fails));
        let f = Arc::clone(&fails);
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (data, f) = (Arc::clone(&data), Arc::clone(&f));
                thread::spawn(move || handle(stream, data, f, chunk_delay_ms, ranges));
            }
        });
        (format!("http://127.0.0.1:{port}/big.bin"), fails)
    }

    fn handle(mut s: TcpStream, data: Arc<Vec<u8>>, fails: Arc<AtomicUsize>, chunk_delay_ms: u64, ranges: bool) {
        let mut range = None;
        let mut reader = std::io::BufReader::new(s.try_clone().unwrap());
        let mut line = String::new();
        while reader.read_line(&mut line).unwrap_or(0) > 0 {
            if line.trim().is_empty() {
                break;
            }
            if let Some((k, v)) = line.split_once(':') {
                if k.eq_ignore_ascii_case("range") {
                    range = v.trim().strip_prefix("bytes=").map(str::to_string);
                }
            }
            line.clear();
        }
        let len = data.len();
        if !ranges {
            range = None;
        }
        let (status, start, end) = match &range {
            Some(r) => {
                let (a, b) = r.split_once('-').unwrap();
                let a: usize = a.parse().unwrap();
                let b: usize = if b.is_empty() { len - 1 } else { b.parse().unwrap() };
                ("206 Partial Content", a, b.min(len - 1))
            }
            None => ("200 OK", 0, len - 1),
        };
        let body = &data[start..=end];
        let mut head = format!(
            "HTTP/1.1 {status}\r\nContent-Length: {}\r\nETag: \"t1\"\r\nAccept-Ranges: bytes\r\nConnection: close\r\n",
            body.len()
        );
        if range.is_some() {
            head += &format!("Content-Range: bytes {start}-{end}/{len}\r\n");
        }
        head += "\r\n";
        let _ = s.write_all(head.as_bytes());
        let cut = body.len() > 1000 && fails.fetch_update(SeqCst, SeqCst, |v| v.checked_sub(1)).is_ok();
        let body = if cut { &body[..1000] } else { body };
        for chunk in body.chunks(64 * 1024) {
            if s.write_all(chunk).is_err() {
                break;
            }
            if chunk_delay_ms > 0 {
                thread::sleep(Duration::from_millis(chunk_delay_ms));
            }
        }
        let _ = s.shutdown(Shutdown::Both);
    }

    /// 다른 테스트 모듈용: 64KB 마다 `delay_ms` 쉬는 Range 지원 서버
    pub(super) fn serve_slow_pub(data: Arc<Vec<u8>>, delay_ms: u64) -> String {
        serve_slow(data, 0, delay_ms).0
    }

    /// 다른 테스트 모듈용: Range 를 무시하는 서버
    pub(super) fn serve_no_range_pub(data: Arc<Vec<u8>>, delay_ms: u64) -> String {
        serve_opts(data, 0, delay_ms, false).0
    }

    fn big_data() -> Arc<Vec<u8>> {
        Arc::new((0..3_500_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect())
    }

    #[test]
    fn segmented_download_survives_transient_disconnects() {
        let data = big_data();
        let (url, _fails) = serve(Arc::clone(&data), 2); // 처음 두 요청만 끊김 → 자동 재시도로 완료
        let dir = tmp("seg");
        let shared = item(&url, "big.bin", &dir);
        download(Arc::clone(&shared), 0, 4);
        let st = shared.lock().unwrap()[0].status.clone();
        assert!(matches!(st, Status::Finished), "status not finished");
        assert_eq!(std::fs::read(dir.join("big.bin")).unwrap(), *data);
        assert!(!dir.join("big.bin.meta").exists());
    }

    #[test]
    fn failed_download_resumes_from_meta() {
        let data = big_data();
        let (url, fails) = serve(Arc::clone(&data), usize::MAX); // 계속 끊김 → 실패
        let dir = tmp("resume");
        let shared = item(&url, "big.bin", &dir);
        download(Arc::clone(&shared), 0, 4);
        assert!(matches!(shared.lock().unwrap()[0].status, Status::Failed(_)));
        assert!(dir.join("big.bin.part").exists());
        let meta = load_meta(&dir.join("big.bin.meta"), &url).expect("meta saved");
        let raw = std::fs::read_to_string(dir.join("big.bin.meta")).unwrap();
        assert_eq!(raw.lines().nth(2), Some(url.as_str()), "새 .meta 는 셋째 줄에 URL 을 기록한다");
        assert_eq!(meta.segs.len(), 3); // 3.5MB / 1MB 최소 구간 → 3분할
        assert!(meta.segs.iter().all(|(_, _, done)| *done > 0), "부분 수신이 기록돼야 한다");

        fails.store(0, SeqCst); // 서버 정상화 후 다시 받기 → 받은 곳부터 이어서 완료
        download(Arc::clone(&shared), 0, 4);
        assert!(matches!(shared.lock().unwrap()[0].status, Status::Finished));
        assert_eq!(std::fs::read(dir.join("big.bin")).unwrap(), *data);
        assert!(!dir.join("big.bin.part").exists());
        assert!(!dir.join("big.bin.meta").exists());
    }
}

#[cfg(test)]
mod pool_tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ud_pool_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn data() -> Arc<Vec<u8>> {
        Arc::new((0..3_500_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect())
    }

    /// 조건이 맞을 때까지 기다린다. `each` 는 매 표본마다 불변식을 확인한다
    fn wait_until(shared: &Shared, what: &str, cond: impl Fn(&[Item]) -> bool, each: impl Fn(&[Item])) {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            {
                let items = shared.lock().unwrap();
                each(&items);
                if cond(&items) {
                    return;
                }
            }
            assert!(Instant::now() < deadline, "시간 초과: {what}");
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn entry(line: usize, url: &str, name: Option<&str>) -> Entry {
        Entry { line, url: url.into(), name: name.map(Into::into) }
    }

    fn settled(items: &[Item]) -> bool {
        !items.iter().any(|i| matches!(i.status, Status::Waiting | Status::Downloading { .. }))
    }

    // T1: 폴더 A 에서 일시정지 → 앱 폴더를 B 로 바꿔도 이어받기는 A 에 완료, B 의 같은 이름 파일은 그대로
    #[test]
    fn resume_goes_to_item_folder_not_current_folder() {
        let data = data();
        let url = tests::serve_slow_pub(Arc::clone(&data), 20);
        let (a, b) = (tmp("t1a"), tmp("t1b"));
        std::fs::write(b.join("big.bin"), b"original").unwrap();
        let mut app = App { concurrent: 1, folder: Some(a.clone()), ..App::default() };
        app.start(vec![entry(1, &url, None)], a.clone());
        wait_until(&app.items, "진행", |it| matches!(it[0].status, Status::Downloading { done, .. } if done > 100_000), |_| {});
        pause_all(&mut app.items.lock().unwrap());
        wait_until(&app.items, "일시정지", |it| matches!(it[0].status, Status::Paused { .. }), |_| {});
        {
            let items = app.items.lock().unwrap();
            assert!(matches!(items[0].status, Status::Paused { resumable: true, done, .. } if done > 0));
            assert_eq!(items[0].dir, a);
        }
        assert!(a.join("big.bin.part").exists() && a.join("big.bin.meta").exists());

        app.folder = Some(b.clone());
        app.spawn_workers(&[0]);
        wait_until(&app.items, "완료", settled, |_| {});
        assert!(matches!(app.items.lock().unwrap()[0].status, Status::Finished));
        assert_eq!(std::fs::read(a.join("big.bin")).unwrap(), *data);
        assert_eq!(std::fs::read(b.join("big.bin")).unwrap(), b"original");
        assert!(!b.join("big.bin.part").exists() && !a.join("big.bin.part").exists());
    }

    // T2: 동시 1개, 3개 시작 → 모두 일시정지 → 셋 다 Paused, 2·3번은 .part 없음
    // → a 만 이어받아 받는 도중에 b·c 의 [이어받기] 를 눌러도 활성 워커가 1개를 넘지 않음 → 셋 다 완료
    #[test]
    fn pause_all_with_queue_then_resume_all_respects_limit() {
        let data = data();
        let url = tests::serve_slow_pub(Arc::clone(&data), 50);
        let dir = tmp("t2");
        let mut app = App { concurrent: 1, ..App::default() };
        let entries = vec![entry(1, &url, Some("a")), entry(2, &url, Some("b")), entry(3, &url, Some("c"))];
        app.start(entries, dir.clone());
        let at_most_one = |it: &[Item]| {
            let n = it.iter().filter(|i| matches!(i.status, Status::Downloading { .. })).count();
            assert!(n <= 1, "동시 파일 한도(1)를 넘었다: {n}");
        };
        wait_until(&app.items, "첫 항목 진행", |it| matches!(it[0].status, Status::Downloading { done, .. } if done > 100_000), at_most_one);
        {
            let mut items = app.items.lock().unwrap();
            assert!(matches!(items[1].status, Status::Waiting) && matches!(items[2].status, Status::Waiting));
            pause_all(&mut items);
            // 대기 중이던 항목은 즉시 일시정지로 보인다
            assert!(matches!(items[1].status, Status::Paused { .. }) && matches!(items[2].status, Status::Paused { .. }));
        }
        wait_until(&app.items, "모두 멈춤", settled, at_most_one);
        {
            let items = app.items.lock().unwrap();
            assert!(items.iter().all(|i| matches!(i.status, Status::Paused { .. })));
            assert!(matches!(items[0].status, Status::Paused { resumable: true, done, .. } if done > 0));
        }
        // 워커는 큐에 남은 옛 작업을 건너뛰고 끝나야 한다
        let deadline = Instant::now() + Duration::from_secs(10);
        while app.active_workers.load(Relaxed) > 0 {
            assert!(Instant::now() < deadline, "워커가 끝나지 않았다");
            thread::sleep(Duration::from_millis(5));
        }
        assert!(dir.join("a.bin.part").exists());
        assert!(!dir.join("b.bin.part").exists() && !dir.join("c.bin.part").exists());

        // 활성 워커 수의 최대값을 끝날 때까지 기록한다
        let (max_active, done_flag) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicBool::new(false)));
        let sampler = {
            let (active, max_active, done_flag) = (Arc::clone(&app.active_workers), Arc::clone(&max_active), Arc::clone(&done_flag));
            thread::spawn(move || {
                while !done_flag.load(Relaxed) {
                    max_active.fetch_max(active.load(Relaxed), Relaxed);
                    thread::sleep(Duration::from_micros(200));
                }
            })
        };
        // a 만 이어받기(선택한 것만 다시 받기 경로)
        let a_paused = match app.items.lock().unwrap()[0].status {
            Status::Paused { done, .. } => done,
            _ => unreachable!(),
        };
        app.items.lock().unwrap()[0].selected = true;
        app.retry_selected();
        wait_until(&app.items, "a 이어받는 중", |it| matches!(it[0].status, Status::Downloading { done, .. } if done > a_paused), at_most_one);
        // a 가 받는 도중에 b, c 의 [이어받기] 를 하나씩 누른다 (UI 와 같은 경로)
        for i in [1, 2] {
            assert!(matches!(app.items.lock().unwrap()[0].status, Status::Downloading { .. }), "a 가 아직 받는 중이어야 의미가 있다");
            app.spawn_workers(&[i]);
            assert!(app.active_workers.load(Relaxed) <= 1, "이어받기로 워커가 한도를 넘었다");
            assert!(matches!(app.items.lock().unwrap()[i].status, Status::Waiting), "자리가 없으니 대기해야 한다");
        }
        wait_until(&app.items, "모두 완료", settled, at_most_one);
        done_flag.store(true, Relaxed);
        sampler.join().unwrap();
        assert_eq!(max_active.load(Relaxed), 1, "활성 워커 최대값은 동시 파일 한도(1)여야 한다");
        for n in ["a.bin", "b.bin", "c.bin"] {
            assert_eq!(std::fs::read(dir.join(n)).unwrap(), *data, "{n}");
        }
        assert!(app.items.lock().unwrap().iter().all(|i| matches!(i.status, Status::Finished)));
    }

    // 대기 중 항목 취소는 바로 반영되고, 큐에 남은 옛 작업이 다시 이어받은 항목을 이중으로 받지 않는다
    #[test]
    fn waiting_cancel_is_immediate_and_stale_queue_is_skipped() {
        let data = data();
        let url = tests::serve_slow_pub(Arc::clone(&data), 20);
        let dir = tmp("gen");
        let mut app = App { concurrent: 1, ..App::default() };
        app.start(vec![entry(1, &url, Some("a")), entry(2, &url, Some("b"))], dir.clone());
        wait_until(&app.items, "첫 항목 진행", |it| matches!(it[0].status, Status::Downloading { .. }), |_| {});
        {
            let mut items = app.items.lock().unwrap();
            cancel_item(&mut items[1]);
            assert!(matches!(items[1].status, Status::Cancelled));
            pause_item(&mut items[0]);
        }
        wait_until(&app.items, "멈춤", settled, |_| {});
        // 취소된 b 를 다시 받으면 세대가 올라가 옛 큐 작업은 무시된다
        app.spawn_workers(&[1, 0]);
        wait_until(&app.items, "완료", settled, |it| {
            let n = it.iter().filter(|i| matches!(i.status, Status::Downloading { .. })).count();
            assert!(n <= 1);
        });
        assert_eq!(std::fs::read(dir.join("a.bin")).unwrap(), *data);
        assert_eq!(std::fs::read(dir.join("b.bin")).unwrap(), *data);
    }

    // 일시정지한 항목도 모두 취소로 파일이 지워진다
    #[test]
    fn cancel_all_removes_paused_files() {
        let dir = tmp("cancelall");
        let mut items = vec![Item::new(1, "http://x/a".into(), "a".into(), dir.clone())];
        std::fs::write(dir.join("a.part"), b"x").unwrap();
        std::fs::write(dir.join("a.meta"), "10\n\n0 10 1\n").unwrap();
        items[0].status = paused_from_disk(&items[0].dest(), "http://x/a");
        assert!(matches!(items[0].status, Status::Paused { done: 1, total: Some(10), resumable: true }));
        cancel_all(&mut items);
        assert!(matches!(items[0].status, Status::Cancelled));
        assert!(!dir.join("a.part").exists() && !dir.join("a.meta").exists());
    }

    // Range 미지원 서버에서 일시정지 → 이어받기 불가로 표시, 다시 받으면 처음부터 받아 완료
    #[test]
    fn pause_without_range_is_not_resumable() {
        let data = data();
        let url = tests::serve_no_range_pub(Arc::clone(&data), 20);
        let dir = tmp("norange");
        let shared: Shared = Arc::new(Mutex::new(vec![Item::new(1, url, "big.bin".into(), dir.clone())]));
        let (w, stop) = (Arc::clone(&shared), Arc::clone(&shared.lock().unwrap()[0].stop));
        thread::spawn(move || loop {
            if matches!(w.lock().unwrap()[0].status, Status::Downloading { done, .. } if done > 100_000) {
                stop.store(STOP_PAUSE, Relaxed);
                break;
            }
            thread::sleep(Duration::from_millis(2));
        });
        download(Arc::clone(&shared), 0, 4);
        assert!(matches!(shared.lock().unwrap()[0].status, Status::Paused { resumable: false, .. }));
        shared.lock().unwrap()[0].stop.store(STOP_NONE, Relaxed);
        download(Arc::clone(&shared), 0, 4);
        assert!(matches!(shared.lock().unwrap()[0].status, Status::Finished));
        assert_eq!(std::fs::read(dir.join("big.bin")).unwrap(), *data);
    }

    // 시작할 때 남의 같은 이름 파일이 있으면 받지 않고 바로 실패한다. 파일은 그대로, .part 도 생기지 않는다
    #[test]
    fn never_overwrites_existing_file() {
        let dir = tmp("noover");
        // 연결할 수 없는 주소: 네트워크에 나갔다면 '같은 이름' 이 아니라 연결 오류가 났을 것이다
        let shared: Shared = Arc::new(Mutex::new(vec![Item::new(1, "http://127.0.0.1:1/big.bin".into(), "big.bin".into(), dir.clone())]));
        std::fs::write(dir.join("big.bin"), b"mine").unwrap();
        download(Arc::clone(&shared), 0, 4);
        assert!(matches!(&shared.lock().unwrap()[0].status, Status::Failed(e) if e == EXISTS));
        assert_eq!(std::fs::read(dir.join("big.bin")).unwrap(), b"mine");
        assert!(!dir.join("big.bin.part").exists() && !dir.join("big.bin.meta").exists());
    }

    // 안전망: 받는 도중에 같은 이름 파일이 생기면 마지막에 덮어쓰지 않고 실패한다
    #[test]
    fn file_created_during_download_is_not_overwritten() {
        let data = data();
        let url = tests::serve_slow_pub(Arc::clone(&data), 20);
        let dir = tmp("noover2");
        let shared: Shared = Arc::new(Mutex::new(vec![Item::new(1, url, "big.bin".into(), dir.clone())]));
        let (w, d) = (Arc::clone(&shared), dir.clone());
        thread::spawn(move || loop {
            if matches!(w.lock().unwrap()[0].status, Status::Downloading { done, .. } if done > 0) {
                std::fs::write(d.join("big.bin"), b"mine").unwrap();
                break;
            }
            thread::sleep(Duration::from_millis(2));
        });
        download(Arc::clone(&shared), 0, 4);
        assert!(matches!(&shared.lock().unwrap()[0].status, Status::Failed(e) if e == EXISTS));
        assert_eq!(std::fs::read(dir.join("big.bin")).unwrap(), b"mine");
    }

    // 완료 항목을 다시 받으면 자기 파일을 지우고 처음부터 받아 다시 완료된다 (덮어쓰기)
    #[test]
    fn finished_item_redownload_overwrites_own_file() {
        let data = data();
        let url = tests::serve_slow_pub(Arc::clone(&data), 0);
        let dir = tmp("redl");
        let mut app = App { concurrent: 1, ..App::default() };
        app.start(vec![entry(1, &url, None)], dir.clone());
        wait_until(&app.items, "첫 완료", settled, |_| {});
        assert!(matches!(app.items.lock().unwrap()[0].status, Status::Finished));
        // 내용을 바꿔 두면, 다시 받은 뒤 원본과 같아야 정말로 처음부터 받은 것이다
        std::fs::write(dir.join("big.bin"), b"stale").unwrap();
        app.items.lock().unwrap()[0].selected = true;
        let t0 = Instant::now();
        app.retry_selected();
        wait_until(&app.items, "다시 완료", settled, |_| {});
        let st = app.items.lock().unwrap()[0].status.clone();
        assert!(matches!(st, Status::Finished), "다시 받기 뒤 완료여야 한다: {}", match st { Status::Failed(e) => e, _ => String::new() });
        assert_eq!(std::fs::read(dir.join("big.bin")).unwrap(), *data);
        assert!(!dir.join("big.bin.part").exists() && !dir.join("big.bin.meta").exists());
        assert!(t0.elapsed() < Duration::from_secs(30));
    }

    // claim: 큐에 같은 항목의 옛 세대 작업이 남아 있으면 건너뛰고, 새 세대만 한 번 잡는다
    #[test]
    fn claim_skips_stale_generation() {
        let dir = tmp("claim");
        let shared: Shared = Arc::new(Mutex::new(vec![Item::new(1, "http://x/a".into(), "a".into(), dir)]));
        let queue: Queue = Default::default();
        // 워커가 이미 한도만큼 돈다고 해 두면 enqueue 가 새 워커를 띄우지 않아 큐를 직접 볼 수 있다
        let active = Arc::new(AtomicUsize::new(1));
        enqueue_jobs(&shared, &queue, &active, &[0], 1, 1);
        pause_item(&mut shared.lock().unwrap()[0]);
        assert!(matches!(shared.lock().unwrap()[0].status, Status::Paused { .. }));
        enqueue_jobs(&shared, &queue, &active, &[0], 1, 1);
        let jobs: Vec<_> = queue.lock().unwrap().drain(..).collect();
        assert_eq!(jobs, vec![(0, 1), (0, 2)]);
        assert!(!claim(&shared, 0, 1), "옛 세대는 건너뛴다");
        assert!(matches!(shared.lock().unwrap()[0].status, Status::Waiting));
        assert!(claim(&shared, 0, 2), "새 세대는 잡는다");
        assert!(matches!(shared.lock().unwrap()[0].status, Status::Downloading { .. }));
        assert!(!claim(&shared, 0, 2), "같은 작업을 두 번 잡지 않는다");
        assert!(!claim(&shared, 5, 1), "없는 번호는 무시");
        assert_eq!(active.load(Relaxed), 1);
    }

    // 취소 신호가 들어간 뒤 일시정지를 눌러도 취소가 유지된다. 모두 취소는 일시정지 위에 덮어쓴다
    #[test]
    fn pause_does_not_override_cancel() {
        let mut it = Item::new(1, "http://x/a".into(), "a".into(), tmp("sig"));
        it.status = Status::Downloading { done: 1, total: None, parts: 1 };
        cancel_item(&mut it);
        pause_item(&mut it);
        assert_eq!(it.stop.load(Relaxed), STOP_CANCEL);
        it.stop.store(STOP_NONE, Relaxed);
        pause_item(&mut it);
        assert_eq!(it.stop.load(Relaxed), STOP_PAUSE);
        cancel_all(std::slice::from_mut(&mut it));
        assert_eq!(it.stop.load(Relaxed), STOP_CANCEL);
    }

    // 다른 URL 의 진행 기록(.meta)은 이어받지 않고 처음부터 받는다. 옛 형식(URL 줄 없음)은 읽는다
    #[test]
    fn meta_of_other_url_is_not_resumed() {
        let data = data();
        let url = tests::serve_slow_pub(Arc::clone(&data), 0);
        let dir = tmp("metaurl");
        let total = data.len();
        // 크기·ETag 는 같지만 다른 URL 의, '다 받았다' 고 적힌 쓰레기 .part
        std::fs::write(dir.join("big.bin.part"), vec![0xAAu8; total]).unwrap();
        let other = format!("{total}\n\"t1\"\nhttp://other.example/big.bin\n0 {total} {total}\n");
        std::fs::write(dir.join("big.bin.meta"), &other).unwrap();
        assert!(load_meta(&dir.join("big.bin.meta"), &url).is_none());
        assert!(load_meta(&dir.join("big.bin.meta"), "http://other.example/big.bin").is_some());
        let shared: Shared = Arc::new(Mutex::new(vec![Item::new(1, url.clone(), "big.bin".into(), dir.clone())]));
        download(Arc::clone(&shared), 0, 4);
        assert!(matches!(shared.lock().unwrap()[0].status, Status::Finished));
        assert_eq!(std::fs::read(dir.join("big.bin")).unwrap(), *data, "처음부터 받아야 내용이 맞다");

        // 옛 형식: 셋째 줄이 바로 구간
        let old = dir.join("old.meta");
        std::fs::write(&old, "10\n\n0 5 1\n5 10 2\n").unwrap();
        let m = load_meta(&old, "http://anything").expect("옛 형식도 읽는다");
        assert_eq!((m.total, m.etag.as_str(), m.segs), (10, "", vec![(0, 5, 1), (5, 10, 2)]));
    }

    // 받다 만 <이름>.part 가 있는 이름도 새 목록에서 피한다
    #[test]
    fn unique_name_avoids_leftover_part() {
        let d = tmp("uniqpart");
        std::fs::write(d.join("a.jpg.part"), b"x").unwrap();
        let mut used = vec![];
        assert_eq!(unique_name(&d, "a.jpg", "http://x/a.jpg", &mut used), "a (1).jpg");
    }

    /// 앞 구간 절반만 받은 .part 와 그 진행 기록(.meta)을 만든다. `url` 이 None 이면 URL 줄 없는 옛 형식
    const PART_MARKER: u8 = 0xAA;

    fn leave_partial(dir: &Path, data: &[u8], url: Option<&str>) -> (Vec<u8>, String) {
        let total = data.len();
        let mid = total / 2;
        let got = mid / 2;
        // 받은 구간은 실제 데이터가 아니라 마커(0xAA)로 채운다: 이어받기라면 최종 파일에 마커가 그대로 남고,
        // 처음부터 다시 받았다면 실제 데이터로 덮여 구별된다
        let mut part = vec![0u8; total];
        part[..got].fill(PART_MARKER);
        let url_line = url.map(|u| format!("{u}\n")).unwrap_or_default();
        let meta = format!("{total}\n\"t1\"\n{url_line}0 {mid} {got}\n{mid} {total} 0\n");
        std::fs::write(dir.join("big.bin.part"), &part).unwrap();
        std::fs::write(dir.join("big.bin.meta"), &meta).unwrap();
        (part, meta)
    }

    /// 새 목록으로 시작해 끝날 때까지 기다린 뒤 (고른 이름, 상태) 를 돌려준다
    fn start_and_settle(dir: &Path, url: &str) -> (String, Status) {
        let mut app = App { concurrent: 1, ..App::default() };
        app.start(vec![entry(1, url, None)], dir.to_path_buf());
        wait_until(&app.items, "완료", settled, |_| {});
        let items = app.items.lock().unwrap();
        (items[0].name.clone(), items[0].status.clone())
    }

    // 앱을 껐다 켠 뒤 같은 URL 을 같은 폴더에 다시 받으면 남은 .part 를 같은 이름으로 이어받는다 (옛 형식·새 형식 .meta)
    #[test]
    fn restart_resumes_leftover_part_with_same_name() {
        let data = data();
        let url = tests::serve_slow_pub(Arc::clone(&data), 0);
        for (tag, meta_url) in [("restart_old", None), ("restart_new", Some(url.as_str()))] {
            let dir = tmp(tag);
            leave_partial(&dir, &data, meta_url);
            let (name, st) = start_and_settle(&dir, &url);
            assert_eq!(name, "big.bin", "{tag}: 같은 이름으로 이어받아야 한다");
            assert!(matches!(st, Status::Finished), "{tag}: 완료여야 한다");
            let got = data.len() / 2 / 2;
            let out = std::fs::read(dir.join("big.bin")).unwrap();
            assert!(out[..got].iter().all(|&b| b == PART_MARKER), "{tag}: 이미 받은 구간을 다시 받지 않아야 한다");
            assert_eq!(out[got..], data[got..], "{tag}: 나머지 구간은 서버 데이터");
            assert!(!dir.join("big.bin.part").exists() && !dir.join("big.bin.meta").exists(), "{tag}: .part/.meta 정리");
            assert!(!dir.join("big (1).bin").exists(), "{tag}");
        }
    }

    // 다른 URL 의 .part/.meta 가 남아 있으면 그 이름을 피해 새 이름으로 받고, 남의 .part/.meta 는 그대로 둔다
    #[test]
    fn restart_with_other_url_part_picks_new_name() {
        let data = data();
        let url = tests::serve_slow_pub(Arc::clone(&data), 0);
        let dir = tmp("restart_other");
        let (part, meta) = leave_partial(&dir, &data, Some("http://other.example/big.bin"));
        let (name, st) = start_and_settle(&dir, &url);
        assert_eq!(name, "big (1).bin");
        assert!(matches!(st, Status::Finished));
        assert_eq!(std::fs::read(dir.join("big (1).bin")).unwrap(), *data);
        assert_eq!(std::fs::read(dir.join("big.bin.part")).unwrap(), part);
        assert_eq!(std::fs::read_to_string(dir.join("big.bin.meta")).unwrap(), meta);
        assert!(!dir.join("big.bin").exists());
    }

    // 이어받기 기록이 맞아도 최종 파일이 이미 있으면 번호를 붙인다
    #[test]
    fn unique_name_resumable_part_but_final_exists() {
        let d = tmp("uniqfinal");
        let url = "http://x/big.bin";
        leave_partial(&d, &[1u8; 16], Some(url));
        let mut used = vec![];
        assert_eq!(unique_name(&d, "big.bin", url, &mut used), "big.bin", "같은 URL 의 기록이면 그 이름");
        std::fs::write(d.join("big.bin"), b"done").unwrap();
        let mut used = vec![];
        assert_eq!(unique_name(&d, "big.bin", url, &mut used), "big (1).bin");
    }

    // T4: 설정 저장 → 불러오기 왕복 (임시 HOME). 지금 없는 폴더도 값은 유지
    #[test]
    fn config_roundtrip() {
        let home = tmp("home");
        std::env::set_var("HOME", &home);
        let mut app = App {
            folder: Some(PathBuf::from("/Volumes/없는 드라이브/받기")),
            concurrent: 5,
            segments: 7,
            theme: Theme::Dark,
            ..App::default()
        };
        assert_eq!(App::default().theme, Theme::System, "기본 테마는 시스템");
        app.save_config_if_changed();
        let path = config_path().unwrap();
        assert!(path.starts_with(&home) && path.exists());
        assert!(!sidecar(&path, "tmp").exists(), "임시 파일이 남으면 안 된다");
        let mut loaded = App::default();
        loaded.load_config();
        assert_eq!(loaded.folder, app.folder);
        assert_eq!((loaded.concurrent, loaded.segments, loaded.theme), (5, 7, Theme::Dark));
        assert!(std::fs::read_to_string(&path).unwrap().contains("\"theme\":\"dark\""));
        // 순환 순서: 데이 → 나이트 → 시스템 → 데이
        assert_eq!(Theme::Light.next(), Theme::Dark);
        assert_eq!(Theme::Dark.next(), Theme::System);
        assert_eq!(Theme::System.next(), Theme::Light);
        assert_eq!(loaded.saved_config, app.saved_config);
    }

    #[test]
    fn finish_notification_rules() {
        let d = PathBuf::from("/tmp");
        let mk = |s: Status| {
            let mut i = Item::new(1, String::new(), String::new(), d.clone());
            i.status = s;
            i
        };
        assert_eq!(finish_message(&[mk(Status::Finished), mk(Status::Cancelled)]).as_deref(), Some("1개 파일 다운로드 완료"));
        assert_eq!(finish_message(&[mk(Status::Finished), mk(Status::Failed("x".into()))]).as_deref(), Some("완료 1개, 실패 1개"));
        assert_eq!(finish_message(&[mk(Status::Cancelled)]), None);
        assert_eq!(finish_message(&[mk(Status::Finished), mk(Status::Paused { done: 0, total: None, resumable: true })]), None);
    }

    #[test]
    fn hourly_recheck_keeps_available() {
        let avail = UpdateState::Available { version: "9.9.9".into(), url: "u".into() };
        assert!(next_update_state(&avail, Err("offline".into())).is_none());
        assert!(next_update_state(&avail, Ok(Some(("9.9.9".into(), "u".into())))).is_none());
        assert!(matches!(next_update_state(&avail, Ok(None)), Some(UpdateState::UpToDate)));
        assert!(matches!(next_update_state(&UpdateState::Checking, Err("x".into())), Some(UpdateState::Error(_))));
        assert!(next_update_state(&UpdateState::Installing, Ok(None)).is_none());
    }

    #[test]
    fn dropped_file_filtering() {
        let d = tmp("drop");
        let ok = d.join("list.txt");
        std::fs::write(&ok, "제목\n사진\thttps://a.com/x.jpg\n그냥 줄\nhttp://b.com/y\n").unwrap();
        assert_eq!(url_lines_from_file(&ok).as_deref(), Some("사진\thttps://a.com/x.jpg\nhttp://b.com/y"));
        let bin = d.join("bin.dat");
        std::fs::write(&bin, b"https://a.com/x\0\x01").unwrap();
        assert_eq!(url_lines_from_file(&bin), None);
        let big = d.join("big.txt");
        let f = File::create(&big).unwrap();
        f.set_len(DROP_MAX_BYTES + 1).unwrap();
        assert_eq!(url_lines_from_file(&big), None);
        let none = d.join("none.txt");
        std::fs::write(&none, "no urls here\n").unwrap();
        assert_eq!(url_lines_from_file(&none), None);
    }
}
