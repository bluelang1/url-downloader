use eframe::egui;
use reqwest::blocking::{Client, Response};
use reqwest::header::{HeaderName, CONTENT_RANGE, ETAG, RANGE};
use reqwest::StatusCode;
use std::collections::{HashMap, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicU8, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

mod s3;

const REPO: &str = "bluelang1/url-downloader";

/// 자체 업데이트 zip 의 minisign 서명을 확인할 공개키 (공개키 파일 `*.pub` 의 두 번째 줄, base64 한 줄).
/// 주 키·예비 키를 넣고, 하나라도 검증에 성공하면 통과. 비어 있으면 모든 업데이트 설치를 거부한다
const UPDATE_PUBLIC_KEYS: &[&str] = &[];
/// 업데이트 서명 파일(`<zip>.minisig`) 크기 상한
const MAX_SIGNATURE_BYTES: u64 = 16 * 1024;
/// 업데이트 요청의 연결 타임아웃
const UPDATE_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// 릴리스 API·서명 파일 요청의 전체 타임아웃 (작은 응답)
const UPDATE_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// 업데이트 zip 내려받기의 멈춤 타임아웃: 이 시간 동안 한 바이트도 오지 않으면 끊는다 (전체 시간 제한 없음)
const UPDATE_STALL_TIMEOUT: Duration = Duration::from_secs(30);
/// 업데이트 설치 실패 문구 끝에 붙이는 안내
const MANUAL_UPDATE_HINT: &str = "릴리스 페이지(https://github.com/bluelang1/url-downloader/releases)에서 직접 받을 수 있습니다";

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

/// 식별자(`s3://버킷/키`) → 실제로 요청할 주소(presigned https). 받기를 시작·재개할 때마다 새로 부른다
type Presign = Arc<dyn Fn(&str) -> Result<String, String> + Send + Sync>;

struct Item {
    line: usize,
    /// 항목의 안정 식별자. URL 항목은 그 URL, S3 항목은 `s3://버킷/키`.
    /// 화면 표시·이어받기 기록(.meta)·이름 고르기에 쓴다 (presigned 주소는 여기 두지 않는다)
    url: String,
    /// S3 항목만: 받을 때마다 식별자로 요청 주소를 발급한다. None 이면 `url` 로 바로 요청
    presign: Option<Presign>,
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
    /// 일시정지 사유 안내 (S3 항목이 요청 주소를 다시 발급받지 못해 멈춘 경우). 다시 큐에 넣으면 지운다
    note: Option<String>,
}

impl Item {
    fn new(line: usize, url: String, name: String, dir: PathBuf) -> Self {
        Self {
            line,
            url,
            presign: None,
            name,
            dir,
            status: Status::Waiting,
            selected: false,
            stop: Arc::new(AtomicU8::new(STOP_NONE)),
            speed: 0.0,
            sample: None,
            gen: Arc::new(AtomicU64::new(0)),
            note: None,
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
    /// 입력창을 URL 목록으로 볼지 S3 경로 목록으로 볼지
    mode: Mode,
    /// S3 모드의 기본 버킷 (줄에 s3://버킷/ 이 없을 때 쓴다)
    bucket: String,
    /// 장기 키를 읽을 ~/.aws/credentials 프로필 (설정 키 s3_profile, 기본 "default")
    s3_profile: String,
    /// 세션 토큰을 읽을 프로필 (설정 키 s3_session_profile, 기본 없음)
    s3_session_profile: Option<String>,
    /// 버킷 → 리전 (앱이 켜져 있는 동안 기억)
    s3_regions: Arc<Mutex<HashMap<String, String>>>,
    /// 테스트용: S3 요청 주소 발급을 바꿔 끼운다. None 이면 실제 자격증명으로 presign
    presign_override: Option<Presign>,
    /// MFA 기기 ARN (설정 키 s3_mfa_serial). ~/.aws/config 의 s3_profile 프로필에 mfa_serial 이 없을 때만 쓴다
    s3_mfa_serial: String,
    /// MFA 6자리 코드 입력 칸. 요청을 보내는 즉시 비운다
    mfa_code: String,
    /// MFA 세션 발급 작업 상태 (백그라운드 스레드가 바꾼다)
    mfa_job: Arc<Mutex<MfaJob>>,
    /// MFA 줄 표시용 캐시. 파일은 MFA_INFO_TTL 마다·갱신 직후·시작 직전에만 읽고, 남은 시간은 프레임마다 계산한다
    mfa_info: MfaInfo,
    mfa_info_at: Option<Instant>,
    /// 다운로드를 시작하지 않은 이유 (MFA 세션 없음)
    start_notice: Option<String>,
    /// STS 주소. None 이면 실제 STS (개발·테스트용 URL_DOWNLOADER_STS_ENDPOINT)
    sts_endpoint: Option<String>,
    /// 테스트용: 자격증명·AWS 설정·세션 파일 경로. None 이면 HOME·환경변수 기준
    s3_files_override: Option<S3Files>,
    /// [기기 변경] 을 눌러 설정의 기기 ARN 칸을 연 상태 (칸에서 포커스가 빠지면 닫는다)
    mfa_serial_edit: bool,
}

/// MFA 세션 발급 작업 상태
#[derive(Clone, Debug)]
enum MfaJob {
    Idle,
    Running,
    /// 세션을 저장했다. UI 가 보고 상태를 다시 읽은 뒤 Idle 로 돌린다
    Done,
    Error(String),
}

/// 입력 모드
#[derive(Clone, Copy, PartialEq, Debug)]
enum Mode {
    Url,
    S3,
}

impl Mode {
    fn key(self) -> &'static str {
        match self {
            Mode::Url => "url",
            Mode::S3 => "s3",
        }
    }

    fn from_key(s: &str) -> Option<Self> {
        match s {
            "url" => Some(Mode::Url),
            "s3" => Some(Mode::S3),
            _ => None,
        }
    }
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
            mode: Mode::Url,
            bucket: String::new(),
            s3_profile: "default".to_string(),
            s3_session_profile: None,
            s3_regions: Default::default(),
            presign_override: None,
            s3_mfa_serial: String::new(),
            mfa_code: String::new(),
            mfa_job: Arc::new(Mutex::new(MfaJob::Idle)),
            mfa_info: MfaInfo::default(),
            mfa_info_at: None,
            start_notice: None,
            sts_endpoint: std::env::var("URL_DOWNLOADER_STS_ENDPOINT").ok().filter(|s| !s.is_empty()),
            s3_files_override: None,
            mfa_serial_edit: false,
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
            // 태그는 주소에서 읽고, 파일명 버전·현재 버전 비교·서명 확인은 앱 안 업데이트와 똑같이 거친다
            match install_update(url, None, &|s| {
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
            // 개발용: 입력 모드·버킷. 설정 파일에 저장된 값으로 치지 않도록 저장 기준(saved_config)도 다시 잡는다
            if let Some(m) = std::env::var("URL_DOWNLOADER_MODE").ok().as_deref().and_then(Mode::from_key) {
                app.mode = m;
            }
            if let Ok(b) = std::env::var("URL_DOWNLOADER_BUCKET") {
                app.bucket = b;
            }
            app.saved_config = app.config_json();
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
        self.poll_mfa();
        if matches!(*self.mfa_job.lock().unwrap(), MfaJob::Running) {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        if std::mem::take(&mut self.autostart) {
            if let (Some(dir), Ok(urls)) = (self.folder.clone(), self.parse_input()) {
                self.start_checked(urls, dir);
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
            // 입력 모드: URL | S3. S3 일 때만 버킷 칸이 보인다. 모드를 바꿔도 아래 목록은 그대로 둔다
            ui.horizontal(|ui| {
                for (m, label, tip) in [
                    (Mode::Url, "URL", "http(s) 링크 목록을 받습니다"),
                    (Mode::S3, "S3", "AWS S3 버킷 안의 경로 목록을 받습니다 (~/.aws/credentials 사용)"),
                ] {
                    if ui.selectable_label(self.mode == m, label).on_hover_text(tip).clicked() {
                        if m == Mode::S3 && self.mode != Mode::S3 {
                            // S3 로 들어올 때 MFA 상태를 새로 읽는다
                            self.mfa_info_at = None;
                        }
                        self.mode = m;
                        self.start_notice = None;
                    }
                }
                if self.mode == Mode::S3 {
                    ui.separator();
                    ui.label("버킷");
                    ui.add(egui::TextEdit::singleline(&mut self.bucket).hint_text("example-bucket").desired_width(220.0))
                        .on_hover_text("줄에 s3://버킷/경로 로 쓰면 그 줄은 이 칸 대신 그 버킷에서 받습니다");
                }
            });
            if self.mode == Mode::S3 {
                self.mfa_row(ui);
            }
            ui.add_space(4.0);
            let (list_title, format_hint, input_hint) = match self.mode {
                Mode::Url => (
                    "링크 목록 (한 줄에 하나)",
                    "형식: URL  또는  파일명<Tab>URL  /  파일명 | URL   (확장자는 원본 그대로)",
                    "https://...  또는  파일명<Tab>https://...",
                ),
                Mode::S3 => (
                    "S3 경로 목록 (한 줄에 하나)",
                    "형식: 경로  또는  이름<Tab>경로  /  이름 | 경로  /  s3://버킷/경로   (확장자는 원본 그대로)",
                    "경로/파일.zip  또는  이름 | 경로/파일.zip",
                ),
            };
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(list_title).strong());
                ui.label(egui::RichText::new(format_hint).color(tone(ui, Tone::Muted)).small());
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
                                .hint_text(input_hint),
                        );
                    });
                });

            ui.add_space(8.0);
            // 끝나지 않은 항목(대기·진행·일시정지)이 있으면 폴더를 못 바꾸고 새로 시작하지도 못한다:
            // 그 항목들은 처음 폴더에 이어서 받고, 새 목록은 이전 목록을 통째로 바꾸기 때문이다
            let unfinished = self.items.lock().unwrap().iter().any(Item::is_unfinished);
            // S3 모드에서 버킷을 알 수 없는 줄이 있으면 시작하지 않고 이유를 보여준다
            let (urls, input_error) = match self.parse_input() {
                Ok(u) => (u, None),
                Err(e) => (Vec::new(), Some(e)),
            };
            ui.horizontal(|ui| {
                let mut btn = ui.add_enabled(
                    !unfinished && !urls.is_empty(),
                    egui::Button::new(format!("다운로드 ({}개)", urls.len())),
                );
                if unfinished {
                    btn = btn.on_disabled_hover_text("진행 중이거나 일시정지된 항목이 있습니다. 끝내거나 취소한 뒤 시작하세요");
                } else if let Some(e) = &input_error {
                    btn = btn.on_disabled_hover_text(e);
                }
                // MFA 기기를 아는데 유효 세션이 없으면 폴더를 고르기 전에 막고 안내한다
                if btn.clicked() && !self.mfa_blocks_start(&urls) {
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
            if let Some(e) = &input_error {
                ui.colored_label(tone(ui, Tone::Bad), format!("⚠ {e}"));
            }
            if let Some(n) = &self.start_notice {
                ui.colored_label(tone(ui, Tone::Warn), format!("⚠ {n}"));
            }

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
                    if let (Status::Paused { .. }, Some(note)) = (&item.status, &item.note) {
                        ui.horizontal(|ui| {
                            ui.add_space(54.0);
                            ui.colored_label(tone(ui, Tone::Warn), format!("⚠ {note}"));
                        });
                    }
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

/// config.json 과 같은 폴더의 S3 세션 캐시 (MFA 갱신이 쓰고, 발급기가 가장 먼저 읽는다)
fn s3_session_path() -> Option<PathBuf> {
    Some(config_path()?.with_file_name("s3_session.json"))
}

/// AWS_SHARED_CREDENTIALS_FILE 또는 ~/.aws/credentials
fn aws_credentials_path() -> Option<PathBuf> {
    aws_file("AWS_SHARED_CREDENTIALS_FILE", ".aws/credentials")
}

/// AWS_CONFIG_FILE 또는 ~/.aws/config (mfa_serial 을 읽는다)
fn aws_config_path() -> Option<PathBuf> {
    aws_file("AWS_CONFIG_FILE", ".aws/config")
}

fn aws_file(env: &str, under_home: &str) -> Option<PathBuf> {
    if let Some(p) = std::env::var_os(env).filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(p));
    }
    Some(PathBuf::from(std::env::var_os("HOME")?).join(under_home))
}

/// S3 가 읽는 파일들. ~/.aws 의 두 파일은 읽기만 하고, 앱이 쓰는 것은 세션 캐시뿐이다
#[derive(Clone, Debug)]
struct S3Files {
    credentials: Option<PathBuf>,
    aws_config: Option<PathBuf>,
    session: Option<PathBuf>,
}

impl S3Files {
    fn from_env() -> Self {
        Self { credentials: aws_credentials_path(), aws_config: aws_config_path(), session: s3_session_path() }
    }
}

fn read_text(path: Option<&Path>) -> Option<String> {
    std::fs::read_to_string(path?).ok()
}

/// macOS 알림 센터에 띄운다 (osascript)
fn notify(title: &str, msg: &str) {
    let script = format!(
        "display notification \"{}\" with title \"{}\"",
        applescript_text(msg),
        applescript_text(title)
    );
    let _ = std::process::Command::new("osascript").arg("-e").arg(script).spawn();
}

/// AppleScript 큰따옴표 문자열 안에 넣을 문구. 역슬래시를 먼저 두 배로 하고(끝의 `\` 가 닫는 따옴표를 무력화하지 못하게),
/// 큰따옴표는 작은따옴표로 바꾼다
fn applescript_text(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "'")
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
        title: "S3 다운로드",
        blocks: &[
            Text("입력창 위의 [URL | S3] 에서 S3 를 고르면 AWS S3 버킷의 파일을 받습니다. 버킷 칸에 버킷 이름을 넣고, 입력창에는 버킷 안의 경로를 한 줄에 하나씩 씁니다."),
            Code("reports/2026/summary.pdf\n요약본 | reports/2026/summary.pdf\nreports/2026/summary.pdf<Tab>요약본\ns3://example-bucket/reports/2026/summary.pdf"),
            Bullets(&[
                "이름과 경로 사이 구분자는 탭이나 세로줄(|) 뿐입니다. 공백·쉼표는 경로의 일부로 봅니다(예: 'photos/2026 봄, 여행.zip').",
                "이름과 경로 중 / 가 든 쪽을 경로로 봅니다. 둘 다 / 가 없으면 확장자(. 뒤 1~8자의 영문·숫자)가 있는 쪽을 경로로 보고, 둘 다 있거나 둘 다 없으면 뒤쪽이 경로입니다(예: 'a.pdf | 보고서' 의 경로는 a.pdf).",
                "경로 맨 앞의 / 는 무시합니다('/reports/a.pdf' 는 'reports/a.pdf'). 경로에 . 이나 .. 조각이 있으면(예: 'a/../b.zip', './a.zip') 서명이 맞지 않아 받을 수 없으므로 몇 번째 줄인지 알리고 시작하지 않습니다.",
                "s3://버킷/경로 로 쓴 줄은 버킷 칸 대신 그 버킷에서 받습니다. 버킷 칸이 비어 있으면 모든 줄을 이 형식으로 써야 [다운로드] 가 켜집니다.",
                "저장 파일명 규칙은 URL 모드와 같습니다. 이름을 주면 이름 + 원본 확장자, 없으면 경로의 마지막 부분입니다. 경로의 %20 같은 글자는 풀지 않고 그대로 씁니다.",
                "자격증명은 이 순서로 찾습니다: ① 앱이 저장해 둔 세션(설정 폴더의 s3_session.json) ② 설정 s3_session_profile 에 적은 ~/.aws/credentials 프로필(세션 토큰이 있고 만료 전일 때) ③ 설정 s3_profile 프로필(기본 default)의 액세스 키. ①② 가 둘 다 유효하면 만료가 더 늦은 쪽을 씁니다. ① 은 지금 s3_profile 의 액세스 키로 받은 세션일 때만 씁니다(프로필을 바꾸면 다시 MFA 갱신). MFA 기기를 알면 ③ 은 쓰지 않습니다(아래).",
                "MFA: ~/.aws/config 의 s3_profile 프로필에 mfa_serial 이 있으면 그 기기를, 없으면 설정 s3_mfa_serial 의 기기를 씁니다. S3 를 고르면 버킷 아래 MFA 줄에 세션 상태가 보입니다: '세션 N시간 M분 남음' / '세션 없음 — 코드를 입력하세요' / 'MFA 미설정 (장기 키 사용)' / '자격증명 없음'.",
                "세션을 받으려면 MFA 앱의 6자리 코드를 넣고 [MFA 갱신] 을 누르거나 Enter 를 칩니다. s3_profile 의 액세스 키로 12시간짜리 세션을 받아 s3_session.json(권한 600)에 저장합니다. 코드는 보내는 즉시 지웁니다. 코드가 틀리면 'MFA 코드가 맞지 않습니다' 가 뜹니다. s3_profile 프로필에 aws_session_token 이 있으면(임시 자격증명) 세션을 받지 않고 장기 키 프로필을 지정하라고 안내합니다.",
                "s3_mfa_serial: ~/.aws/config 에도 설정에도 기기가 없을 때만 MFA 줄에 [기기 ARN] 칸이 나타나고, 넣은 값(예: arn:aws:iam::123456789012:mfa/example)은 config.json 의 s3_mfa_serial 로 저장됩니다. 설정에서 온 기기는 MFA 줄의 [기기 변경] 을 눌러 다시 입력할 수 있습니다(~/.aws/config 에서 온 기기는 그 파일에서 고치세요).",
                "MFA 기기를 아는데 유효한 세션이 없으면 [다운로드] 를 눌러도 시작하지 않고 'MFA 코드를 입력해 세션을 갱신하세요' 를 보여줍니다(장기 키로 대신 받다가 403 이 나지 않도록). 기기를 모르면 MFA 없이 장기 키로 받습니다.",
                "받는 도중 서명 주소가 만료돼 거부(403)되면 새 주소를 받아 그 지점부터 이어 받습니다(분할 구간 포함, 한 번 받을 때 최대 3번). 세션까지 끝나 새 주소를 받을 수 없으면 그 항목은 일시정지되고 'MFA 세션이 만료됐습니다…' 가 표시됩니다. 받은 만큼은 남아 있으니 세션을 갱신한 뒤 [이어받기] 를 누르세요. MFA 만료가 아닌 이유(자격증명 없음, 리전 확인 실패 등)로 새 주소를 받지 못하면 처음 시작할 때와 같이 실패로 표시되지만, 받은 만큼은 남아 있어 [실패·일시정지 다시 받기] 로 이어받습니다.",
                "~/.aws/credentials · ~/.aws/config 는 읽기만 하고 고치지 않습니다. 앱이 쓰는 것은 설정 폴더의 s3_session.json 뿐입니다.",
                "s3_profile / s3_session_profile 은 설정 파일(config.json)에서 바꿉니다. 예: \"s3_profile\": \"example\", \"s3_session_profile\": \"example-session\". s3_session_profile 은 없어도 됩니다.",
                "받기를 시작하거나 이어받을 때마다 임시 서명 주소를 새로 만듭니다(최대 12시간, 자격증명이 그보다 먼저 만료되면 그때까지). 목록과 .meta 에는 s3://버킷/경로 만 남고 서명 주소·키는 남기지 않습니다.",
                "일시정지·실패 뒤 이어받기, 앱을 껐다 켠 뒤 같은 경로를 같은 폴더에 다시 받기는 URL 모드와 똑같이 이어집니다.",
                "자격증명을 찾지 못하면 그 항목이 'AWS 자격증명을 찾을 수 없습니다' 로 실패합니다. ~/.aws/credentials 를 고친 뒤 [실패·일시정지 다시 받기] 를 누르세요.",
                "URL/S3 선택과 마지막 버킷은 앱을 껐다 켜도 유지됩니다. 모드를 바꿔도 아래 목록은 지워지지 않고, 목록의 항목은 각자 처음 모드대로 이어받습니다.",
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
                "동시 파일·분할 수·저장 폴더·테마·URL/S3 모드·버킷은 앱을 껐다 켜도 유지됩니다 (~/Library/Application Support/URL Downloader/config.json).",
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
                "받은 업데이트 파일은 개발자 서명(minisign)을 확인한 뒤에만 설치합니다. 서명이 없거나 맞지 않으면 설치하지 않고 받은 파일을 지웁니다.",
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
        // s3_profile·s3_session_profile 은 화면에서 바꾸지 않지만, 파일에 손으로 적은 값이 다음 저장 때 지워지지 않도록 함께 쓴다
        serde_json::json!({
            "folder": self.folder.as_ref().map(|p| p.display().to_string()),
            "concurrent": self.concurrent,
            "segments": self.segments,
            "theme": self.theme.key(),
            "mode": self.mode.key(),
            "bucket": self.bucket,
            "s3_profile": self.s3_profile,
            "s3_session_profile": self.s3_session_profile,
            "s3_mfa_serial": Some(self.s3_mfa_serial.trim()).filter(|s| !s.is_empty()),
        })
        .to_string()
    }

    fn load_config(&mut self) {
        let Some(path) = config_path() else { return };
        let Ok(text) = std::fs::read_to_string(&path) else { return };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else { return };
        self.apply_config(&v);
        self.saved_config = self.config_json();
    }

    /// 설정 값을 반영한다. 없는 키(옛 설정 파일)는 기본값 그대로 둔다
    fn apply_config(&mut self, v: &serde_json::Value) {
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
        if let Some(m) = v["mode"].as_str().and_then(Mode::from_key) {
            self.mode = m;
        }
        if let Some(b) = v["bucket"].as_str() {
            self.bucket = b.to_string();
        }
        let text = |k: &str| v[k].as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
        if let Some(p) = text("s3_profile") {
            self.s3_profile = p;
        }
        self.s3_session_profile = text("s3_session_profile");
        self.s3_mfa_serial = text("s3_mfa_serial").unwrap_or_default();
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
        let presign = urls.iter().any(|e| s3_parts(&e.url).is_some()).then(|| self.presigner());
        let items: Vec<Item> = urls
            .iter()
            .map(|e| {
                let name = unique_name(&dir, &target_name(e), &e.url, &mut names);
                let mut item = Item::new(e.line, e.url.clone(), name, dir.clone());
                if s3_parts(&e.url).is_some() {
                    item.presign = presign.clone();
                }
                item
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

    /// 지금 모드로 입력창을 해석한다. S3 모드에서 버킷을 알 수 없는 줄이 있으면 Err(안내 문구)
    fn parse_input(&self) -> Result<Vec<Entry>, String> {
        match self.mode {
            Mode::Url => Ok(parse_urls(&self.input)),
            Mode::S3 => parse_s3_input(&self.input, &self.bucket),
        }
    }

    /// S3 항목의 요청 주소 발급기. 지금 설정(프로필)과 앱의 리전 캐시를 쓴다
    fn presigner(&self) -> Presign {
        if let Some(p) = &self.presign_override {
            return Arc::clone(p);
        }
        let files = self.s3_files();
        let cfg = S3Config {
            profile: self.s3_profile.clone(),
            session_profile: self.s3_session_profile.clone(),
            credentials_file: files.credentials,
            session_cache: files.session,
            aws_config_file: files.aws_config,
            mfa_serial: Some(self.s3_mfa_serial.trim().to_string()).filter(|s| !s.is_empty()),
            region_endpoint: None,
            regions: Arc::clone(&self.s3_regions),
        };
        Arc::new(move |id: &str| cfg.presign(id))
    }

    fn s3_files(&self) -> S3Files {
        self.s3_files_override.clone().unwrap_or_else(S3Files::from_env)
    }

    /// MFA 줄 상태를 파일에서 다시 읽는다 (자격증명·AWS 설정·세션 캐시)
    fn reload_mfa_info(&mut self) {
        let now = s3::now_unix();
        self.mfa_info = MfaInfo::load(&self.s3_files(), &self.s3_profile, self.s3_session_profile.as_deref(), &self.s3_mfa_serial, now);
        self.mfa_info_at = Some(Instant::now());
    }

    /// 발급 스레드가 끝났으면 반영한다. 성공이면 상태를 다시 읽고 시작 차단 안내를 지운다
    fn poll_mfa(&mut self) {
        let done = {
            let mut job = self.mfa_job.lock().unwrap();
            let done = matches!(*job, MfaJob::Done);
            if done {
                *job = MfaJob::Idle;
            }
            done
        };
        if done {
            self.reload_mfa_info();
            self.start_notice = None;
        }
    }

    /// S3 항목이 있고 MFA 기기를 아는데 유효 세션이 없으면 true + 안내. 장기 키로 조용히 떨어져 403 이 나는 것을 막는다.
    /// 기기를 모르면 지금처럼 장기 키로 진행한다
    fn mfa_blocks_start(&mut self, urls: &[Entry]) -> bool {
        let blocked = urls.iter().any(|e| s3_parts(&e.url).is_some()) && {
            self.reload_mfa_info();
            self.mfa_info.blocks_start(s3::now_unix())
        };
        self.start_notice = blocked.then(|| MFA_NEEDED.to_string());
        blocked
    }

    /// MFA 확인을 거쳐 시작한다. 막혔으면 false
    fn start_checked(&mut self, urls: Vec<Entry>, dir: PathBuf) -> bool {
        if self.mfa_blocks_start(&urls) {
            return false;
        }
        self.start(urls, dir);
        true
    }

    /// 코드 칸의 6자리로 MFA 세션을 받는다 (백그라운드). 코드는 성공·실패와 상관없이 요청 직후 메모리에서 지운다
    fn refresh_mfa(&mut self) {
        let code = std::mem::take(&mut self.mfa_code);
        if matches!(*self.mfa_job.lock().unwrap(), MfaJob::Running) {
            wipe(code);
            return;
        }
        self.reload_mfa_info();
        let files = self.s3_files();
        let long_term = read_text(files.credentials.as_deref()).and_then(|t| s3::parse_credentials(&t, &self.s3_profile));
        let prepared = match (self.mfa_info.serial.clone(), long_term, files.session) {
            (None, ..) => Err("MFA 기기 ARN 을 먼저 입력하세요".to_string()),
            (_, None, _) => Err(format!("장기 키를 찾을 수 없습니다 (~/.aws/credentials 의 s3_profile '{}' 프로필)", self.s3_profile)),
            // 세션 토큰이 든 프로필로는 GetSessionToken 을 부를 수 없다 (STS 에 보내기 전에 막는다)
            (_, Some(long), _) if long.session_token.is_some() => Err(format!(
                "s3_profile '{}' 프로필은 임시 자격증명(세션 토큰 포함)입니다. 장기 키 프로필을 s3_profile 로 지정하세요",
                self.s3_profile
            )),
            (_, _, None) => Err("세션을 저장할 폴더를 찾을 수 없습니다 (HOME)".to_string()),
            (Some(serial), Some(long), Some(path)) => Ok((serial, long, path)),
        };
        let (serial, long, path) = match prepared {
            Ok(p) => p,
            Err(e) => {
                wipe(code);
                *self.mfa_job.lock().unwrap() = MfaJob::Error(e);
                return;
            }
        };
        *self.mfa_job.lock().unwrap() = MfaJob::Running;
        let (job, endpoint) = (Arc::clone(&self.mfa_job), self.sts_endpoint.clone());
        thread::spawn(move || {
            let result = s3::get_session_token(&long, &serial, &code, MFA_SESSION_SECS, s3::now_unix(), endpoint.as_deref());
            wipe(code);
            let next = match result {
                Ok(creds) => match s3::save_session(&path, &creds, &long.access_key) {
                    Ok(()) => MfaJob::Done,
                    Err(e) => MfaJob::Error(format!("세션 저장 실패: {e}")),
                },
                Err(s3::StsError::BadCode) => MfaJob::Error(MFA_BAD_CODE.to_string()),
                Err(s3::StsError::Other(m)) => MfaJob::Error(m),
            };
            *job.lock().unwrap() = next;
        });
    }

    /// S3 모드의 MFA 줄: 세션 상태 · 6자리 코드 · [MFA 갱신] · (기기를 모를 때만) 기기 ARN
    fn mfa_row(&mut self, ui: &mut egui::Ui) {
        if self.mfa_info_at.is_none_or(|t| t.elapsed() >= MFA_INFO_TTL) {
            self.reload_mfa_info();
        }
        let now = s3::now_unix();
        let status = self.mfa_info.status(now);
        let (text, t) = mfa_status_text(&status);
        let job = self.mfa_job.lock().unwrap().clone();
        let busy = matches!(job, MfaJob::Running);
        ui.horizontal_wrapped(|ui| {
            ui.label("MFA");
            ui.colored_label(tone(ui, t), text).on_hover_text(
                "세션은 ① 앱이 저장한 세션(s3_session.json) ② s3_session_profile 프로필 순으로 찾습니다(둘 다 유효하면 만료가 늦은 쪽).\n\
                 MFA 기기를 아는데 세션이 없으면 S3 다운로드를 시작하지 않습니다.",
            );
            ui.separator();
            let code_id = egui::Id::new("mfa_code");
            let code = ui.add_enabled(
                !busy,
                egui::TextEdit::singleline(&mut self.mfa_code).id(code_id).char_limit(6).hint_text("6자리").desired_width(64.0),
            );
            if code.changed() {
                self.mfa_code.retain(|c| c.is_ascii_digit());
            }
            let enter = code.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let why_not = if busy {
                Some("발급 중입니다")
            } else if self.mfa_info.serial.is_none() {
                Some("MFA 기기 ARN 을 먼저 입력하세요")
            } else if self.mfa_code.len() != 6 {
                Some("MFA 앱의 6자리 코드를 입력하세요")
            } else {
                None
            };
            let btn = ui
                .add_enabled(why_not.is_none(), egui::Button::new("MFA 갱신"))
                .on_hover_text("코드로 12시간짜리 세션을 받아 저장합니다")
                .on_disabled_hover_text(why_not.unwrap_or(""));
            if (btn.clicked() || enter) && why_not.is_none() {
                self.refresh_mfa();
                // 입력 칸의 되돌리기 기록에도 코드가 남지 않게 지운다
                egui::text_edit::TextEditState::default().store(ui.ctx(), code_id);
            }
            if busy {
                ui.spinner();
            }
            if let MfaJob::Error(e) = &job {
                ui.colored_label(tone(ui, Tone::Bad), e);
            }
            // 기기를 모를 때만 ARN 칸을 보인다. 입력하는 동안에는 기기를 알게 돼도 칸을 유지한다.
            // 설정 파일에서 온 기기는 [기기 변경] 으로 칸을 다시 연다 (~/.aws/config 에서 온 기기는 칸·버튼 없음)
            let arn_id = egui::Id::new("mfa_serial");
            let editing = self.mfa_serial_edit || ui.memory(|m| m.has_focus(arn_id));
            match serial_ui(self.mfa_info.serial_in_aws_config, &self.s3_mfa_serial, editing) {
                SerialUi::Hidden => {}
                SerialUi::ChangeButton => {
                    if ui.small_button("기기 변경").on_hover_text("설정 파일의 MFA 기기 ARN 을 다시 입력합니다").clicked() {
                        self.mfa_serial_edit = true;
                        ui.memory_mut(|m| m.request_focus(arn_id));
                    }
                }
                SerialUi::Field => {
                    ui.label("기기 ARN");
                    let arn = ui.add(
                        egui::TextEdit::singleline(&mut self.s3_mfa_serial)
                            .id(arn_id)
                            .hint_text("arn:aws:iam::123456789012:mfa/example")
                            .desired_width(260.0),
                    );
                    if arn.changed() {
                        self.reload_mfa_info();
                    }
                    if arn.lost_focus() {
                        self.mfa_serial_edit = false;
                    }
                }
            }
        });
    }
}

/// 자격증명을 찾지 못했을 때의 오류
const NO_AWS_CREDS: &str = "AWS 자격증명을 찾을 수 없습니다 (~/.aws/credentials)";
/// MFA 기기를 아는데 유효 세션이 없어 요청 주소를 발급할 수 없을 때 (장기 키로 떨어지지 않는다)
const MFA_EXPIRED: &str = "MFA 세션이 만료됐습니다. 갱신 후 [이어받기] 로 계속하세요";
/// 시작을 막을 때의 안내
const MFA_NEEDED: &str = "MFA 코드를 입력해 세션을 갱신하세요";
const MFA_BAD_CODE: &str = "MFA 코드가 맞지 않습니다. 새 코드로 다시 시도하세요";
/// MFA 세션 길이 (12시간)
const MFA_SESSION_SECS: u64 = 12 * 3600;
/// MFA 줄 상태를 파일에서 다시 읽는 간격
const MFA_INFO_TTL: Duration = Duration::from_secs(30);
/// presigned 주소 유효 시간 상한 (12시간)
const PRESIGN_MAX_SECS: u64 = 12 * 3600;

/// 비밀 문자열(MFA 코드)을 0 으로 덮고 버린다
fn wipe(s: String) {
    let mut b = s.into_bytes();
    b.iter_mut().for_each(|x| *x = 0);
    std::hint::black_box(&b);
}

/// 세션 자격증명의 출처
#[derive(Clone, Copy, PartialEq, Debug)]
enum SessionSource {
    /// 앱이 MFA 로 받아 저장한 세션 (s3_session.json)
    App,
    /// s3_session_profile 프로필의 세션 토큰
    Profile,
}

/// MFA 줄 표시에 필요한 것만 담은 캐시 (비밀 값은 없다)
#[derive(Clone, Default, Debug, PartialEq)]
struct MfaInfo {
    /// MFA 기기 ARN: ~/.aws/config 의 s3_profile 프로필 mfa_serial → 설정 s3_mfa_serial
    serial: Option<String>,
    /// 기기 ARN 을 ~/.aws/config 에서 찾았는지 (아니면 화면에 ARN 칸을 보인다)
    serial_in_aws_config: bool,
    /// s3_profile 프로필에 (만료되지 않은) 키가 있는지
    long_term: bool,
    /// 읽을 때 유효했던 세션의 출처와 만료 시각 (만료 시각이 적혀 있지 않은 프로필 세션은 None)
    session: Option<(SessionSource, Option<u64>)>,
}

#[derive(Clone, PartialEq, Debug)]
enum MfaStatus {
    /// `left`: 남은 초 (모르면 None)
    Valid { source: SessionSource, left: Option<u64> },
    /// 기기를 아는데 유효 세션이 없다
    NeedCode,
    /// 기기를 모른다 (장기 키로 받는다)
    NoMfa,
    /// 세션도 장기 키도 없다
    NoCreds,
}

impl MfaInfo {
    fn load(files: &S3Files, profile: &str, session_profile: Option<&str>, serial_setting: &str, now: u64) -> Self {
        let creds = read_text(files.credentials.as_deref());
        let from_config = read_text(files.aws_config.as_deref()).and_then(|t| s3::parse_mfa_serial(&t, profile));
        let setting = Some(serial_setting.trim().to_string()).filter(|s| !s.is_empty());
        Self {
            serial_in_aws_config: from_config.is_some(),
            serial: from_config.or(setting),
            long_term: creds.as_deref().and_then(|t| long_term_creds(t, profile, now)).is_some(),
            session: session_creds(files.session.as_deref(), creds.as_deref(), session_profile, profile, now).map(|(c, s)| (s, c.expires_at)),
        }
    }

    /// 지금 시각 기준으로도 유효한 세션 (남은 시간 60초 이상)
    fn valid_session(&self, now: u64) -> Option<(SessionSource, Option<u64>)> {
        self.session.filter(|(_, e)| e.is_none_or(|e| e >= now.saturating_add(s3::MIN_REMAINING_SECS)))
    }

    fn status(&self, now: u64) -> MfaStatus {
        match self.valid_session(now) {
            Some((source, exp)) => MfaStatus::Valid { source, left: exp.map(|e| e.saturating_sub(now)) },
            None if !self.long_term => MfaStatus::NoCreds,
            None if self.serial.is_some() => MfaStatus::NeedCode,
            None => MfaStatus::NoMfa,
        }
    }

    /// 기기를 아는데 유효 세션이 없으면 S3 다운로드를 시작하지 않는다
    fn blocks_start(&self, now: u64) -> bool {
        self.serial.is_some() && self.valid_session(now).is_none()
    }
}

/// MFA 줄의 기기 ARN 자리에 무엇을 보일지
#[derive(Clone, Copy, PartialEq, Debug)]
enum SerialUi {
    /// ~/.aws/config 에서 온 기기: 칸도 버튼도 없다
    Hidden,
    /// 입력 칸 (설정에도 없거나, 고치는 중)
    Field,
    /// 설정 파일(s3_mfa_serial)에서 온 기기: [기기 변경] 버튼
    ChangeButton,
}

fn serial_ui(in_aws_config: bool, setting: &str, editing: bool) -> SerialUi {
    match (in_aws_config, setting.trim().is_empty() || editing) {
        (true, _) => SerialUi::Hidden,
        (false, true) => SerialUi::Field,
        (false, false) => SerialUi::ChangeButton,
    }
}

/// MFA 줄 상태 글자와 색
fn mfa_status_text(s: &MfaStatus) -> (String, Tone) {
    match s {
        MfaStatus::Valid { source, left } => {
            let profile = *source == SessionSource::Profile;
            let text = match left {
                Some(l) => format!("세션 {} 남음{}", fmt_left(*l), if profile { " (프로필)" } else { "" }),
                None => format!("세션 사용 중 (만료 시각 모름{})", if profile { ", 프로필" } else { "" }),
            };
            (text, Tone::Ok)
        }
        MfaStatus::NeedCode => ("세션 없음 — 코드를 입력하세요".to_string(), Tone::Warn),
        MfaStatus::NoMfa => ("MFA 미설정 (장기 키 사용)".to_string(), Tone::Muted),
        MfaStatus::NoCreds => ("자격증명 없음".to_string(), Tone::Bad),
    }
}

/// 남은 시간: `N시간 M분` (1시간 미만이면 `M분`)
fn fmt_left(secs: u64) -> String {
    let (h, m) = (secs / 3600, secs % 3600 / 60);
    if h > 0 {
        format!("{h}시간 {m}분")
    } else {
        format!("{m}분")
    }
}

/// S3 요청 주소 발급에 필요한 설정
struct S3Config {
    profile: String,
    session_profile: Option<String>,
    credentials_file: Option<PathBuf>,
    session_cache: Option<PathBuf>,
    /// mfa_serial 을 읽을 AWS 설정 파일 (~/.aws/config)
    aws_config_file: Option<PathBuf>,
    /// 설정 s3_mfa_serial (AWS 설정 파일에 없을 때 쓴다)
    mfa_serial: Option<String>,
    /// 테스트용 리전 조회 주소 (기본 https://s3.amazonaws.com)
    region_endpoint: Option<String>,
    regions: Arc<Mutex<HashMap<String, String>>>,
}

impl S3Config {
    /// `s3://버킷/키` → presigned GET 주소. 자격증명을 먼저 찾고(없으면 네트워크에 나가지 않는다), 리전은 버킷별로 한 번만 묻는다.
    /// MFA 기기를 알면 세션(①②)만 쓰고, 없으면 장기 키로 떨어지지 않고 MFA_EXPIRED
    fn presign(&self, id: &str) -> Result<String, String> {
        let (bucket, key) = s3_parts(id).ok_or("S3 경로가 올바르지 않습니다")?;
        let now = s3::now_unix();
        let text = read_text(self.credentials_file.as_deref());
        let mfa = read_text(self.aws_config_file.as_deref()).and_then(|t| s3::parse_mfa_serial(&t, &self.profile)).or(self.mfa_serial.clone());
        let creds = if mfa.is_some() {
            session_creds(self.session_cache.as_deref(), text.as_deref(), self.session_profile.as_deref(), &self.profile, now)
                .map(|(c, _)| c)
                .ok_or(MFA_EXPIRED)?
        } else {
            resolve_creds(self.session_cache.as_deref(), text.as_deref(), self.session_profile.as_deref(), &self.profile, now)
                .ok_or(NO_AWS_CREDS)?
        };
        let region = self.region(bucket)?;
        Ok(s3::presign_get(&creds, &region, bucket, key, presign_secs(&creds, now), now))
    }

    fn region(&self, bucket: &str) -> Result<String, String> {
        if let Some(r) = self.regions.lock().unwrap().get(bucket) {
            return Ok(r.clone());
        }
        // 네트워크 조회 중에는 잠그지 않는다 (동시에 두 번 물어도 같은 값이 들어갈 뿐)
        let r = s3::bucket_region(bucket, self.region_endpoint.as_deref())?;
        self.regions.lock().unwrap().insert(bucket.to_string(), r.clone());
        Ok(r)
    }
}

/// 자격증명 찾는 순서: ① 앱 세션 캐시(s3_session.json, 남은 시간 60초 이상, `profile` 의 장기 키가 발급한 것)
/// ② `session_profile` 프로필의 세션 토큰(만료 전. 만료 시각이 적혀 있지 않으면 판단할 수 없어 쓴다)
/// ①② 가 둘 다 유효하면 만료가 더 늦은 쪽(②의 만료 시각을 모르면 ①)
/// ③ `profile` 프로필의 키(만료 시각이 적혀 있고 지났으면 쓰지 않는다)
fn resolve_creds(session_cache: Option<&Path>, credentials: Option<&str>, session_profile: Option<&str>, profile: &str, now: u64) -> Option<s3::Creds> {
    if let Some((c, _)) = session_creds(session_cache, credentials, session_profile, profile, now) {
        return Some(c);
    }
    long_term_creds(credentials?, profile, now)
}

/// ①② 세션만 (MFA 기기를 알 때는 이것만 쓴다). 앱 세션은 지금 `profile` 장기 키의 ID 로 발급한 것만 쓴다
fn session_creds(
    session_cache: Option<&Path>,
    credentials: Option<&str>,
    session_profile: Option<&str>,
    profile: &str,
    now: u64,
) -> Option<(s3::Creds, SessionSource)> {
    let owner = credentials.and_then(|t| s3::parse_credentials(t, profile)).map(|c| c.access_key);
    let app = owner.and_then(|o| s3::load_session(session_cache?, &o, now));
    let prof = credentials
        .zip(session_profile)
        .and_then(|(t, sp)| s3::parse_credentials(t, sp))
        .filter(|c| c.session_token.is_some() && fresh(c, now));
    match (app, prof) {
        (Some(a), Some(p)) if p.expires_at.is_some_and(|pe| a.expires_at.is_some_and(|ae| pe > ae)) => Some((p, SessionSource::Profile)),
        (Some(a), _) => Some((a, SessionSource::App)),
        (None, Some(p)) => Some((p, SessionSource::Profile)),
        (None, None) => None,
    }
}

/// ③ `profile` 프로필의 키 (만료 시각이 적혀 있고 지났으면 쓰지 않는다)
fn long_term_creds(credentials: &str, profile: &str, now: u64) -> Option<s3::Creds> {
    s3::parse_credentials(credentials, profile).filter(|c| fresh(c, now))
}

fn fresh(c: &s3::Creds, now: u64) -> bool {
    c.expires_at.is_none_or(|e| e >= now.saturating_add(s3::MIN_REMAINING_SECS))
}

/// presign 유효 시간: 12시간과 자격증명 남은 시간 중 짧은 쪽
fn presign_secs(creds: &s3::Creds, now: u64) -> u64 {
    creds.expires_at.map_or(PRESIGN_MAX_SECS, |e| e.saturating_sub(now)).clamp(1, PRESIGN_MAX_SECS)
}

/// `s3://버킷/키` → (버킷, 키). S3 항목이 아니거나 버킷·키가 비었으면 None
fn s3_parts(id: &str) -> Option<(&str, &str)> {
    let (bucket, key) = id.strip_prefix("s3://")?.split_once('/')?;
    (!bucket.is_empty() && !key.is_empty()).then_some((bucket, key))
}

/// S3 모드 입력 → 항목. 줄에 s3://버킷/ 이 있으면 그 버킷, 없으면 `bucket` 칸.
/// 둘 다 없는 줄, 또는 키에 `.`/`..` 경로 조각이 있는 줄(HTTP 클라이언트가 주소를 정규화해 서명이 깨진다)이 있으면
/// 시작하지 않도록 Err(안내 문구)
fn parse_s3_input(input: &str, bucket: &str) -> Result<Vec<Entry>, String> {
    // 버킷 칸에 s3://버킷/ 처럼 붙여넣어도 받아 준다
    let field = bucket.trim().trim_start_matches("s3://").trim_end_matches('/');
    let mut out = Vec::new();
    let (mut dots, mut missing) = (Vec::new(), Vec::new());
    for (i, l) in input.lines().enumerate() {
        let Some(p) = s3::parse_s3_line(l) else { continue };
        if p.key.split('/').any(|seg| seg == "." || seg == "..") {
            dots.push(i + 1);
            continue;
        }
        let b = p.bucket.as_deref().unwrap_or(field);
        if b.is_empty() {
            missing.push(i + 1);
            continue;
        }
        out.push(Entry { line: i + 1, url: format!("s3://{b}/{}", p.key), name: p.name });
    }
    let lines = |v: &[usize]| {
        let more = if v.len() > 3 { " 외" } else { "" };
        let shown: Vec<String> = v.iter().take(3).map(usize::to_string).collect();
        format!("{}번째 줄{more}", shown.join(", "))
    };
    let mut errs = Vec::new();
    if !dots.is_empty() {
        errs.push(format!("경로에 . 또는 .. 조각이 있어 받을 수 없습니다 ({})", lines(&dots)));
    }
    if !missing.is_empty() {
        errs.push(format!("버킷 칸이 비어 있습니다. 버킷을 입력하거나 s3://버킷/경로 로 쓰세요 ({})", lines(&missing)));
    }
    if errs.is_empty() {
        Ok(out)
    } else {
        Err(errs.join(" / "))
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
            item.note = None;
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
                if ui.add(btn).on_hover_text("새 버전을 받아 서명을 확인한 뒤 설치하고 다시 시작합니다").clicked() {
                    self.apply_update(version, url);
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
    /// `version` 은 릴리스 API 의 태그(앞의 v 제외). 내려받기 주소의 태그와 같아야 설치한다
    fn apply_update(&self, version: String, url: String) {
        let state = Arc::clone(&self.update);
        thread::spawn(move || {
            let set = |s: UpdateState| *state.lock().unwrap() = s;
            match install_update(&url, Some(&version), &set) {
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
    let (url, dest, stop, presign) = {
        let items = shared.lock().unwrap();
        (items[idx].url.clone(), items[idx].dest(), Arc::clone(&items[idx].stop), items[idx].presign.clone())
    };
    let set = |s: Status| shared.lock().unwrap()[idx].status = s;

    // 큐에서 기다리는 동안 멈춤 신호가 왔으면 시작하지 않는다
    let result = match stop.load(Relaxed) {
        STOP_NONE => match &presign {
            None => download_file(&Source::fixed(&url), &dest, segments, &stop, &set),
            // S3: 시작·재개할 때마다 요청 주소를 새로 발급한다. 이어받기 기록은 식별자(url)로 맞춘다.
            // MFA 세션이 없어 발급하지 못하면 실패가 아니라 일시정지(안내)로 둔다.
            // 오류 문구에 서명 주소가 섞이지 않도록 식별자로 바꿔 둔다
            Some(p) => match p(&url) {
                Err(e) => Err(issue_error(e)),
                Ok(fetch) => {
                    download_file(&Source::signed(&url, fetch, p), &dest, segments, &stop, &set).map_err(|e| hide_fetch_url(e, &url))
                }
            },
        },
        _ => Err(STOPPED.to_string()),
    };
    match result {
        Ok(()) => set(Status::Finished),
        Err(e) if e == STOPPED || e.starts_with(REISSUE_FAILED) => {
            let mut items = shared.lock().unwrap();
            let (done, total) = match items[idx].status {
                Status::Downloading { done, total, .. } => (done, total),
                _ => (0, None),
            };
            if stop.load(Relaxed) == STOP_CANCEL {
                remove_partial(&dest);
                items[idx].status = Status::Cancelled;
                return;
            } else if done == 0 {
                items[idx].status = paused_from_disk(&dest, &url);
            } else {
                // .meta 가 없으면 Range 미지원 경로로 받던 중이라 이어받을 수 없다
                let resumable = sidecar(&dest, "meta").exists();
                items[idx].status = Status::Paused { done, total, resumable };
            }
            // 요청 주소를 다시 받지 못해 멈췄으면(MFA 세션 만료 등) 그 사유를 보인다. 받은 만큼은 .part/.meta 에 남아 있다
            if let Some(why) = e.strip_prefix(REISSUE_FAILED) {
                items[idx].note = Some(why.to_string());
            }
        }
        // 발급 오류(MFA 만료 외)도 여기: 받던 .part/.meta 는 지우지 않으므로 다시 받으면 이어받는다
        Err(e) => {
            let mut items = shared.lock().unwrap();
            items[idx].status = Status::Failed(e.strip_prefix(ISSUE_ERROR).unwrap_or(&e).to_string());
            items[idx].selected = true;
        }
    }
}

/// 요청 주소 발급 오류 → 내부 표시. 처음 발급·도중 재발급 모두 같은 규칙:
/// MFA 세션 만료·없음(MFA_EXPIRED)만 일시정지(REISSUE_FAILED), 그 외(자격증명 없음·리전 조회 실패 등)는 오류 상태(ISSUE_ERROR)
fn issue_error(e: String) -> String {
    if e == MFA_EXPIRED {
        format!("{REISSUE_FAILED}{e}")
    } else {
        format!("{ISSUE_ERROR}{e}")
    }
}

/// 오류 문구 안의 http(s) 주소(presigned 서명·토큰 포함)를 식별자로 바꾼다.
/// 바꿔 넣은 식별자(키에 `https://` 가 들어 있을 수 있다)는 다시 보지 않도록 바꾼 자리 뒤부터 찾는다
fn hide_fetch_url(msg: String, id: &str) -> String {
    let mut out = String::with_capacity(msg.len());
    let mut rest = msg.as_str();
    while let Some(p) = [rest.find("https://"), rest.find("http://")].into_iter().flatten().min() {
        out.push_str(&rest[..p]);
        out.push_str(id);
        let tail = &rest[p..];
        rest = &tail[tail.find(|c: char| c.is_whitespace() || c == ')').unwrap_or(tail.len())..];
    }
    out.push_str(rest);
    out
}

/// 다운로드 한 번(시작·재개)에서 403 으로 요청 주소를 다시 발급하는 최대 횟수
const MAX_REISSUE: usize = 3;
/// 요청 주소를 다시 발급하지 못해 멈췄음을 나타내는 오류 접두사 (뒤에 사유). 실패가 아니라 일시정지로 바뀐다
const REISSUE_FAILED: &str = "\u{0}reissue:";
/// 요청 주소 발급이 MFA 만료 외의 이유로 실패했음을 나타내는 오류 접두사 (뒤에 사유). 구간 재시도 없이 오류 상태로 바뀐다
const ISSUE_ERROR: &str = "\u{0}issue:";

/// 요청 주소. URL 항목은 고정이고, S3 항목은 403/400 을 받으면 발급기로 새 주소를 받아 같은 범위를 다시 요청한다
struct Source<'a> {
    /// 이어받기 기록(.meta)에 남기는 식별자 (URL 항목은 요청 주소와 같다)
    id: &'a str,
    presign: Option<&'a Presign>,
    state: Mutex<SourceState>,
}

struct SourceState {
    url: String,
    reissued: usize,
    /// 재발급이 한 번 실패하면 그 오류(issue_error 로 표시한 것). 다른 구간이 발급기를 다시 부르지 않는다
    failed: Option<String>,
}

impl<'a> Source<'a> {
    fn fixed(url: &'a str) -> Self {
        Self { id: url, presign: None, state: Mutex::new(SourceState { url: url.to_string(), reissued: 0, failed: None }) }
    }

    fn signed(id: &'a str, url: String, presign: &'a Presign) -> Self {
        Self { id, presign: Some(presign), state: Mutex::new(SourceState { url, reissued: 0, failed: None }) }
    }

    fn current(&self) -> String {
        self.state.lock().unwrap().url.clone()
    }

    /// `used` 로 보낸 요청이 거부됐을 때 다음에 쓸 주소. 다른 구간이 이미 새로 받았으면 그 주소를 쓴다(구간이 여럿이어도 한 번만 발급).
    /// URL 항목이거나 상한(MAX_REISSUE)을 넘었으면 None(거부 응답을 그대로 쓴다),
    /// 발급기가 실패하면 Err(issue_error: MFA 만료는 REISSUE_FAILED, 그 외는 ISSUE_ERROR + 사유)
    fn reissue(&self, used: &str) -> Result<Option<String>, String> {
        let Some(presign) = self.presign else { return Ok(None) };
        // 발급하는 동안 잠가 둔다: 같이 거부된 다른 구간은 기다렸다가 새 주소를 받아 간다
        let mut st = self.state.lock().unwrap();
        if let Some(e) = &st.failed {
            return Err(e.clone());
        }
        if st.url != used {
            return Ok(Some(st.url.clone()));
        }
        if st.reissued >= MAX_REISSUE {
            return Ok(None);
        }
        st.reissued += 1;
        match presign(self.id) {
            Ok(url) => {
                st.url = url.clone();
                Ok(Some(url))
            }
            Err(e) => {
                let e = issue_error(e);
                st.failed = Some(e.clone());
                Err(e)
            }
        }
    }
}

/// GET 을 보낸다. S3 항목이 403/400(서명 주소·세션 만료)을 받으면 주소를 다시 발급해 같은 범위를 다시 요청한다
fn get_resp(client: &Client, src: &Source, range: Option<&str>) -> Result<Response, String> {
    let mut url = src.current();
    loop {
        let mut req = client.get(&url);
        if let Some(r) = range {
            req = req.header(RANGE, r);
        }
        let resp = req.send().map_err(|e| e.to_string())?;
        if !matches!(resp.status(), StatusCode::FORBIDDEN | StatusCode::BAD_REQUEST) {
            return Ok(resp);
        }
        match src.reissue(&url)? {
            Some(next) => url = next,
            None => return Ok(resp),
        }
    }
}

/// `dest` 옆에 `<dest>.part`(받는 중인 본문)와 `<dest>.meta`(구간별 진행 상황)를 두고 받는다.
/// 서버가 Range 를 지원하면 분할·이어받기, 아니면 처음부터 한 줄기로 받는다.
/// 요청 주소는 `src`, 이어받기 기록(.meta)에는 `src.id` 를 남긴다
fn download_file(src: &Source, dest: &Path, segments: usize, stop: &AtomicU8, set: &(dyn Fn(Status) + Sync)) -> Result<(), String> {
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
    let probe = get_resp(&client, src, Some("bytes=0-0"))?;
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
            Some(total) => download_ranged(&client, src, &part, &meta, total, &etag, segments, stop, set)?,
            None => {
                // 206 인데 전체 크기를 안 알려주는 서버: 일반 GET 으로 받는다
                let resp = get_resp(&client, src, None)?;
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
    std::fs::rename(&part, dest).map_err(|e| e.to_string())?;
    mark_quarantine(dest);
    Ok(())
}

/// 받은 최종 파일에 브라우저로 받은 파일처럼 격리 표시(com.apple.quarantine)를 단다. 열 때 Gatekeeper 확인이 뜬다.
/// 파일시스템이 확장 속성을 지원하지 않는 등으로 실패해도 다운로드는 성공으로 둔다.
/// 자체 업데이트(install_update)로 받은 번들에는 달지 않는다
fn mark_quarantine(path: &Path) {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let _ = std::process::Command::new("/usr/bin/xattr")
        .arg("-w")
        .arg("com.apple.quarantine")
        .arg(quarantine_value(now))
        // 절대경로로 넘긴다: 상대경로의 `-` 로 시작하는 파일명이 옵션으로 읽히지 않게
        .arg(std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf()))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// 격리 표시 값: `<플래그>;<유닉스 시각 16진수>;<받은 앱>;` (0081 = 다운로드한 파일, 아직 사용자가 확인하지 않음)
fn quarantine_value(unix_secs: u64) -> String {
    format!("0081;{unix_secs:08x};URL Downloader;")
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
    src: &Source,
    part: &Path,
    meta_path: &Path,
    total: u64,
    etag: &str,
    segments: usize,
    stop: &AtomicU8,
    set: &(dyn Fn(Status) + Sync),
) -> Result<(), String> {
    let id = src.id;
    // 같은 파일(식별자·크기·ETag 일치)의 진행 기록이 남아 있으면 거기서부터 이어받는다
    let resumed = load_meta(meta_path, id).filter(|m| m.total == total && m.etag == etag && part.exists());
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
    let save = || save_meta(meta_path, total, etag, id, &segs);
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
            .map(|seg| sc.spawn(move || fetch_segment(client, src, file, seg, stop, tick)))
            .collect();
        handles
            .into_iter()
            .filter_map(|h| h.join().unwrap_or_else(|_| Err("스레드 오류".into())).err())
            .collect()
    });
    if !errors.is_empty() {
        let _ = save();
        // 멈춤 신호가 원인이면 그걸 우선 보고한다 (다른 구간의 '연결 끊김' 보다). 다음은 재발급 실패(일시정지 또는 오류 상태)
        let e = errors
            .iter()
            .find(|e| *e == STOPPED)
            .or_else(|| errors.iter().find(|e| e.starts_with(REISSUE_FAILED) || e.starts_with(ISSUE_ERROR)))
            .unwrap_or(&errors[0])
            .clone();
        return Err(e);
    }
    let _ = std::fs::remove_file(meta_path);
    Ok(())
}

/// 구간 하나를 받는다. 끊기면 받은 곳부터 다시 요청한다
fn fetch_segment(client: &Client, src: &Source, file: &File, seg: &Seg, stop: &AtomicU8, tick: &(dyn Fn() + Sync)) -> Result<(), String> {
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
        match fetch_once(client, src, file, seg, stop, tick) {
            Ok(()) => return Ok(()),
            // 재발급 실패는 기다려도 풀리지 않는다 (세션 갱신·설정 수정이 필요하다)
            Err(e) if e == STOPPED || e.starts_with(REISSUE_FAILED) || e.starts_with(ISSUE_ERROR) => return Err(e),
            Err(e) => last_err = e,
        }
    }
    Err(last_err)
}

fn fetch_once(client: &Client, src: &Source, file: &File, seg: &Seg, stop: &AtomicU8, tick: &(dyn Fn() + Sync)) -> Result<(), String> {
    let len = seg.end - seg.start;
    let mut done = seg.done.load(Relaxed);
    let mut resp = get_resp(client, src, Some(&format!("bytes={}-{}", seg.start + done, seg.end - 1)))?;
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

/// .meta 형식: 전체 크기 / ETag / 식별자(URL 또는 s3://버킷/키) / 구간마다 "시작 끝 받은수" 한 줄
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

/// (새 버전, zip 다운로드 URL). 현재 버전 이하이거나 서명된 zip 이 없으면 None
fn fetch_latest_release() -> Result<Option<(String, String)>, String> {
    let resp = update_client()?
        .get(
            std::env::var("URL_DOWNLOADER_UPDATE_API")
                .unwrap_or_else(|_| format!("https://api.github.com/repos/{REPO}/releases/latest")),
        )
        .header("Accept", "application/vnd.github+json")
        .timeout(UPDATE_REQUEST_TIMEOUT)
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
    Ok(latest_release_from_json(&json, VERSION))
}

/// 릴리스 API 응답에서 (새 버전, zip 주소). zip 과 같은 이름 + `.minisig` 자산이 함께 있을 때만 새 버전으로 본다.
/// 서명 전 릴리스(zip 만 있음)는 업데이트로 제안하지 않는다 (설치해도 서명 확인에서 실패하므로 최신 상태와 같은 취급)
fn latest_release_from_json(json: &serde_json::Value, current: &str) -> Option<(String, String)> {
    let latest = json["tag_name"].as_str().unwrap_or("").trim_start_matches('v');
    if parse_version(latest) <= parse_version(current) {
        return None;
    }
    let assets: Vec<(&str, &str)> = json["assets"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| Some((a["name"].as_str()?, a["browser_download_url"].as_str()?)))
        .collect();
    assets
        .iter()
        .filter(|(name, _)| name.ends_with(".zip"))
        .find(|(name, _)| assets.iter().any(|(sig, _)| sig.strip_suffix(".minisig") == Some(*name)))
        .map(|(_, url)| (latest.to_string(), url.to_string()))
}

/// 업데이트 zip 을 받아도 되는 주소: 이 리포 릴리스의 첨부 파일. 정규화(`..`·`%2e%2e` 등)로 경로가 바뀌는 주소는 거부한다
fn is_release_asset_url(url: &str) -> bool {
    let prefix = format!("https://github.com/{REPO}/releases/download/");
    let Some(rest) = url.strip_prefix(&prefix) else { return false };
    if rest.is_empty() {
        return false;
    }
    match reqwest::Url::parse(url) {
        Ok(u) => u.as_str() == url && u.scheme() == "https" && u.host_str() == Some("github.com") && u.port().is_none(),
        Err(_) => false,
    }
}

/// 업데이트 확인·서명·zip 을 받는 클라이언트. GitHub 은 다른 호스트(objects.githubusercontent.com 등)로 보내므로
/// 리다이렉트 호스트는 제한하지 않고 https 만 따라간다
fn update_client() -> Result<Client, String> {
    update_client_with(UPDATE_CONNECT_TIMEOUT, UPDATE_STALL_TIMEOUT)
}

/// `stall`: reqwest blocking 의 클라이언트 timeout 은 전체 시간이 아니라 응답 헤더 기다림과 본문 `read()` 한 번마다
/// 적용된다(0.12 blocking 에는 read_timeout 이 없음). 그래서 zip 처럼 오래 걸리는 내려받기에는 "멈춤" 타임아웃이 된다.
/// 전체 시간을 제한할 요청(API·서명 파일)은 요청마다 `.timeout(UPDATE_REQUEST_TIMEOUT)` 을 붙인다
fn update_client_with(connect: Duration, stall: Duration) -> Result<Client, String> {
    Client::builder()
        .user_agent(format!("url-downloader/{VERSION}"))
        .connect_timeout(connect)
        .timeout(stall)
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if redirect_allowed(attempt.url().scheme(), attempt.previous().len()) {
                attempt.follow()
            } else {
                attempt.error("업데이트 리다이렉트는 https 로만 따라갑니다")
            }
        }))
        .build()
        .map_err(|e| e.to_string())
}

/// 업데이트 리다이렉트를 따라가도 되는지. `previous` 는 지금까지 요청한 주소 수(처음 요청 포함)
fn redirect_allowed(scheme: &str, previous: usize) -> bool {
    scheme == "https" && previous < 10
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

/// 설치할 업데이트 주소 `.../releases/download/<tag>/<zip>` 를 확인하고 zip 파일명을 돌려준다.
/// - 태그는 `v<버전>`, 파일명은 `URL-Downloader-<tag>-….zip` (bundle.sh 이름 규칙)
/// - `expected_version`(릴리스 API 의 태그, 앞의 v 제외)이 있으면 주소의 태그와 같아야 한다
/// - 현재 버전보다 높아야 한다 (되돌리기 방지)
fn check_update_target<'a>(url: &'a str, expected_version: Option<&str>, current: &str) -> Result<&'a str, String> {
    let prefix = format!("https://github.com/{REPO}/releases/download/");
    let rest = url
        .strip_prefix(&prefix)
        .filter(|r| !r.contains(['?', '#']))
        .ok_or_else(|| format!("공식 릴리스 주소가 아니라 설치하지 않습니다: {url}"))?;
    let (tag, file) = rest
        .split_once('/')
        .filter(|(t, f)| !t.is_empty() && !f.is_empty() && !f.contains('/'))
        .ok_or_else(|| format!("릴리스 파일 주소 형식이 아니라 설치하지 않습니다: {url}"))?;
    let version = tag
        .strip_prefix('v')
        .filter(|v| !v.is_empty())
        .ok_or_else(|| format!("릴리스 태그 형식(vX.Y.Z)이 아니라 설치하지 않습니다: {tag}"))?;
    if let Some(expected) = expected_version {
        if expected != version {
            return Err(format!("릴리스 태그(v{expected})와 내려받기 주소의 태그({tag})가 달라 설치하지 않습니다"));
        }
    }
    if !(file.starts_with(&format!("URL-Downloader-{tag}-")) && file.ends_with(".zip")) {
        return Err(format!("파일명의 버전이 릴리스 태그({tag})와 달라 설치하지 않습니다: {file}"));
    }
    if parse_version(version) <= parse_version(current) {
        return Err(format!("v{version} 이 현재 버전(v{current})보다 높지 않아 설치하지 않습니다"));
    }
    Ok(file)
}

/// 업데이트 zip 의 서명 파일 주소 (`<zip 주소>.minisig`). 공식 릴리스 주소가 아니면 None
fn update_signature_url(url: &str) -> Option<String> {
    let sig = format!("{url}.minisig");
    is_release_asset_url(&sig).then_some(sig)
}

/// `max` 바이트까지만 읽는다. 더 있으면 오류 (끝없는 응답도 max + 1 바이트에서 멈춘다)
fn read_limited(r: impl Read, max: u64) -> Result<Vec<u8>, String> {
    let mut buf = Vec::new();
    r.take(max + 1).read_to_end(&mut buf).map_err(|e| e.to_string())?;
    if buf.len() as u64 > max {
        return Err(format!("서명 파일이 크기 상한({max} 바이트)을 넘습니다"));
    }
    Ok(buf)
}

/// 업데이트 zip 의 minisign 서명 확인 (네트워크 없음).
/// `keys` 중 하나라도 검증에 성공해야 하고, 서명에 포함된 trusted comment 가 정확히 `file:<file_name>` 이어야 한다.
/// `open` 은 검증할 내용을 처음부터 읽는 reader 를 연다 (키 id 가 맞는 키에서만 호출)
fn verify_update_signature<R: Read>(
    keys: &[&str],
    sig: &[u8],
    file_name: &str,
    open: impl Fn() -> std::io::Result<R>,
) -> Result<(), String> {
    use minisign_verify::PublicKey;
    if keys.is_empty() {
        return Err("업데이트 서명 키가 설정되지 않아 설치하지 않습니다".to_string());
    }
    let signature = decode_update_signature(sig)?;
    let mut last_err = "설정된 서명 키와 맞지 않습니다".to_string();
    let mut verified = false;
    for key in keys {
        let Ok(pk) = PublicKey::from_base64(key) else { continue };
        // 키 id 가 다르면 내용을 읽지 않고 바로 실패한다. 레거시(사전 해시 없는) 서명은 받지 않는다
        let Ok(mut verifier) = pk.verify_stream(&signature) else { continue };
        let mut reader = open().map_err(|e| e.to_string())?;
        let mut buf = vec![0u8; BUF];
        loop {
            let n = reader.read(&mut buf).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            verifier.update(&buf[..n]);
        }
        match verifier.finalize() {
            Ok(()) => {
                verified = true;
                break;
            }
            Err(e) => last_err = format!("서명이 내용과 맞지 않습니다: {e}"),
        }
    }
    if !verified {
        return Err(last_err);
    }
    // trusted comment 는 전역 서명에 포함되므로 위조할 수 없다. 다른 파일(예: 예전 버전)의 서명을 재사용하지 못하게 한다
    let expected = format!("file:{file_name}");
    if signature.trusted_comment() != expected {
        return Err(format!("서명된 파일명이 받은 파일과 다릅니다 ({} ≠ {expected})", signature.trusted_comment()));
    }
    Ok(())
}

/// 서명 파일의 크기·형식 확인 (검증 전, 내용과 무관). zip 을 받기 전에도 쓴다
fn decode_update_signature(sig: &[u8]) -> Result<minisign_verify::Signature, String> {
    if sig.len() as u64 > MAX_SIGNATURE_BYTES {
        return Err(format!("서명 파일이 크기 상한({MAX_SIGNATURE_BYTES} 바이트)을 넘습니다"));
    }
    let text = std::str::from_utf8(sig).map_err(|_| "서명 파일 형식이 올바르지 않습니다".to_string())?;
    minisign_verify::Signature::decode(text).map_err(|e| format!("서명 파일 형식이 올바르지 않습니다: {e}"))
}

/// 서명 파일을 받는다 (https 전용 업데이트 클라이언트, 크기 상한 MAX_SIGNATURE_BYTES, 전체 타임아웃 `total`)
fn fetch_update_signature(client: &Client, sig_url: &str, total: Duration) -> Result<Vec<u8>, String> {
    let resp = client.get(sig_url).timeout(total).send().map_err(|e| e.to_string())?;
    if resp.status() == StatusCode::NOT_FOUND {
        return Err("릴리스에 서명 파일이 없습니다".to_string());
    }
    if !resp.status().is_success() {
        return Err(format!("서명 파일 HTTP {}", resp.status()));
    }
    if resp.content_length().is_some_and(|n| n > MAX_SIGNATURE_BYTES) {
        return Err(format!("서명 파일이 크기 상한({MAX_SIGNATURE_BYTES} 바이트)을 넘습니다"));
    }
    read_limited(resp, MAX_SIGNATURE_BYTES)
}

/// zip 을 받아 서명을 확인한 뒤 풀고 현재 번들을 새 번들로 바꾼다. 성공하면 새 번들 경로를 돌려준다.
/// `expected_version` 은 릴리스 API 의 태그(앞의 v 제외). `--self-update` 처럼 없으면 주소의 태그를 쓴다
fn install_update(url: &str, expected_version: Option<&str>, set: &dyn Fn(UpdateState)) -> Result<PathBuf, String> {
    install_update_with_keys(url, expected_version, UPDATE_PUBLIC_KEYS, set)
}

fn install_update_with_keys(
    url: &str,
    expected_version: Option<&str>,
    keys: &[&str],
    set: &dyn Fn(UpdateState),
) -> Result<PathBuf, String> {
    // 이 리포의 공식 릴리스 파일만 설치한다 (릴리스 API 응답·--self-update 인자 모두). 우회 경로는 없다
    if !is_release_asset_url(url) {
        return Err(format!("공식 릴리스 주소가 아니라 설치하지 않습니다: {url}"));
    }
    let file_name = check_update_target(url, expected_version, VERSION)?;
    let sig_url = update_signature_url(url).ok_or_else(|| format!("서명 파일 주소가 공식 릴리스 주소가 아닙니다: {url}"))?;
    if keys.is_empty() {
        return Err(format!("업데이트 서명 키가 설정되지 않아 설치하지 않습니다. {MANUAL_UPDATE_HINT}"));
    }
    let bundle = current_bundle()?;
    // 다른 사용자·프로세스가 미리 만들어 둘 수 없는 새 폴더(임의 이름, 0700)에서만 작업한다
    let work = new_update_work_dir()?;
    // 성공·실패 모두 받은 zip·서명·압축 푼 것을 지운다
    let result = download_verify_replace(url, &sig_url, file_name, keys, &bundle, &work, set);
    let _ = std::fs::remove_dir_all(&work);
    result
}

/// 업데이트 작업 폴더: 임시 폴더 안에 `url-downloader-update-<임의 문자열>` 을 새로 만든다
fn new_update_work_dir() -> Result<PathBuf, String> {
    create_private_dir(&std::env::temp_dir(), "url-downloader-update-", random_suffix)
}

/// `parent/<prefix><suffix()>` 폴더를 권한 0700 으로 새로 만든다. 이미 있으면(폴더·파일·심볼릭 링크 모두) 쓰지 않고
/// 다른 이름으로 다시 시도한다. 기존 경로를 받아들이는 경우는 없다
fn create_private_dir(parent: &Path, prefix: &str, mut suffix: impl FnMut() -> String) -> Result<PathBuf, String> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    for _ in 0..16 {
        let dir = parent.join(format!("{prefix}{}", suffix()));
        // mkdir 은 경로가 이미 있으면(심볼릭 링크 포함) 실패한다
        match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => {
                // umask 와 무관하게 0700
                std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
                    .map_err(|e| format!("임시 폴더 권한을 설정하지 못했습니다: {e}"))?;
                return Ok(dir);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("임시 폴더를 만들지 못했습니다: {e}")),
        }
    }
    Err("임시 폴더를 새로 만들지 못했습니다 (이름이 계속 겹침)".to_string())
}

/// 임시 폴더 이름용 임의 문자열 (32자리 16진수). /dev/urandom 을 못 읽으면 OS 난수로 시드되는 RandomState 해시를 쓴다
fn random_suffix() -> String {
    let mut bytes = [0u8; 16];
    if File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut bytes)).is_err() {
        use std::hash::{BuildHasher, Hasher};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        for half in bytes.chunks_mut(8) {
            let mut h = std::collections::hash_map::RandomState::new().build_hasher();
            h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos()));
            h.write_u32(std::process::id());
            h.write_u64(COUNTER.fetch_add(1, Relaxed));
            half.copy_from_slice(&h.finish().to_le_bytes());
        }
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// 서명 파일 → zip → 검증 → 압축 풀기 → 번들 교체. 주소·버전·키 검사는 호출하는 `install_update_with_keys` 가 먼저 한다
/// (이 함수는 받은 주소를 그대로 요청하므로 공개 진입점에서 직접 부르지 않는다. 테스트는 로컬 서버 주소로 부른다)
fn download_verify_replace(
    url: &str,
    sig_url: &str,
    file_name: &str,
    keys: &[&str],
    bundle: &Path,
    work: &Path,
    set: &dyn Fn(UpdateState),
) -> Result<PathBuf, String> {
    let zip = work.join("update.zip");
    let client = update_client()?;
    const SIG_FAIL: &str = "업데이트 파일의 서명을 확인할 수 없어 설치하지 않습니다";
    let sig_fail = |e: String| format!("{SIG_FAIL}: {e}. {MANUAL_UPDATE_HINT}");

    set(UpdateState::Downloading { done: 0, total: None });
    // 서명 파일을 먼저 받아 크기·형식을 확인한다. 서명이 없는 릴리스면 큰 zip 을 받기 전에 끝난다
    let sig = fetch_update_signature(&client, sig_url, UPDATE_REQUEST_TIMEOUT).map_err(sig_fail)?;
    decode_update_signature(&sig).map_err(sig_fail)?;

    // zip 은 전체 시간 제한 없이, 멈춤 타임아웃(UPDATE_STALL_TIMEOUT)만으로 받는다
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

    // 검증 전에는 zip 을 풀거나 실행하지 않는다
    verify_update_signature(keys, &sig, file_name, || File::open(&zip)).map_err(sig_fail)?;

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
    std::fs::rename(bundle, &backup).map_err(|e| format!("기존 앱을 옮기지 못했습니다: {e}"))?;
    if let Err(e) = run("ditto", &[&new_app.to_string_lossy(), &bundle.to_string_lossy()]) {
        let _ = std::fs::rename(&backup, bundle); // 실패하면 원복
        return Err(e);
    }
    let _ = std::fs::remove_dir_all(&backup);
    Ok(bundle.to_path_buf())
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
/// S3 항목은 키의 마지막 조각이 원본 파일명이다 (키의 ?·#·% 는 주소 문법이 아니라 이름의 일부)
fn target_name(e: &Entry) -> String {
    let original = match s3_parts(&e.url) {
        Some((_, key)) => file_name_from_key(key, e.line),
        None => file_name_from_url(&e.url, e.line),
    };
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

fn file_name_from_key(key: &str, idx: usize) -> String {
    let cleaned = sanitize(key.rsplit('/').next().unwrap_or(""));
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
        // 이어받기로 완료한 파일에도 격리 표시가 붙는다
        let q = quarantine_of(&dir.join("big.bin")).expect("이어받아 완료한 파일에 격리 표시가 없다");
        assert!(q.starts_with("0081;"), "격리 표시 값: {q}");
    }

    /// `xattr -p com.apple.quarantine` 결과 (없으면 None)
    pub(super) fn quarantine_of(path: &Path) -> Option<String> {
        let out = std::process::Command::new("/usr/bin/xattr").arg("-p").arg("com.apple.quarantine").arg(path).output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    // 받은 최종 파일에는 브라우저로 받은 파일처럼 격리 표시(com.apple.quarantine)를 단다. 받는 중(.part)에는 달지 않는다
    #[test]
    fn finished_download_gets_quarantine_attribute() {
        let data = big_data();
        let (url, _) = serve(Arc::clone(&data), 0);
        let dir = tmp("quarantine");
        let shared = item(&url, "big.bin", &dir);
        download(Arc::clone(&shared), 0, 4);
        assert!(matches!(shared.lock().unwrap()[0].status, Status::Finished));
        let q = quarantine_of(&dir.join("big.bin")).expect("완료한 파일에 격리 표시가 없다");
        assert!(q.starts_with("0081;"), "격리 표시 값: {q}");
        assert!(q.ends_with(";URL Downloader;"), "격리 표시 값: {q}");

        // Range 를 무시하는 서버(한 줄기로 받기)도 같다
        let url = serve_no_range_pub(Arc::clone(&data), 0);
        let dir = tmp("quarantine_whole");
        let shared = item(&url, "big.bin", &dir);
        download(Arc::clone(&shared), 0, 4);
        assert!(matches!(shared.lock().unwrap()[0].status, Status::Finished));
        assert!(quarantine_of(&dir.join("big.bin")).is_some_and(|q| q.starts_with("0081;")));
    }

    #[test]
    fn quarantine_value_format() {
        assert_eq!(quarantine_value(0x5f8a_1b2c), "0081;5f8a1b2c;URL Downloader;");
        assert_eq!(quarantine_value(0x10), "0081;00000010;URL Downloader;");
    }
}

#[cfg(test)]
mod security_tests {
    use super::*;
    use std::io::BufRead;
    use std::net::{Shutdown, TcpListener};
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

    const GOOD: &str = "https://github.com/bluelang1/url-downloader/releases/download/v0.6.0/URL-Downloader-v0.6.0-macos-universal.zip";

    #[test]
    fn release_asset_url_accepts_only_this_repo_release_downloads() {
        assert!(is_release_asset_url(GOOD));
        for bad in [
            // http
            "http://github.com/bluelang1/url-downloader/releases/download/v0.6.0/x.zip",
            // 다른 호스트
            "https://evil.example/bluelang1/url-downloader/releases/download/v0.6.0/x.zip",
            "https://github.com.evil.example/bluelang1/url-downloader/releases/download/v0.6.0/x.zip",
            "https://objects.githubusercontent.com/bluelang1/url-downloader/releases/download/v0.6.0/x.zip",
            "https://github.com:8443/bluelang1/url-downloader/releases/download/v0.6.0/x.zip",
            "https://user@github.com/bluelang1/url-downloader/releases/download/v0.6.0/x.zip",
            // 다른 리포·다른 경로
            "https://github.com/someone/url-downloader/releases/download/v0.6.0/x.zip",
            "https://github.com/bluelang1/url-downloader-fork/releases/download/v0.6.0/x.zip",
            "https://github.com/bluelang1/url-downloader/archive/refs/tags/v0.6.0.zip",
            "https://github.com/bluelang1/url-downloader/releases/download/",
            // 정규화하면 다른 경로로 빠지는 주소
            "https://github.com/bluelang1/url-downloader/releases/download/../../../../evil/x/releases/download/v1/x.zip",
            "https://github.com/bluelang1/url-downloader/releases/download/%2e%2e/%2e%2e/%2e%2e/%2e%2e/evil/x.zip",
            "",
            "not a url",
        ] {
            assert!(!is_release_asset_url(bad), "거부해야 한다: {bad}");
        }
    }

    // 검증을 통과하지 못한 주소는 내려받기 전에 사용자에게 보이는 오류로 거부한다
    #[test]
    fn install_update_rejects_untrusted_url_before_downloading() {
        let (base, hits) = serve_redirect(None);
        let started = AtomicUsize::new(0);
        for url in [format!("{base}/update.zip"), "https://evil.example/x.zip".to_string()] {
            let err = install_update(&url, None, &|_| {
                started.fetch_add(1, SeqCst);
            })
            .unwrap_err();
            assert!(err.contains("공식 릴리스"), "오류 문구: {err}");
        }
        assert_eq!(started.load(SeqCst), 0, "내려받기를 시작하면 안 된다");
        assert_eq!(hits.load(SeqCst), 0, "요청을 보내면 안 된다");
    }

    #[test]
    fn update_redirects_follow_only_https() {
        assert!(redirect_allowed("https", 0));
        assert!(redirect_allowed("https", 9));
        assert!(!redirect_allowed("https", 10), "리다이렉트 횟수 상한");
        assert!(!redirect_allowed("http", 0));
        assert!(!redirect_allowed("file", 0));

        // 실제 클라이언트: http 로 리다이렉트하면 따라가지 않고 오류
        let (target, target_hits) = serve_redirect(None);
        let (base, _) = serve_redirect(Some(format!("{target}/next.zip")));
        let res = update_client().unwrap().get(format!("{base}/start.zip")).send();
        assert!(res.is_err(), "http 리다이렉트를 따라가면 안 된다");
        assert_eq!(target_hits.load(SeqCst), 0);
    }

    /// 요청을 세고, `location` 이 있으면 302 로 보내고 없으면 404 를 준다
    fn serve_redirect(location: Option<String>) -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits = Arc::new(AtomicUsize::new(0));
        let h = Arc::clone(&hits);
        thread::spawn(move || {
            for mut s in listener.incoming().flatten() {
                h.fetch_add(1, SeqCst);
                let mut reader = std::io::BufReader::new(s.try_clone().unwrap());
                let mut line = String::new();
                while reader.read_line(&mut line).unwrap_or(0) > 0 && !line.trim().is_empty() {
                    line.clear();
                }
                let resp = match &location {
                    Some(l) => format!("HTTP/1.1 302 Found\r\nLocation: {l}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"),
                    None => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(),
                };
                let _ = s.write_all(resp.as_bytes());
                let _ = s.shutdown(Shutdown::Both);
            }
        });
        (format!("http://127.0.0.1:{port}"), hits)
    }

    // ---- 업데이트 서명 검증 ----
    // TEST ONLY: 아래 키·서명은 테스트 전용으로 비밀번호 없이(minisign -G -W) 만든 일회용 키쌍의 결과물이다.
    // 비밀키는 픽스처를 만든 뒤 지웠고 리포에 없다. 실제 업데이트 서명 키와 무관하다
    const TEST_KEY_A: &str = "RWRKAj7DZaiYzIAjzewtqiWPpSiuGWXP0i7B+RZsnGCAV6kalapfVtz/";
    const TEST_KEY_B: &str = "RWSXs1W7zMNYFjecDkGWzG3KDxvteWAXNZto93T9oS5ex3UD3/jF6lCg";
    const TEST_FILE: &str = "URL-Downloader-v0.6.0-macos-universal.zip";
    const TEST_DATA: &[u8] = b"TEST ONLY update payload\n";
    // TEST ONLY: TEST_DATA 를 키 A 로 서명 (trusted comment: file:URL-Downloader-v0.6.0-macos-universal.zip)
    const TEST_SIG_A: &str = "untrusted comment: signature from minisign secret key
RURKAj7DZaiYzIe5A6tUxx5T++u8sqZk/E1JuYQ5AmmHnnGJMa7BIdltRmsFhYUv4rYQ+ItL3bg3mauTh1KZaJhuDCW190iLTQ8=
trusted comment: file:URL-Downloader-v0.6.0-macos-universal.zip
Q/K+ZpC0M0Bf3q+cw5bXoggGXWNedVwBh+SmLnt7NphJgCVzhe5oscvUTejcpL8hVZpq1HzOxbjHsA94+HW5AQ==
";
    // TEST ONLY: 같은 TEST_DATA 를 키 B 로 서명 (trusted comment 동일)
    const TEST_SIG_B: &str = "untrusted comment: signature from minisign secret key
RUSXs1W7zMNYFjwTR6Mwv4ZrZr6FREAM4OEeSMFbzTZluqNhqF2PlUQlaRgCA5zZi3diSG/G77+ZCkKY4gxbFyT6mpQhHERcZQs=
trusted comment: file:URL-Downloader-v0.6.0-macos-universal.zip
wok80NKOs9bx2ACBaXBDj6RApPAnO88LAdqCEPNMY6TGvjUflvzU14IqV2io9nsgNA402XAm+LAzeNX29NqnCQ==
";

    fn verify(keys: &[&str], sig: &[u8], file: &str, data: &[u8]) -> Result<(), String> {
        verify_update_signature(keys, sig, file, || Ok(data))
    }

    #[test]
    fn update_signature_accepts_valid_signature() {
        assert_eq!(verify(&[TEST_KEY_A], TEST_SIG_A.as_bytes(), TEST_FILE, TEST_DATA), Ok(()));
    }

    #[test]
    fn update_signature_rejects_tampered_content() {
        let mut data = TEST_DATA.to_vec();
        data[0] ^= 1;
        assert!(verify(&[TEST_KEY_A], TEST_SIG_A.as_bytes(), TEST_FILE, &data).is_err());
        // 끝에 한 바이트 덧붙여도 거부
        let mut longer = TEST_DATA.to_vec();
        longer.push(b'x');
        assert!(verify(&[TEST_KEY_A], TEST_SIG_A.as_bytes(), TEST_FILE, &longer).is_err());
    }

    #[test]
    fn update_signature_rejects_other_key() {
        assert!(verify(&[TEST_KEY_B], TEST_SIG_A.as_bytes(), TEST_FILE, TEST_DATA).is_err());
        assert!(verify(&[TEST_KEY_A], TEST_SIG_B.as_bytes(), TEST_FILE, TEST_DATA).is_err());
    }

    #[test]
    fn update_signature_accepts_any_listed_key() {
        assert_eq!(verify(&[TEST_KEY_A, TEST_KEY_B], TEST_SIG_B.as_bytes(), TEST_FILE, TEST_DATA), Ok(()));
        assert_eq!(verify(&[TEST_KEY_A, TEST_KEY_B], TEST_SIG_A.as_bytes(), TEST_FILE, TEST_DATA), Ok(()));
        // 깨진 키가 섞여 있어도 나머지 키로 확인한다
        assert_eq!(verify(&["not-a-key", TEST_KEY_B], TEST_SIG_B.as_bytes(), TEST_FILE, TEST_DATA), Ok(()));
        assert!(verify(&["not-a-key"], TEST_SIG_B.as_bytes(), TEST_FILE, TEST_DATA).is_err());
    }

    #[test]
    fn update_signature_rejects_when_no_keys_configured() {
        let err = verify(&[], TEST_SIG_A.as_bytes(), TEST_FILE, TEST_DATA).unwrap_err();
        assert!(err.contains("서명 키"), "오류 문구: {err}");
        // 배포 빌드의 키 목록이 비어 있으면 실제 설치 경로도 닫힌 쪽으로 실패한다
        if UPDATE_PUBLIC_KEYS.is_empty() {
            let err = verify(UPDATE_PUBLIC_KEYS, TEST_SIG_A.as_bytes(), TEST_FILE, TEST_DATA).unwrap_err();
            assert!(err.contains("서명 키"), "오류 문구: {err}");
        }
    }

    #[test]
    fn update_signature_rejects_trusted_comment_for_other_file() {
        // 서명은 유효하지만 다른 파일명(예: 예전 버전 zip)으로 받은 경우
        let err = verify(&[TEST_KEY_A], TEST_SIG_A.as_bytes(), "URL-Downloader-v0.5.0-macos-universal.zip", TEST_DATA).unwrap_err();
        assert!(err.contains("파일명"), "오류 문구: {err}");
        // trusted comment 를 고쳐 쓰면 전역 서명이 맞지 않아 거부된다
        let forged = TEST_SIG_A.replace("file:URL-Downloader-v0.6.0", "file:URL-Downloader-v0.7.0");
        assert!(verify(&[TEST_KEY_A], forged.as_bytes(), "URL-Downloader-v0.7.0-macos-universal.zip", TEST_DATA).is_err());
    }

    #[test]
    fn update_signature_rejects_broken_or_oversized_signature() {
        for bad in ["", "garbage", "untrusted comment: x\nnot base64\n", &TEST_SIG_A[..TEST_SIG_A.len() / 2]] {
            assert!(verify(&[TEST_KEY_A], bad.as_bytes(), TEST_FILE, TEST_DATA).is_err(), "거부해야 한다: {bad:?}");
        }
        // 서명 한 글자 변조
        let flipped = TEST_SIG_A.replacen("Q/K+", "Q/K-", 1);
        assert!(verify(&[TEST_KEY_A], flipped.as_bytes(), TEST_FILE, TEST_DATA).is_err());
        // 유효한 서명이라도 크기 상한을 넘으면 거부
        let mut big = TEST_SIG_A.as_bytes().to_vec();
        big.resize(MAX_SIGNATURE_BYTES as usize + 1, b'\n');
        let err = verify(&[TEST_KEY_A], &big, TEST_FILE, TEST_DATA).unwrap_err();
        assert!(err.contains("크기"), "오류 문구: {err}");
        // 내려받을 때도 상한 + 1 바이트까지만 읽고 거부
        assert_eq!(read_limited(&[7u8; 10][..], 10).unwrap(), vec![7u8; 10]);
        assert!(read_limited(&[7u8; 11][..], 10).is_err());
        assert!(read_limited(std::io::repeat(0), MAX_SIGNATURE_BYTES).is_err(), "끝없는 응답도 멈춘다");
        // 내용을 읽다 실패하면 거부
        let err = verify_update_signature(&[TEST_KEY_A], TEST_SIG_A.as_bytes(), TEST_FILE, || {
            Err::<&[u8], _>(std::io::Error::other("disk"))
        });
        assert!(err.is_err());
    }

    const V: &str = "https://github.com/bluelang1/url-downloader/releases/download";

    #[test]
    fn update_target_accepts_matching_newer_release() {
        let url = format!("{V}/v0.6.0/URL-Downloader-v0.6.0-macos-universal.zip");
        assert_eq!(check_update_target(&url, Some("0.6.0"), "0.5.0"), Ok(TEST_FILE));
        assert_eq!(check_update_target(&url, None, "0.5.9"), Ok(TEST_FILE));
        assert_eq!(update_signature_url(&url).as_deref(), Some(format!("{url}.minisig").as_str()));
    }

    #[test]
    fn update_target_rejects_tag_url_or_file_version_mismatch() {
        let good = format!("{V}/v0.6.0/URL-Downloader-v0.6.0-macos-universal.zip");
        // 릴리스 API 의 태그와 주소의 태그가 다름
        assert!(check_update_target(&good, Some("0.7.0"), "0.5.0").is_err());
        assert!(check_update_target(&good, Some("v0.6.0"), "0.5.0").is_err());
        for bad in [
            // 주소의 태그와 파일명 버전이 다름
            format!("{V}/v0.6.0/URL-Downloader-v0.5.0-macos-universal.zip"),
            format!("{V}/v0.6.0/URL-Downloader-v0.6.01-macos-universal.zip"),
            format!("{V}/v0.6.0/URL-Downloader-v0.6.0.zip"),
            // 태그 형식이 아님
            format!("{V}/0.6.0/URL-Downloader-0.6.0-macos-universal.zip"),
            // zip 이 아님 / 경로 조각 수가 다름 / 쿼리·조각
            format!("{V}/v0.6.0/URL-Downloader-v0.6.0-macos-universal.tar"),
            format!("{V}/v0.6.0/x/URL-Downloader-v0.6.0-macos-universal.zip"),
            format!("{V}/v0.6.0/"),
            format!("{V}/v0.6.0/URL-Downloader-v0.6.0-macos-universal.zip?x=.zip"),
            format!("{V}/v0.6.0/URL-Downloader-v0.6.0-macos-universal.zip#a.zip"),
            "https://evil.example/v0.6.0/URL-Downloader-v0.6.0-macos-universal.zip".to_string(),
        ] {
            assert!(check_update_target(&bad, None, "0.5.0").is_err(), "거부해야 한다: {bad}");
        }
    }

    #[test]
    fn update_target_rejects_same_or_older_version() {
        let url = format!("{V}/v0.6.0/URL-Downloader-v0.6.0-macos-universal.zip");
        for current in ["0.6.0", "0.6.1", "0.7.0", "1.0.0"] {
            let err = check_update_target(&url, None, current).unwrap_err();
            assert!(err.contains("현재 버전"), "{current}: {err}");
            assert!(check_update_target(&url, Some("0.6.0"), current).is_err());
        }
    }

    // 서명 키가 없으면 앱 번들 확인·내려받기 전에 거부한다
    #[test]
    fn install_update_refuses_without_keys_before_downloading() {
        let url = format!("{V}/v99.0.0/URL-Downloader-v99.0.0-macos-universal.zip");
        let started = AtomicUsize::new(0);
        let err = install_update_with_keys(&url, Some("99.0.0"), &[], &|_| {
            started.fetch_add(1, SeqCst);
        })
        .unwrap_err();
        assert!(err.contains("서명 키"), "오류 문구: {err}");
        // 버전 확인도 내려받기 전에
        let old = format!("{V}/v0.0.1/URL-Downloader-v0.0.1-macos-universal.zip");
        let err = install_update_with_keys(&old, None, &[TEST_KEY_A], &|_| {
            started.fetch_add(1, SeqCst);
        })
        .unwrap_err();
        assert!(err.contains("현재 버전"), "오류 문구: {err}");
        assert_eq!(started.load(SeqCst), 0, "내려받기를 시작하면 안 된다");
    }

    // 릴리스 전용 검사 (Release 워크플로·scripts/release.sh 가 `-- --ignored` 로 실행). 평소 테스트에서는 건너뛴다.
    // 키가 비어 있거나 하나뿐이면 릴리스하지 않는다: 주 키를 잃어도 예비 키로 다음 업데이트를 낼 수 있어야 한다
    #[test]
    #[ignore = "릴리스 전용: cargo test --locked release_keys_are_configured -- --ignored"]
    fn release_keys_are_configured() {
        use minisign_verify::PublicKey;
        assert!(
            UPDATE_PUBLIC_KEYS.len() >= 2,
            "UPDATE_PUBLIC_KEYS(src/main.rs)에 주 키·예비 키를 2개 이상 넣어야 릴리스할 수 있습니다 (지금 {}개)",
            UPDATE_PUBLIC_KEYS.len()
        );
        let keys: Vec<PublicKey> = UPDATE_PUBLIC_KEYS
            .iter()
            .map(|k| PublicKey::from_base64(k).unwrap_or_else(|e| panic!("UPDATE_PUBLIC_KEYS 의 공개키를 읽지 못했습니다 ({k}): {e}")))
            .collect();
        for (i, a) in keys.iter().enumerate() {
            for b in &keys[i + 1..] {
                assert!(a != b, "UPDATE_PUBLIC_KEYS 에 같은 키가 두 번 들어 있습니다");
            }
        }
    }

    // ---- 릴리스 API: 서명 파일이 함께 있는 릴리스만 업데이트로 제안 ----
    fn release_json(tag: &str, assets: &[&str]) -> serde_json::Value {
        let assets: Vec<serde_json::Value> = assets
            .iter()
            .map(|n| serde_json::json!({ "name": n, "browser_download_url": format!("{V}/{tag}/{n}") }))
            .collect();
        serde_json::json!({ "tag_name": tag, "assets": assets })
    }

    #[test]
    fn latest_release_requires_zip_and_matching_signature() {
        let zip = "URL-Downloader-v0.6.0-macos-universal.zip";
        let sig = "URL-Downloader-v0.6.0-macos-universal.zip.minisig";
        assert_eq!(
            latest_release_from_json(&release_json("v0.6.0", &[zip, sig]), "0.5.0"),
            Some(("0.6.0".to_string(), format!("{V}/v0.6.0/{zip}")))
        );
        // 순서가 달라도 짝을 찾는다
        assert!(latest_release_from_json(&release_json("v0.6.0", &[sig, zip]), "0.5.0").is_some());
        // 서명이 아직 없음(서명 전 상태) / 다른 파일의 서명 / zip 없음 → 최신 상태와 같은 취급
        for assets in [
            vec![zip],
            vec![zip, "URL-Downloader-v0.5.0-macos-universal.zip.minisig"],
            vec![zip, "URL-Downloader-v0.6.0-macos-universal.minisig"],
            vec![sig],
            vec![],
        ] {
            assert_eq!(latest_release_from_json(&release_json("v0.6.0", &assets), "0.5.0"), None, "{assets:?}");
        }
        // 서명된 zip 이 여러 개면 서명이 있는 쪽을 고른다
        let other = "URL-Downloader-v0.6.0-macos-arm64.zip";
        assert_eq!(
            latest_release_from_json(&release_json("v0.6.0", &[other, zip, sig]), "0.5.0").map(|(_, u)| u),
            Some(format!("{V}/v0.6.0/{zip}"))
        );
        // 현재 버전 이하이면 서명이 있어도 None
        assert_eq!(latest_release_from_json(&release_json("v0.6.0", &[zip, sig]), "0.6.0"), None);
    }

    // ---- 업데이트 임시 폴더 ----
    #[test]
    fn update_work_dir_is_new_random_and_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let a = new_update_work_dir().unwrap();
        let b = new_update_work_dir().unwrap();
        assert_ne!(a, b, "매번 다른 이름이어야 한다");
        for d in [&a, &b] {
            let name = d.file_name().unwrap().to_string_lossy().to_string();
            assert!(name.starts_with("url-downloader-update-"), "{name}");
            assert_ne!(name, format!("url-downloader-update-{}", std::process::id()), "pid 로 예측할 수 있으면 안 된다");
            let meta = std::fs::symlink_metadata(d).unwrap();
            assert!(meta.is_dir());
            assert_eq!(meta.permissions().mode() & 0o777, 0o700, "{}", d.display());
            assert_eq!(std::fs::read_dir(d).unwrap().count(), 0, "새로 만든 빈 폴더여야 한다");
        }
        let _ = std::fs::remove_dir_all(&a);
        let _ = std::fs::remove_dir_all(&b);
    }

    #[test]
    fn private_dir_never_reuses_existing_path() {
        let parent = std::env::temp_dir().join(format!("ud_privdir_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&parent);
        std::fs::create_dir_all(&parent).unwrap();
        // 이미 있는 폴더, 다른 곳을 가리키는 심볼릭 링크는 건너뛰고 건드리지 않는다
        std::fs::create_dir(parent.join("p-taken")).unwrap();
        std::fs::write(parent.join("p-taken/keep.txt"), b"keep").unwrap();
        let elsewhere = parent.join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, parent.join("p-link")).unwrap();
        let mut names = vec!["taken", "link", "fresh"].into_iter();
        let dir = create_private_dir(&parent, "p-", || names.next().unwrap().to_string()).unwrap();
        assert_eq!(dir, parent.join("p-fresh"));
        assert_eq!(std::fs::read(parent.join("p-taken/keep.txt")).unwrap(), b"keep");
        assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 0);
        // 계속 겹치면 기존 폴더를 쓰지 않고 실패한다
        assert!(create_private_dir(&parent, "p-", || "taken".to_string()).is_err());
        let _ = std::fs::remove_dir_all(&parent);
    }

    // ---- 내려받기 → 검증 → 교체 순서 (로컬 서버, 임시 번들) ----
    // TEST ONLY: testdata/update-test.zip(New.app/Contents/marker.txt 하나)을 비밀번호 없는 일회용 키로 서명한 것.
    // 비밀키는 서명 직후 지웠고 리포에 없다. 실제 업데이트 서명 키와 무관하다
    const TEST_KEY_ZIP: &str = "RWRYwUrVRiA9SgFGG8bNfY3Gi1B2hoBDh1msalNucnEMtlfc2a4Xn1ys";
    const TEST_ZIP: &[u8] = include_bytes!("../testdata/update-test.zip");
    const TEST_ZIP_SIG: &str = "untrusted comment: signature from minisign secret key
RURYwUrVRiA9StBfdntD0PhPXBombhIIuX9cYWmbfRyUgvXAf3vrf7TLYO1Bn+3333gS/B9HyzO5NMgiYR/M9COzWviGw6OqRQY=
trusted comment: file:URL-Downloader-v0.6.0-macos-universal.zip
W6qHkE+RLhdmmhbWdVNyk+7dC884pEubSvte5CWAvu/c+I0zQX3abo3Qu1NCalW7iRBHbfKR0iVuixcLPuNJDA==
";

    /// 경로별로 정해진 상태·본문을 주는 서버. 요청한 경로를 순서대로 기록한다
    fn serve_routes(routes: Vec<(&'static str, u16, Vec<u8>)>) -> (String, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let log = Arc::new(Mutex::new(Vec::new()));
        let l = Arc::clone(&log);
        thread::spawn(move || {
            for mut s in listener.incoming().flatten() {
                let mut reader = std::io::BufReader::new(s.try_clone().unwrap());
                let mut first = String::new();
                let _ = reader.read_line(&mut first);
                let path = first.split_whitespace().nth(1).unwrap_or("").to_string();
                let mut line = String::new();
                while reader.read_line(&mut line).unwrap_or(0) > 0 && !line.trim().is_empty() {
                    line.clear();
                }
                l.lock().unwrap().push(path.clone());
                let (status, body) = routes
                    .iter()
                    .find(|(p, _, _)| *p == path)
                    .map(|(_, st, b)| (*st, b.clone()))
                    .unwrap_or((404, Vec::new()));
                let head = format!("HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                let _ = s.write_all(head.as_bytes());
                let _ = s.write_all(&body);
                let _ = s.shutdown(Shutdown::Both);
            }
        });
        (format!("http://127.0.0.1:{port}"), log)
    }

    struct Sandbox {
        root: PathBuf,
        bundle: PathBuf,
        work: PathBuf,
    }

    /// 임시 폴더에 기존 번들(`URL Downloader.app/Contents/marker.txt` = old)과 빈 작업 폴더를 만든다
    fn sandbox(tag: &str) -> Sandbox {
        let root = std::env::temp_dir().join(format!("ud_dvr_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let bundle = root.join("URL Downloader.app");
        std::fs::create_dir_all(bundle.join("Contents")).unwrap();
        std::fs::write(bundle.join("Contents/marker.txt"), b"old bundle\n").unwrap();
        let work = root.join("work");
        std::fs::create_dir(&work).unwrap();
        Sandbox { root, bundle, work }
    }

    impl Sandbox {
        fn marker(&self) -> Vec<u8> {
            std::fs::read(self.bundle.join("Contents/marker.txt")).unwrap()
        }
        fn assert_untouched(&self) {
            assert_eq!(self.marker(), b"old bundle\n", "기존 번들이 그대로여야 한다");
            assert!(!self.work.join("extracted").exists(), "압축을 풀면 안 된다");
            assert!(!self.bundle.with_extension("app.old").exists());
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn run_dvr(sb: &Sandbox, base: &str, file_name: &str, keys: &[&str]) -> Result<PathBuf, String> {
        download_verify_replace(
            &format!("{base}/update.zip"),
            &format!("{base}/update.zip.minisig"),
            file_name,
            keys,
            &sb.bundle,
            &sb.work,
            &|_| {},
        )
    }

    #[test]
    fn update_signature_missing_stops_before_zip_download() {
        let sb = sandbox("nosig");
        let (base, log) = serve_routes(vec![("/update.zip", 200, TEST_ZIP.to_vec())]);
        let err = run_dvr(&sb, &base, TEST_FILE, &[TEST_KEY_ZIP]).unwrap_err();
        assert!(err.contains("서명 파일이 없습니다"), "오류 문구: {err}");
        assert!(err.contains("릴리스 페이지"), "직접 받는 안내가 있어야 한다: {err}");
        assert_eq!(*log.lock().unwrap(), vec!["/update.zip.minisig".to_string()], "서명이 없으면 zip 을 받지 않는다");
        assert!(!sb.work.join("update.zip").exists());
        sb.assert_untouched();
    }

    #[test]
    fn update_signature_unparsable_or_oversized_stops_before_zip_download() {
        let big = vec![b'a'; MAX_SIGNATURE_BYTES as usize + 1];
        for sig in [b"not a signature".to_vec(), big] {
            let sb = sandbox("badfmt");
            let (base, log) = serve_routes(vec![("/update.zip", 200, TEST_ZIP.to_vec()), ("/update.zip.minisig", 200, sig)]);
            let err = run_dvr(&sb, &base, TEST_FILE, &[TEST_KEY_ZIP]).unwrap_err();
            assert!(err.contains("릴리스 페이지"), "오류 문구: {err}");
            assert_eq!(*log.lock().unwrap(), vec!["/update.zip.minisig".to_string()], "zip 을 받으면 안 된다");
            sb.assert_untouched();
        }
    }

    #[test]
    fn update_wrong_signature_keeps_bundle_and_does_not_extract() {
        // 형식은 맞지만 다른 내용의 서명 (키는 맞음) / 모르는 키의 서명
        for (sig, keys) in [(TEST_SIG_A, &[TEST_KEY_A][..]), (TEST_ZIP_SIG, &[TEST_KEY_A][..])] {
            let sb = sandbox("wrongsig");
            let (base, log) = serve_routes(vec![
                ("/update.zip", 200, TEST_ZIP.to_vec()),
                ("/update.zip.minisig", 200, sig.as_bytes().to_vec()),
            ]);
            let err = run_dvr(&sb, &base, TEST_FILE, keys).unwrap_err();
            assert!(err.contains("서명을 확인할 수 없어"), "오류 문구: {err}");
            assert!(err.contains("릴리스 페이지"), "오류 문구: {err}");
            assert_eq!(log.lock().unwrap().first().map(String::as_str), Some("/update.zip.minisig"), "서명을 먼저 받는다");
            sb.assert_untouched();
        }
    }

    #[test]
    fn update_signature_for_other_file_name_keeps_bundle() {
        let sb = sandbox("othername");
        let (base, _) = serve_routes(vec![
            ("/update.zip", 200, TEST_ZIP.to_vec()),
            ("/update.zip.minisig", 200, TEST_ZIP_SIG.as_bytes().to_vec()),
        ]);
        let err = run_dvr(&sb, &base, "URL-Downloader-v0.7.0-macos-universal.zip", &[TEST_KEY_ZIP]).unwrap_err();
        assert!(err.contains("파일명"), "오류 문구: {err}");
        sb.assert_untouched();
    }

    #[test]
    fn update_with_valid_signature_replaces_bundle() {
        let sb = sandbox("good");
        let (base, log) = serve_routes(vec![
            ("/update.zip", 200, TEST_ZIP.to_vec()),
            ("/update.zip.minisig", 200, TEST_ZIP_SIG.as_bytes().to_vec()),
        ]);
        let out = run_dvr(&sb, &base, TEST_FILE, &[TEST_KEY_ZIP]).unwrap();
        assert_eq!(out, sb.bundle);
        assert_eq!(sb.marker(), b"TEST ONLY new bundle\n", "새 번들로 바뀌어야 한다");
        assert!(!sb.bundle.with_extension("app.old").exists(), "치운 기존 번들은 지운다");
        assert_eq!(*log.lock().unwrap(), vec!["/update.zip.minisig".to_string(), "/update.zip".to_string()]);
    }

    // ---- 타임아웃 (설치된 reqwest blocking 의 동작을 확인) ----
    /// 헤더를 보낸 뒤 본문 10 바이트를 `gaps_ms` 간격으로 한 바이트씩 보낸다
    fn serve_drip(gaps_ms: Vec<u64>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            for mut s in listener.incoming().flatten() {
                let gaps = gaps_ms.clone();
                thread::spawn(move || {
                    let mut reader = std::io::BufReader::new(s.try_clone().unwrap());
                    let mut line = String::new();
                    while reader.read_line(&mut line).unwrap_or(0) > 0 && !line.trim().is_empty() {
                        line.clear();
                    }
                    let _ = s.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", gaps.len()).as_bytes());
                    let _ = s.flush();
                    for g in gaps {
                        thread::sleep(Duration::from_millis(g));
                        if s.write_all(b"x").and_then(|_| s.flush()).is_err() {
                            return;
                        }
                    }
                    let _ = s.shutdown(Shutdown::Both);
                });
            }
        });
        format!("http://127.0.0.1:{port}")
    }

    fn read_all(client: &Client, url: &str) -> Result<usize, String> {
        let mut resp = client.get(url).send().map_err(|e| e.to_string())?;
        let mut buf = Vec::new();
        resp.read_to_end(&mut buf).map_err(|e| e.to_string())?;
        Ok(buf.len())
    }

    #[test]
    fn update_zip_download_uses_stall_timeout_not_total() {
        let client = update_client_with(Duration::from_secs(5), Duration::from_millis(500)).unwrap();
        // 꾸준히 오면 정지 타임아웃(0.5초)보다 오래 걸려도(약 1초) 끝까지 받는다
        assert_eq!(read_all(&client, &serve_drip(vec![100; 10])), Ok(10));
        // 중간에 2초 멈추면 실패한다
        let mut gaps = vec![50; 10];
        gaps[3] = 2000;
        let started = Instant::now();
        assert!(read_all(&client, &serve_drip(gaps)).is_err(), "멈춘 연결은 끊어야 한다");
        assert!(started.elapsed() < Duration::from_millis(1800), "정지 타임아웃에서 끊어야 한다: {:?}", started.elapsed());
    }

    #[test]
    fn update_signature_fetch_uses_total_timeout() {
        let client = update_client_with(Duration::from_secs(5), Duration::from_secs(5)).unwrap();
        // 한 바이트씩 꾸준히 와도(정지 없음) 전체 시간이 넘으면 실패한다
        let url = format!("{}/x.minisig", serve_drip(vec![100; 10]));
        let started = Instant::now();
        assert!(fetch_update_signature(&client, &url, Duration::from_millis(400)).is_err());
        assert!(started.elapsed() < Duration::from_millis(900), "전체 타임아웃에서 끊어야 한다: {:?}", started.elapsed());
        assert_eq!(fetch_update_signature(&client, &url, Duration::from_secs(5)).map(|b| b.len()), Ok(10));
    }

    #[test]
    fn install_update_errors_point_to_release_page() {
        let url = format!("{V}/v99.0.0/URL-Downloader-v99.0.0-macos-universal.zip");
        let err = install_update_with_keys(&url, Some("99.0.0"), &[], &|_| {}).unwrap_err();
        assert!(err.contains("서명 키") && err.contains("릴리스 페이지"), "오류 문구: {err}");
    }

    // 알림 문구는 AppleScript 문자열 안에 들어간다: 역슬래시를 먼저 두 배로, 큰따옴표는 작은따옴표로
    #[test]
    fn applescript_text_escapes_backslash_and_quote() {
        assert_eq!(applescript_text("plain"), "plain");
        assert_eq!(applescript_text(r"C:\dir\file"), r"C:\\dir\\file");
        assert_eq!(applescript_text(r#"say "hi""#), "say 'hi'");
        // 끝의 역슬래시가 닫는 따옴표를 무력화하지 못한다
        assert_eq!(applescript_text(r"abc\"), r"abc\\");
        assert_eq!(applescript_text(r#"\" & (do shell script "x") & ""#), r#"\\' & (do shell script 'x') & '"#);
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

#[cfg(test)]
mod s3_tests {
    use super::*;
    use std::io::BufRead;
    use std::net::TcpListener;

    const ID: &str = "s3://example-bucket/data/big.bin";

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ud_s3_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn data() -> Arc<Vec<u8>> {
        Arc::new((0..3_500_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect())
    }

    /// 부를 때마다 서명이 다른 주소를 발급하는 가짜 presign. (받은 식별자, 발급한 주소) 를 기록한다
    fn fake_presign(base: &str) -> (Presign, Arc<Mutex<Vec<(String, String)>>>) {
        let log: Arc<Mutex<Vec<(String, String)>>> = Default::default();
        let (base, l) = (base.to_string(), Arc::clone(&log));
        let p: Presign = Arc::new(move |id: &str| {
            let mut l = l.lock().unwrap();
            let url = format!("{base}?X-Amz-Signature=sig{}&X-Amz-Security-Token=TOPSECRETTOKEN", l.len());
            l.push((id.to_string(), url.clone()));
            Ok(url)
        });
        (p, log)
    }

    fn s3_item(dir: &Path, name: &str, presign: Presign) -> Shared {
        let mut it = Item::new(1, ID.into(), name.into(), dir.to_path_buf());
        it.presign = Some(presign);
        Arc::new(Mutex::new(vec![it]))
    }

    fn settled(items: &[Item]) -> bool {
        !items.iter().any(|i| matches!(i.status, Status::Waiting | Status::Downloading { .. }))
    }

    fn wait_settled(shared: &Shared) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while !settled(&shared.lock().unwrap()) {
            assert!(Instant::now() < deadline, "시간 초과");
            thread::sleep(Duration::from_millis(5));
        }
    }

    // (i) S3 항목은 발급된 주소로 받는다. URL 항목과 섞여 있어도 URL 항목은 발급기를 부르지 않는다
    #[test]
    fn s3_item_downloads_via_issued_url_mixed_with_url_item() {
        let data = data();
        let base = tests::serve_slow_pub(Arc::clone(&data), 0);
        let dir = tmp("issued");
        let (presign, log) = fake_presign(&base);
        let mut app = App {
            concurrent: 2,
            mode: Mode::S3,
            bucket: "example-bucket".into(),
            presign_override: Some(presign),
            ..App::default()
        };
        app.input = "받은 파일 | data/big.bin".into();
        let mut entries = app.parse_input().unwrap();
        entries.push(Entry { line: 2, url: base.clone(), name: Some("plain".into()) });
        app.start(entries, dir.clone());
        wait_settled(&app.items);
        {
            let items = app.items.lock().unwrap();
            assert!(items.iter().all(|i| matches!(i.status, Status::Finished)));
            assert_eq!(items[0].url, ID, "목록에는 식별자를 보인다");
            assert!(items[0].presign.is_some() && items[1].presign.is_none());
        }
        assert_eq!(std::fs::read(dir.join("받은 파일.bin")).unwrap(), *data);
        assert_eq!(std::fs::read(dir.join("plain.bin")).unwrap(), *data);
        let log = log.lock().unwrap();
        assert_eq!(log.len(), 1, "S3 항목 하나만 발급");
        assert_eq!(log[0].0, ID);
    }

    // (ii) 일시정지 뒤 다른 주소가 발급돼도 같은 식별자의 기록으로 받은 곳부터 이어받는다
    #[test]
    fn s3_resume_after_pause_with_new_presigned_url() {
        const MARK: u8 = 0xAA;
        let data = data();
        let base = tests::serve_slow_pub(Arc::clone(&data), 20);
        let dir = tmp("resume");
        let (presign, log) = fake_presign(&base);
        let shared = s3_item(&dir, "big.bin", presign);
        let stop = Arc::clone(&shared.lock().unwrap()[0].stop);
        let (w, w_stop) = (Arc::clone(&shared), Arc::clone(&stop));
        thread::spawn(move || loop {
            if matches!(w.lock().unwrap()[0].status, Status::Downloading { done, .. } if done > 100_000) {
                w_stop.store(STOP_PAUSE, Relaxed);
                break;
            }
            thread::sleep(Duration::from_millis(2));
        });
        download(Arc::clone(&shared), 0, 4);
        assert!(matches!(shared.lock().unwrap()[0].status, Status::Paused { resumable: true, done, .. } if done > 0));

        // 진행 기록에는 식별자만 남고 발급 주소(서명·토큰·호스트)는 없다
        let meta_path = dir.join("big.bin.meta");
        let raw = std::fs::read_to_string(&meta_path).unwrap();
        assert_eq!(raw.lines().nth(2), Some(ID));
        assert!(!raw.contains("X-Amz") && !raw.contains("TOPSECRETTOKEN") && !raw.contains("127.0.0.1"), "{raw}");
        assert!(matches!(paused_from_disk(&dir.join("big.bin"), ID), Status::Paused { resumable: true, .. }));

        // 받은 구간을 마커로 덮는다: 이어받기라면 마커가 남고, 처음부터 받았다면 서버 데이터로 덮인다
        let m = load_meta(&meta_path, ID).unwrap();
        let part = OpenOptions::new().write(true).open(dir.join("big.bin.part")).unwrap();
        let got: Vec<(usize, usize)> = m.segs.iter().map(|(s, e, d)| (*s as usize, (*d).min(e - s) as usize)).collect();
        for &(s, d) in &got {
            part.write_all_at(&vec![MARK; d], s as u64).unwrap();
        }
        drop(part);

        stop.store(STOP_NONE, Relaxed);
        download(Arc::clone(&shared), 0, 4);
        assert!(matches!(shared.lock().unwrap()[0].status, Status::Finished));
        let out = std::fs::read(dir.join("big.bin")).unwrap();
        assert_eq!(out.len(), data.len());
        let mut kept = 0;
        for (i, b) in out.iter().enumerate() {
            match got.iter().find(|(s, d)| i >= *s && i < s + d) {
                Some(_) => {
                    assert_eq!(*b, MARK, "{i}: 받은 구간을 다시 받지 않아야 한다");
                    kept += 1;
                }
                None => assert_eq!(*b, data[i], "{i}: 나머지는 서버 데이터"),
            }
        }
        assert!(kept > 100_000);
        let log = log.lock().unwrap();
        assert_eq!(log.len(), 2, "시작·재개마다 새로 발급");
        assert!(log.iter().all(|(id, _)| id == ID));
        assert_ne!(log[0].1, log[1].1, "두 번째는 다른 주소");
        assert!(!dir.join("big.bin.meta").exists() && !dir.join("big.bin.part").exists());
    }

    // (iii) 발급 실패 → 오류 상태 + 문구 표시(자동 선택), 파일은 만들지 않는다
    #[test]
    fn presign_failure_marks_item_failed() {
        let dir = tmp("fail");
        let p: Presign = Arc::new(|_: &str| Err(NO_AWS_CREDS.to_string()));
        let shared = s3_item(&dir, "big.bin", p);
        download(Arc::clone(&shared), 0, 4);
        let items = shared.lock().unwrap();
        assert!(matches!(&items[0].status, Status::Failed(e) if e == NO_AWS_CREDS));
        assert!(items[0].selected);
        assert!(std::fs::read_dir(&dir).unwrap().next().is_none(), "아무 파일도 생기지 않는다");
    }

    // 실제 발급기: 자격증명이 없으면 네트워크에 나가지 않고 같은 문구로 실패한다
    #[test]
    fn real_presigner_without_creds_fails_with_message() {
        let dir = tmp("nocreds");
        let cfg = S3Config {
            profile: "default".into(),
            session_profile: None,
            credentials_file: Some(dir.join("없음")),
            session_cache: Some(dir.join("s3_session.json")),
            aws_config_file: None,
            mfa_serial: None,
            // 연결할 수 없는 주소: 리전을 물으러 나갔다면 다른 오류가 났을 것이다
            region_endpoint: Some("http://127.0.0.1:1".into()),
            regions: Default::default(),
        };
        assert_eq!(cfg.presign(ID), Err(NO_AWS_CREDS.to_string()));
    }

    // 받는 중 연결 오류가 나도 오류 문구에 서명 주소·토큰이 남지 않는다
    #[test]
    fn download_error_hides_presigned_url() {
        let dir = tmp("hide");
        let p: Presign = Arc::new(|_: &str| Ok("http://127.0.0.1:1/big.bin?X-Amz-Signature=abc&X-Amz-Security-Token=TOPSECRETTOKEN".into()));
        let shared = s3_item(&dir, "big.bin", p);
        download(Arc::clone(&shared), 0, 4);
        let Status::Failed(e) = shared.lock().unwrap()[0].status.clone() else { panic!("실패여야 한다") };
        assert!(!e.contains("X-Amz") && !e.contains("TOPSECRETTOKEN") && !e.contains("127.0.0.1"), "{e}");
        assert_eq!(
            hide_fetch_url("error sending request for url (https://h/k?X-Amz-Signature=s): refused".into(), ID),
            format!("error sending request for url ({ID}): refused")
        );
    }

    // 키에 http(s):// 가 들어 있어도 바꿔 넣은 식별자를 다시 바꾸지 않고 끝난다
    #[test]
    fn hide_fetch_url_terminates_when_id_contains_url() {
        const LOOPY: &str = "s3://example-bucket/mirror/https://example.com/a.zip";
        let (tx, rx) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let msgs = [
                "error sending request for url (https://h/mirror/https%3A//example.com/a.zip?X-Amz-Signature=s&X-Amz-Security-Token=TOPSECRETTOKEN): refused",
                "a http://h/x?X-Amz-Signature=s b https://h/y?X-Amz-Signature=t c",
                "https://h/z?X-Amz-Signature=u",
            ];
            let _ = tx.send(msgs.iter().map(|m| hide_fetch_url(m.to_string(), LOOPY)).collect::<Vec<_>>());
        });
        let out = rx.recv_timeout(Duration::from_secs(5)).expect("hide_fetch_url 이 끝나지 않는다 (무한 루프)");
        assert_eq!(out[0], format!("error sending request for url ({LOOPY}): refused"));
        assert_eq!(out[1], format!("a {LOOPY} b {LOOPY} c"));
        assert_eq!(out[2], LOOPY);
        assert!(out.iter().all(|m| !m.contains("X-Amz-Signature") && !m.contains("TOPSECRETTOKEN")), "{out:?}");

        // 실제 다운로드 경로에서도 끝나고 서명 쿼리가 남지 않는다
        let dir = tmp("hide_loop");
        let p: Presign = Arc::new(|_: &str| Ok("http://127.0.0.1:1/mirror/https%3A//example.com/a.zip?X-Amz-Signature=abc".into()));
        let mut it = Item::new(1, LOOPY.into(), "a.zip".into(), dir.clone());
        it.presign = Some(p);
        let shared: Shared = Arc::new(Mutex::new(vec![it]));
        let (tx, rx) = std::sync::mpsc::channel();
        let s = Arc::clone(&shared);
        thread::spawn(move || {
            download(s, 0, 4);
            let _ = tx.send(());
        });
        rx.recv_timeout(Duration::from_secs(30)).expect("download 이 끝나지 않는다");
        let Status::Failed(e) = shared.lock().unwrap()[0].status.clone() else { panic!("실패여야 한다") };
        assert!(!e.contains("X-Amz-Signature") && e.contains(LOOPY), "{e}");
    }

    // `.`/`..` 경로 조각이 있는 키는 HTTP 클라이언트가 주소를 정규화해 서명이 깨지므로 시작하지 않고 몇 번째 줄인지 알린다
    #[test]
    fn s3_input_rejects_dot_segments() {
        for key in ["a/../b.zip", "./a.zip", "a/./b.zip", "a/..", "s3://example-bucket/../a.zip"] {
            let err = parse_s3_input(&format!("ok.zip\n{key}"), "example-bucket").unwrap_err();
            assert!(err.contains("2번째 줄") && err.contains(".."), "{key}: {err}");
        }
        // 여러 줄이면 앞 셋 + 외
        let err = parse_s3_input("./a\n./b\nok.zip\n./c\n./d", "example-bucket").unwrap_err();
        assert!(err.contains("1, 2, 4번째 줄 외"), "{err}");
        // 점이 이름의 일부인 조각은 괜찮다
        let ok = parse_s3_input("a/..b/c.zip\n.hidden/x.zip\na/b../c\n...", "example-bucket").unwrap();
        assert_eq!(ok.len(), 4);
        // 버킷 누락과 함께 있으면 둘 다 알린다
        let err = parse_s3_input("./a.zip\nb.zip", "").unwrap_err();
        assert!(err.contains("1번째 줄") && err.contains("버킷") && err.contains("2번째 줄"), "{err}");
    }

    // (iv) S3 입력 해석 → 식별자·저장 파일명
    #[test]
    fn s3_input_parsing_and_names() {
        let input = "사진 | photos/2026 봄, 여행.JPG\nphotos/2026 봄, 여행.zip\n\n# 주석\ns3://other-bucket/a/b.tar.gz\tarchive\ndocs/50%25 off?.pdf\nfolder/";
        let es = parse_s3_input(input, " example-bucket ").unwrap();
        let got: Vec<(usize, &str, String)> = es.iter().map(|e| (e.line, e.url.as_str(), target_name(e))).collect();
        assert_eq!(
            got,
            vec![
                (1, "s3://example-bucket/photos/2026 봄, 여행.JPG", "사진.JPG".to_string()),
                (2, "s3://example-bucket/photos/2026 봄, 여행.zip", "2026 봄, 여행.zip".to_string()),
                (5, "s3://other-bucket/a/b.tar.gz", "archive.tar.gz".to_string()),
                (6, "s3://example-bucket/docs/50%25 off?.pdf", "50%25 off_.pdf".to_string()),
                (7, "s3://example-bucket/folder/", "file_7".to_string()),
            ]
        );
        // 버킷 칸에 s3://버킷/ 을 붙여넣어도 된다
        assert_eq!(parse_s3_input("a.zip", "s3://example-bucket/").unwrap()[0].url, "s3://example-bucket/a.zip");
        // 버킷 칸이 비어도 모든 줄에 버킷이 있으면 시작할 수 있다
        assert_eq!(parse_s3_input("s3://example-bucket/a.zip", "").unwrap().len(), 1);
        // 버킷을 알 수 없는 줄이 있으면 시작하지 않는다
        let err = parse_s3_input("s3://example-bucket/a.zip\nb.zip\nc.zip", "  ").unwrap_err();
        assert!(err.contains("버킷") && err.contains("2, 3번째 줄"), "{err}");
        // 같은 이름 처리(unique_name)는 URL 모드와 같다
        let d = tmp("names");
        std::fs::write(d.join("2026 봄, 여행.zip"), b"x").unwrap();
        let mut used = vec![];
        assert_eq!(unique_name(&d, &target_name(&es[1]), &es[1].url, &mut used), "2026 봄, 여행 (1).zip");
        // URL 모드 해석은 그대로
        let app = App { input: "https://a.com/x.jpg".into(), ..App::default() };
        assert_eq!(app.parse_input().unwrap()[0].url, "https://a.com/x.jpg");
        assert_eq!(s3_parts("s3://b/k/x"), Some(("b", "k/x")));
        assert_eq!(s3_parts("https://b/k"), None);
    }

    // (v) 설정 저장·복원: 모드·버킷·프로필. 새 키가 없는 옛 설정도 그대로 읽힌다
    #[test]
    fn s3_config_roundtrip_and_old_config() {
        let app = App {
            mode: Mode::S3,
            bucket: "example-bucket".into(),
            s3_profile: "example".into(),
            s3_session_profile: Some("example-session".into()),
            ..App::default()
        };
        let v: serde_json::Value = serde_json::from_str(&app.config_json()).unwrap();
        let mut loaded = App::default();
        loaded.apply_config(&v);
        assert_eq!(loaded.mode, Mode::S3);
        assert_eq!(loaded.bucket, "example-bucket");
        assert_eq!((loaded.s3_profile.as_str(), loaded.s3_session_profile.as_deref()), ("example", Some("example-session")));
        assert_eq!(loaded.config_json(), app.config_json());

        let old: serde_json::Value = serde_json::from_str(r#"{"folder":"/tmp/x","concurrent":5,"segments":2,"theme":"dark"}"#).unwrap();
        let mut loaded = App::default();
        loaded.apply_config(&old);
        assert_eq!((loaded.mode, loaded.bucket.as_str()), (Mode::Url, ""));
        assert_eq!((loaded.s3_profile.as_str(), loaded.s3_session_profile.as_deref()), ("default", None));
        assert_eq!((loaded.concurrent, loaded.segments, loaded.theme), (5, 2, Theme::Dark));
    }

    // 자격증명 찾는 순서: 세션 캐시 → 세션 프로필(토큰 있고 만료 전) → 기본 프로필 장기 키
    #[test]
    fn credential_resolution_order() {
        let dir = tmp("creds");
        let now = 1_800_000_000u64;
        let text = format!(
            "[default]\naws_access_key_id = AKIADEFAULT\naws_secret_access_key = test-secret\n\n\
             [example-session]\naws_access_key_id = ASIASESSION\naws_secret_access_key = test-secret\naws_session_token = test-token\naws_session_expiration = 2027-06-01T00:00:00Z\n\n\
             [stale-session]\naws_access_key_id = ASIASTALE\naws_secret_access_key = test-secret\naws_session_token = test-token\naws_session_expiration = 2020-01-01T00:00:00Z\n\n\
             [no-token]\naws_access_key_id = AKIANOTOKEN\naws_secret_access_key = test-secret\n"
        );
        let cache = dir.join("s3_session.json");
        let key = |c: Option<s3::Creds>| c.map(|c| c.access_key);
        let r = |sp: Option<&str>, p: &str| key(resolve_creds(Some(&cache), Some(&text), sp, p, now));
        assert_eq!(r(Some("example-session"), "default").as_deref(), Some("ASIASESSION"));
        assert_eq!(r(Some("stale-session"), "default").as_deref(), Some("AKIADEFAULT"), "만료된 세션은 건너뛴다");
        assert_eq!(r(Some("no-token"), "default").as_deref(), Some("AKIADEFAULT"), "토큰 없는 프로필은 세션으로 보지 않는다");
        assert_eq!(r(None, "default").as_deref(), Some("AKIADEFAULT"));
        assert_eq!(r(None, "missing"), None);
        assert_eq!(key(resolve_creds(Some(&cache), None, None, "default", now)), None);
        // 앱 세션 캐시가 유효하면 장기 키보다 먼저
        let cached = s3::Creds {
            access_key: "ASIACACHE".into(),
            secret_key: "test-secret".into(),
            session_token: Some("test-token".into()),
            expires_at: Some(now + 3600),
        };
        s3::save_session(&cache, &cached, "AKIADEFAULT").unwrap();
        assert_eq!(r(None, "default").as_deref(), Some("ASIACACHE"));
        assert_eq!(r(Some("stale-session"), "default").as_deref(), Some("ASIACACHE"));
        // 앱 세션과 세션 프로필이 둘 다 유효하면 만료가 더 늦은 쪽 (example-session 은 2027-06-01 까지)
        assert_eq!(r(Some("example-session"), "default").as_deref(), Some("ASIASESSION"));
        let later = s3::Creds { expires_at: Some(s3::parse_iso8601("2027-07-01T00:00:00Z").unwrap()), ..cached.clone() };
        s3::save_session(&cache, &later, "AKIADEFAULT").unwrap();
        assert_eq!(r(Some("example-session"), "default").as_deref(), Some("ASIACACHE"));
        // 만료 시각이 적혀 있지 않은 프로필 세션보다는 만료 시각을 아는 앱 세션
        let no_exp = text.replace("aws_session_expiration = 2027-06-01T00:00:00Z\n", "");
        assert_eq!(key(resolve_creds(Some(&cache), Some(&no_exp), Some("example-session"), "default", now)).as_deref(), Some("ASIACACHE"));
        s3::save_session(&cache, &cached, "AKIADEFAULT").unwrap();
        // 세션 주인(장기 키 ID)이 지금 s3_profile 의 키와 다르면 쓰지 않는다: no-token 프로필로 바꾸면 그 키로
        assert_eq!(r(None, "no-token").as_deref(), Some("AKIANOTOKEN"));
        // 장기 키를 알 수 없으면(자격증명 파일 없음) 앱 세션의 주인을 확인할 수 없어 쓰지 않는다
        assert_eq!(key(resolve_creds(Some(&cache), None, None, "default", now)), None);
        // 캐시가 만료됐으면 다음 순서로
        assert_eq!(key(resolve_creds(Some(&cache), Some(&text), None, "default", now + 3600)).as_deref(), Some("AKIADEFAULT"));
        // 주인 정보가 없는 옛 세션 파일은 무효
        std::fs::write(&cache, format!(r#"{{"access_key":"ASIACACHE","secret_key":"s","session_token":"t","expires_at":{}}}"#, now + 3600)).unwrap();
        assert_eq!(r(None, "default").as_deref(), Some("AKIADEFAULT"));

        // presign 유효 시간: 12시간과 남은 시간 중 짧은 쪽
        assert_eq!(presign_secs(&cached, now), 3600);
        assert_eq!(presign_secs(&s3::Creds { expires_at: None, ..cached.clone() }, now), 12 * 3600);
        assert_eq!(presign_secs(&s3::Creds { expires_at: Some(now + 20 * 3600), ..cached }, now), 12 * 3600);
    }

    /// x-amz-bucket-region 헤더만 돌려주는 가짜 리전 조회 서버. 받은 요청 수를 센다
    fn serve_region(region: &'static str) -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits = Arc::new(AtomicUsize::new(0));
        let h = Arc::clone(&hits);
        thread::spawn(move || {
            for mut s in listener.incoming().flatten() {
                h.fetch_add(1, Relaxed);
                let mut r = std::io::BufReader::new(s.try_clone().unwrap());
                let mut line = String::new();
                while r.read_line(&mut line).unwrap_or(0) > 0 && !line.trim().is_empty() {
                    line.clear();
                }
                let _ = s.write_all(format!("HTTP/1.1 403 Forbidden\r\nx-amz-bucket-region: {region}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes());
            }
        });
        (format!("http://127.0.0.1:{port}"), hits)
    }

    // 실제 발급기: 리전은 버킷별로 한 번만 묻고, 유효 시간은 12시간 이내
    #[test]
    fn real_presigner_caches_region_per_bucket() {
        let dir = tmp("presign");
        let creds = dir.join("credentials");
        std::fs::write(&creds, "[default]\naws_access_key_id = AKIADEFAULT\naws_secret_access_key = test-secret\n").unwrap();
        let (endpoint, hits) = serve_region("ap-northeast-2");
        let cfg = S3Config {
            profile: "default".into(),
            session_profile: None,
            credentials_file: Some(creds),
            session_cache: Some(dir.join("s3_session.json")),
            aws_config_file: None,
            mfa_serial: None,
            region_endpoint: Some(endpoint),
            regions: Default::default(),
        };
        let a = cfg.presign("s3://example-bucket/photos/2026 봄.zip").unwrap();
        let b = cfg.presign("s3://example-bucket/b.zip").unwrap();
        assert!(a.starts_with("https://example-bucket.s3.ap-northeast-2.amazonaws.com/photos/2026%20"), "{a}");
        assert!(a.contains("X-Amz-Expires=43200") && b.contains("X-Amz-Expires=43200"));
        assert_eq!(hits.load(Relaxed), 1, "같은 버킷의 리전은 한 번만 묻는다");
        assert_eq!(cfg.regions.lock().unwrap().get("example-bucket").map(String::as_str), Some("ap-northeast-2"));
    }
}

#[cfg(test)]
mod mfa_tests {
    use super::*;
    use std::io::BufRead;
    use std::net::{Shutdown, TcpListener, TcpStream};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};

    const ID: &str = "s3://example-bucket/data/big.bin";
    const SERIAL: &str = "arn:aws:iam::123456789012:mfa/example";
    const CREDS: &str = "[default]\naws_access_key_id = AKIAEXAMPLE\naws_secret_access_key = test-secret\n";

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ud_mfa_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn data() -> Arc<Vec<u8>> {
        Arc::new((0..3_500_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect())
    }

    /// 가짜 자격증명 폴더. `creds` 가 있으면 credentials 파일, `serial` 이 있으면 config 의 [default] 에 mfa_serial
    fn files(dir: &Path, creds: Option<&str>, serial: Option<&str>) -> S3Files {
        if let Some(c) = creds {
            std::fs::write(dir.join("credentials"), c).unwrap();
        }
        let cfg = match serial {
            Some(s) => format!("[default]\nregion = us-east-1\nmfa_serial = {s}\n"),
            None => "[default]\nregion = us-east-1\n".to_string(),
        };
        std::fs::write(dir.join("config"), cfg).unwrap();
        S3Files { credentials: Some(dir.join("credentials")), aws_config: Some(dir.join("config")), session: Some(dir.join("s3_session.json")) }
    }

    fn mfa_app(files: S3Files) -> App {
        App { mode: Mode::S3, bucket: "example-bucket".into(), s3_files_override: Some(files), ..App::default() }
    }

    /// unix 초 → `2026-08-13T22:10:00Z`
    fn iso(unix: u64) -> String {
        let a = s3::amz_date(unix);
        format!("{}-{}-{}T{}:{}:{}Z", &a[0..4], &a[4..6], &a[6..8], &a[9..11], &a[11..13], &a[13..15])
    }

    /// 요청 하나를 받아 기록하고 `status`·`body` 로 답하는 가짜 STS
    fn fake_sts(status: &str, body: String) -> (String, Arc<Mutex<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let got: Arc<Mutex<String>> = Default::default();
        let (g, status) = (Arc::clone(&got), status.to_string());
        thread::spawn(move || {
            if let Some(Ok(mut s)) = listener.incoming().next() {
                let mut r = std::io::BufReader::new(s.try_clone().unwrap());
                let (mut raw, mut len) = (String::new(), 0usize);
                loop {
                    let mut line = String::new();
                    if r.read_line(&mut line).unwrap_or(0) == 0 {
                        break;
                    }
                    if let Some((k, v)) = line.split_once(':') {
                        if k.eq_ignore_ascii_case("content-length") {
                            len = v.trim().parse().unwrap_or(0);
                        }
                    }
                    let end = line.trim().is_empty();
                    raw.push_str(&line);
                    if end {
                        break;
                    }
                }
                let mut b = vec![0u8; len];
                let _ = r.read_exact(&mut b);
                raw.push_str(&String::from_utf8_lossy(&b));
                *g.lock().unwrap() = raw;
                let resp = format!("HTTP/1.1 {status}\r\nContent-Type: text/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                let _ = s.write_all(resp.as_bytes());
                let _ = s.shutdown(Shutdown::Both);
            }
        });
        (format!("http://127.0.0.1:{port}"), got)
    }

    fn sts_ok(expires: u64) -> String {
        format!(
            "<GetSessionTokenResponse><GetSessionTokenResult><Credentials>\
             <SessionToken>test-session-token</SessionToken><SecretAccessKey>test-session-secret</SecretAccessKey>\
             <Expiration>{}</Expiration><AccessKeyId>ASIAEXAMPLE</AccessKeyId>\
             </Credentials></GetSessionTokenResult></GetSessionTokenResponse>",
            iso(expires)
        )
    }

    fn wait_job(app: &App) -> MfaJob {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let j = app.mfa_job.lock().unwrap().clone();
            if !matches!(j, MfaJob::Running) {
                return j;
            }
            assert!(Instant::now() < deadline, "MFA 발급이 끝나지 않았다");
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn settled(items: &[Item]) -> bool {
        !items.iter().any(|i| matches!(i.status, Status::Waiting | Status::Downloading { .. }))
    }

    fn wait_settled(shared: &Shared) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while !settled(&shared.lock().unwrap()) {
            assert!(Instant::now() < deadline, "시간 초과");
            thread::sleep(Duration::from_millis(5));
        }
    }

    // (i) 가짜 STS 성공 → 세션 파일 0600 저장, 상태가 유효로 바뀐다. 코드 칸은 요청 직후 비워진다
    #[test]
    fn mfa_refresh_saves_session_and_becomes_valid() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp("ok");
        let now = s3::now_unix();
        let (endpoint, req) = fake_sts("200 OK", sts_ok(now + MFA_SESSION_SECS));
        let mut app = mfa_app(files(&dir, Some(CREDS), Some(SERIAL)));
        app.sts_endpoint = Some(endpoint);
        app.reload_mfa_info();
        assert_eq!(app.mfa_info.status(now), MfaStatus::NeedCode);
        app.start_notice = Some(MFA_NEEDED.into());
        app.mfa_code = "123456".into();
        app.refresh_mfa();
        assert!(app.mfa_code.is_empty(), "요청을 보내는 즉시 코드 칸을 비운다");
        assert!(matches!(wait_job(&app), MfaJob::Done));
        app.poll_mfa();
        assert!(matches!(*app.mfa_job.lock().unwrap(), MfaJob::Idle));
        assert_eq!(app.start_notice, None, "세션을 받으면 시작 차단 안내를 지운다");

        let path = dir.join("s3_session.json");
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let saved = s3::load_session(&path, "AKIAEXAMPLE", now).expect("세션 저장");
        assert_eq!(saved.access_key, "ASIAEXAMPLE");
        assert_eq!(app.mfa_info.status(now), MfaStatus::Valid { source: SessionSource::App, left: Some(MFA_SESSION_SECS) });
        assert_eq!(mfa_status_text(&app.mfa_info.status(now)).0, "세션 12시간 0분 남음");
        assert!(!app.mfa_info.blocks_start(now));

        // 장기 키·기기 번호·12시간으로 요청했다
        let raw = req.lock().unwrap().clone();
        assert!(raw.contains("Credential=AKIAEXAMPLE/"), "{raw}");
        assert!(raw.contains("DurationSeconds=43200") && raw.contains("SerialNumber=arn%3Aaws%3Aiam%3A%3A123456789012%3Amfa%2Fexample"));

        // 이제 발급기는 앱 세션으로 서명한다 (MFA 만료로 막지 않는다. 리전은 미리 넣어 네트워크에 나가지 않게 한다)
        app.s3_regions.lock().unwrap().insert("example-bucket".into(), "us-east-1".into());
        let url = (app.presigner())(ID).unwrap();
        assert!(url.contains("X-Amz-Credential=ASIAEXAMPLE") && url.contains("X-Amz-Security-Token="), "{url}");
    }

    // (ii) 틀린 코드 → 세션 파일 없음, 안내 문구에 코드 값이 없다
    #[test]
    fn mfa_bad_code_leaves_no_session_and_hides_code() {
        let dir = tmp("bad");
        let denied = "<ErrorResponse><Error><Type>Sender</Type><Code>AccessDenied</Code>\
                      <Message>MultiFactorAuthentication failed with invalid MFA one time pass code. </Message></Error></ErrorResponse>";
        let (endpoint, _req) = fake_sts("403 Forbidden", denied.into());
        let mut app = mfa_app(files(&dir, Some(CREDS), Some(SERIAL)));
        app.sts_endpoint = Some(endpoint);
        app.reload_mfa_info();
        app.mfa_code = "654321".into();
        app.refresh_mfa();
        assert!(app.mfa_code.is_empty());
        let MfaJob::Error(msg) = wait_job(&app) else { panic!("오류여야 한다") };
        assert_eq!(msg, MFA_BAD_CODE);
        assert!(!msg.contains("654321"));
        app.poll_mfa();
        assert!(!dir.join("s3_session.json").exists(), "세션 파일을 만들지 않는다");
        assert_eq!(app.mfa_info.status(s3::now_unix()), MfaStatus::NeedCode);

        // 형식이 틀린 코드는 STS 에 나가지 않고 같은 안내 (연결할 수 없는 주소)
        app.sts_endpoint = Some("http://127.0.0.1:1".into());
        app.mfa_code = "12345".into();
        app.refresh_mfa();
        assert!(matches!(wait_job(&app), MfaJob::Error(m) if m == MFA_BAD_CODE));

        // 장기 키가 없으면 요청하지 않고 이유를 보인다
        let dir2 = tmp("bad_nokey");
        let mut app2 = mfa_app(files(&dir2, None, Some(SERIAL)));
        app2.reload_mfa_info();
        app2.mfa_code = "123456".into();
        app2.refresh_mfa();
        assert!(app2.mfa_code.is_empty());
        assert!(matches!(wait_job(&app2), MfaJob::Error(m) if m.contains("s3_profile") && !m.contains("123456")));
    }

    // (iii) 기기 번호 있음 + 세션 없음 → S3 시작 차단, 기기 번호 없음 → 장기 키로 진행
    #[test]
    fn s3_start_blocked_without_session_when_serial_known() {
        let dir = tmp("block");
        let mut app = mfa_app(files(&dir, Some(CREDS), Some(SERIAL)));
        app.input = "data/big.bin".into();
        let entries = app.parse_input().unwrap();
        let out = tmp("block_out");
        assert!(!app.start_checked(entries.clone(), out.clone()));
        assert!(app.items.lock().unwrap().is_empty(), "시작하지 않는다");
        assert_eq!(app.start_notice.as_deref(), Some(MFA_NEEDED));
        // 실제 발급기도 장기 키로 떨어지지 않는다 (네트워크에 나가지 않고 바로)
        assert_eq!((app.presigner())(ID), Err(MFA_EXPIRED.to_string()));
        // URL 항목만 있으면 막지 않는다
        assert!(!app.mfa_blocks_start(&[Entry { line: 1, url: "https://example.com/a.zip".into(), name: None }]));
        assert_eq!(app.start_notice, None);
        // 유효한 앱 세션이 있으면 막지 않는다
        let now = s3::now_unix();
        let session = s3::Creds {
            access_key: "ASIAEXAMPLE".into(),
            secret_key: "test-secret".into(),
            session_token: Some("test-token".into()),
            expires_at: Some(now + 3600),
        };
        s3::save_session(&dir.join("s3_session.json"), &session, "AKIAEXAMPLE").unwrap();
        assert!(!app.mfa_blocks_start(&entries));

        // 기기 번호는 config.json 의 s3_mfa_serial 로도 안다 (~/.aws/config 에 없을 때)
        let dir_s = tmp("block_setting");
        let mut app_s = mfa_app(files(&dir_s, Some(CREDS), None));
        app_s.s3_mfa_serial = SERIAL.into();
        assert!(app_s.mfa_blocks_start(&entries));

        // 기기 번호 없음 → 진행
        let data = data();
        let base = tests::serve_slow_pub(Arc::clone(&data), 0);
        let dir2 = tmp("noserial");
        let mut app2 = mfa_app(files(&dir2, Some(CREDS), None));
        let b = base.clone();
        app2.presign_override = Some(Arc::new(move |_: &str| Ok(b.clone())));
        let out2 = tmp("noserial_out");
        assert!(app2.start_checked(entries, out2.clone()));
        assert_eq!(app2.start_notice, None);
        wait_settled(&app2.items);
        assert!(matches!(app2.items.lock().unwrap()[0].status, Status::Finished));
        assert_eq!(std::fs::read(out2.join("big.bin")).unwrap(), *data);
    }

    /// 서명(X-Amz-Signature 값)별 응답 규칙. (서명, 그 서명으로 온 몇 번째 요청(0부터)) → 동작
    #[derive(Clone, Copy, PartialEq)]
    enum Act {
        Ok,
        /// 본문 1000 바이트 뒤에 끊는다
        Cut,
        Forbidden,
    }

    /// Range 를 지원하는 가짜 S3. 규칙대로 403·끊김을 낸다. 요청 수를 센다
    fn serve_signed(data: Arc<Vec<u8>>, rule: fn(&str, usize) -> Act) -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let counts: Arc<Mutex<HashMap<String, usize>>> = Default::default();
        let total = Arc::new(AtomicUsize::new(0));
        let t = Arc::clone(&total);
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (data, counts) = (Arc::clone(&data), Arc::clone(&counts));
                t.fetch_add(1, SeqCst);
                thread::spawn(move || handle_signed(stream, data, counts, rule));
            }
        });
        (format!("http://127.0.0.1:{port}/big.bin"), total)
    }

    fn handle_signed(mut s: TcpStream, data: Arc<Vec<u8>>, counts: Arc<Mutex<HashMap<String, usize>>>, rule: fn(&str, usize) -> Act) {
        let mut reader = std::io::BufReader::new(s.try_clone().unwrap());
        let (mut first, mut range) = (String::new(), None);
        let _ = reader.read_line(&mut first);
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
        let sig = first.split("X-Amz-Signature=").nth(1).map(|r| r.split(['&', ' ']).next().unwrap_or("").to_string()).unwrap_or_default();
        let nth = {
            let mut c = counts.lock().unwrap();
            let n = c.entry(sig.clone()).or_insert(0);
            *n += 1;
            *n - 1
        };
        let act = rule(&sig, nth);
        if act == Act::Forbidden {
            let body = "<Error><Code>AccessDenied</Code><Message>Request has expired</Message></Error>";
            let _ = s.write_all(format!("HTTP/1.1 403 Forbidden\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes());
            let _ = s.shutdown(Shutdown::Both);
            return;
        }
        let len = data.len();
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
        let mut head = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nETag: \"t1\"\r\nAccept-Ranges: bytes\r\nConnection: close\r\n", body.len());
        if range.is_some() {
            head += &format!("Content-Range: bytes {start}-{end}/{len}\r\n");
        }
        head += "\r\n";
        let _ = s.write_all(head.as_bytes());
        let body = if act == Act::Cut && body.len() > 1000 { &body[..1000] } else { body };
        let _ = s.write_all(body);
        let _ = s.shutdown(Shutdown::Both);
    }

    /// 부를 때마다 sig0, sig1 … 로 서명한 주소를 주는 가짜 발급기. `allow` 가 false 면 첫 발급 뒤로는 MFA 만료로 실패한다
    fn signer(base: &str, allow: Arc<AtomicBool>) -> (Presign, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let (base, c) = (base.to_string(), Arc::clone(&calls));
        let p: Presign = Arc::new(move |id: &str| {
            assert_eq!(id, ID);
            let n = c.fetch_add(1, SeqCst);
            if n > 0 && !allow.load(SeqCst) {
                return Err(MFA_EXPIRED.to_string());
            }
            Ok(format!("{base}?X-Amz-Signature=sig{n}&X-Amz-Security-Token=TOPSECRETTOKEN"))
        });
        (p, calls)
    }

    fn s3_item(dir: &Path, presign: Presign) -> Shared {
        let mut it = Item::new(1, ID.into(), "big.bin".into(), dir.to_path_buf());
        it.presign = Some(presign);
        Arc::new(Mutex::new(vec![it]))
    }

    /// sig0: 확인 요청(0번)은 받고, 구간 첫 요청 셋은 1000 바이트 뒤 끊기고, 그 뒤로는 403. 새 서명은 정상
    fn expire_mid_download(sig: &str, nth: usize) -> Act {
        match (sig, nth) {
            ("sig0", 0) => Act::Ok,
            ("sig0", 1..=3) => Act::Cut,
            ("sig0", _) => Act::Forbidden,
            _ => Act::Ok,
        }
    }

    // (iv) 받는 도중 403 → 한 번 재발급해 그 지점부터 이어서 완료, 내용 일치. 처음부터 403 이어도 재발급. 상한이 있다.
    // URL 항목의 403 은 예전처럼 재발급 없이 실패
    #[test]
    fn s3_403_mid_download_reissues_and_completes() {
        let data = data();
        let (base, _) = serve_signed(Arc::clone(&data), expire_mid_download);
        let dir = tmp("reissue");
        let (p, calls) = signer(&base, Arc::new(AtomicBool::new(true)));
        let shared = s3_item(&dir, p);
        download(Arc::clone(&shared), 0, 4);
        let st = shared.lock().unwrap()[0].status.clone();
        assert!(matches!(st, Status::Finished), "완료여야 한다: {}", match st { Status::Failed(e) => e, _ => String::new() });
        assert_eq!(std::fs::read(dir.join("big.bin")).unwrap(), *data);
        assert_eq!(calls.load(SeqCst), 2, "구간이 셋이어도 재발급은 한 번");
        // S3 항목도 완료한 파일에 격리 표시가 붙는다
        let q = tests::quarantine_of(&dir.join("big.bin")).expect("S3 로 받은 파일에 격리 표시가 없다");
        assert!(q.starts_with("0081;"), "격리 표시 값: {q}");

        // 확인 요청부터 403 (이미 만료된 주소로 시작)
        let (base, _) = serve_signed(Arc::clone(&data), |sig, _| if sig == "sig0" { Act::Forbidden } else { Act::Ok });
        let dir = tmp("reissue_probe");
        let (p, calls) = signer(&base, Arc::new(AtomicBool::new(true)));
        let shared = s3_item(&dir, p);
        download(Arc::clone(&shared), 0, 4);
        assert!(matches!(shared.lock().unwrap()[0].status, Status::Finished));
        assert_eq!(std::fs::read(dir.join("big.bin")).unwrap(), *data);
        assert_eq!(calls.load(SeqCst), 2);

        // 새 주소도 계속 403 → 상한(MAX_REISSUE)까지만 재발급하고 실패. 오류에 서명·토큰이 없다
        let (base, _) = serve_signed(Arc::clone(&data), |_, _| Act::Forbidden);
        let dir = tmp("reissue_cap");
        let (p, calls) = signer(&base, Arc::new(AtomicBool::new(true)));
        let shared = s3_item(&dir, p);
        download(Arc::clone(&shared), 0, 4);
        let Status::Failed(e) = shared.lock().unwrap()[0].status.clone() else { panic!("실패여야 한다") };
        assert!(e.contains("403"), "{e}");
        assert!(!e.contains("X-Amz") && !e.contains("TOPSECRETTOKEN"), "{e}");
        assert_eq!(calls.load(SeqCst), 1 + MAX_REISSUE);

        // URL 항목: 403 이면 재발급 없이 바로 실패 (요청 한 번)
        let (url, hits) = serve_signed(Arc::clone(&data), |_, _| Act::Forbidden);
        let dir = tmp("url403");
        let shared: Shared = Arc::new(Mutex::new(vec![Item::new(1, url, "big.bin".into(), dir.clone())]));
        download(Arc::clone(&shared), 0, 4);
        assert!(matches!(&shared.lock().unwrap()[0].status, Status::Failed(e) if e == "HTTP 403 Forbidden"));
        assert_eq!(hits.load(SeqCst), 1);
        assert!(std::fs::read_dir(&dir).unwrap().next().is_none());
    }

    // (v) 재발급할 자격증명이 없으면(MFA 세션 만료) 일시정지 + 부분 파일·메타 보존 + 안내. 세션을 갱신한 뒤 이어받으면 완료
    #[test]
    fn s3_expired_session_pauses_and_resumes() {
        let data = data();
        let (base, _) = serve_signed(Arc::clone(&data), expire_mid_download);
        let dir = tmp("expire");
        let allow = Arc::new(AtomicBool::new(false));
        let (p, _calls) = signer(&base, Arc::clone(&allow));
        let shared = s3_item(&dir, p);
        download(Arc::clone(&shared), 0, 4);
        {
            let items = shared.lock().unwrap();
            assert!(matches!(items[0].status, Status::Paused { resumable: true, done, .. } if done > 0), "일시정지여야 한다");
            assert_eq!(items[0].note.as_deref(), Some(MFA_EXPIRED));
            assert!(!items[0].selected, "실패가 아니므로 자동 선택하지 않는다");
        }
        assert!(dir.join("big.bin.part").exists());
        let raw = std::fs::read_to_string(dir.join("big.bin.meta")).unwrap();
        assert_eq!(raw.lines().nth(2), Some(ID));
        assert!(!raw.contains("X-Amz") && !raw.contains("TOPSECRETTOKEN") && !raw.contains("sig"), "{raw}");
        let meta = load_meta(&dir.join("big.bin.meta"), ID).unwrap();
        assert!(meta.segs.iter().any(|(_, _, d)| *d > 0), "받은 만큼 기록");

        // 세션 갱신 뒤 [이어받기] (UI 와 같은 경로: 큐에 다시 넣는다) → 안내가 지워지고 완료
        allow.store(true, SeqCst);
        let (queue, active): (Queue, _) = (Default::default(), Arc::new(AtomicUsize::new(0)));
        enqueue_jobs(&shared, &queue, &active, &[0], 1, 4);
        assert_eq!(shared.lock().unwrap()[0].note, None);
        wait_settled(&shared);
        assert!(matches!(shared.lock().unwrap()[0].status, Status::Finished));
        assert_eq!(std::fs::read(dir.join("big.bin")).unwrap(), *data);
        assert!(!dir.join("big.bin.part").exists() && !dir.join("big.bin.meta").exists());

        // 시작할 때부터 세션이 없으면(이어받기 버튼 등) 실패가 아니라 일시정지 + 같은 안내, 파일은 만들지 않는다
        let dir = tmp("expire_start");
        let p: Presign = Arc::new(|_: &str| Err(MFA_EXPIRED.to_string()));
        let shared = s3_item(&dir, p);
        download(Arc::clone(&shared), 0, 4);
        let items = shared.lock().unwrap();
        assert!(matches!(items[0].status, Status::Paused { .. }));
        assert_eq!(items[0].note.as_deref(), Some(MFA_EXPIRED));
        assert!(std::fs::read_dir(&dir).unwrap().next().is_none());
    }

    // 도중 재발급이 MFA 만료가 아닌 이유(자격증명 없음·리전 조회 실패 등)로 실패하면 처음 발급과 같은 규칙으로 오류 상태.
    // 단 부분 파일·메타는 남겨 [실패·일시정지 다시 받기] 로 이어받는다
    #[test]
    fn s3_reissue_other_error_fails_but_keeps_partial() {
        let data = data();
        let (base, hits) = serve_signed(Arc::clone(&data), expire_mid_download);
        let dir = tmp("reissue_err");
        let allow = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicUsize::new(0));
        let (b, a, c) = (base.clone(), Arc::clone(&allow), Arc::clone(&calls));
        let p: Presign = Arc::new(move |id: &str| {
            assert_eq!(id, ID);
            let n = c.fetch_add(1, SeqCst);
            if n > 0 && !a.load(SeqCst) {
                return Err("버킷 리전 확인 실패: HTTP 500".to_string());
            }
            Ok(format!("{b}?X-Amz-Signature=sig{n}&X-Amz-Security-Token=TOPSECRETTOKEN"))
        });
        let shared = s3_item(&dir, p);
        download(Arc::clone(&shared), 0, 4);
        {
            let items = shared.lock().unwrap();
            let Status::Failed(e) = &items[0].status else { panic!("오류 상태여야 한다") };
            assert_eq!(e, "버킷 리전 확인 실패: HTTP 500");
            assert!(items[0].selected, "실패는 자동 선택");
            assert_eq!(items[0].note, None);
        }
        assert_eq!(calls.load(SeqCst), 2, "재발급 실패는 구간 재시도로 다시 부르지 않는다");
        // 시간 대신 요청 수로 확인한다(느린 CI 에서 시간 기준은 흔들린다). 정상은 7회, 구간 재시도를 되풀이하면 더 늘어난다
        assert!(hits.load(SeqCst) <= 8, "재발급 실패는 구간 재시도를 되풀이하지 않는다: 요청 {}회", hits.load(SeqCst));
        assert!(dir.join("big.bin.part").exists(), "부분 파일 보존");
        let meta = load_meta(&dir.join("big.bin.meta"), ID).expect("메타 보존");
        assert!(meta.segs.iter().any(|(_, _, d)| *d > 0));

        // 원인을 고친 뒤 다시 받기 → 이어받아 완료
        allow.store(true, SeqCst);
        let (queue, active): (Queue, _) = (Default::default(), Arc::new(AtomicUsize::new(0)));
        enqueue_jobs(&shared, &queue, &active, &[0], 1, 4);
        wait_settled(&shared);
        assert!(matches!(shared.lock().unwrap()[0].status, Status::Finished));
        assert_eq!(std::fs::read(dir.join("big.bin")).unwrap(), *data);

        // 처음 발급도 같은 규칙: MFA 만료가 아닌 이유는 오류 상태 (MFA 만료는 위 테스트처럼 일시정지)
        let dir = tmp("issue_err_start");
        let p: Presign = Arc::new(|_: &str| Err("버킷 리전 확인 실패: HTTP 500".to_string()));
        let shared = s3_item(&dir, p);
        download(Arc::clone(&shared), 0, 4);
        assert!(matches!(&shared.lock().unwrap()[0].status, Status::Failed(e) if e == "버킷 리전 확인 실패: HTTP 500"));
    }

    // s3_profile 프로필에 세션 토큰이 있으면(임시 자격증명) STS 에 보내지 않고 안내한다
    #[test]
    fn mfa_refresh_refuses_temporary_long_term_profile() {
        let dir = tmp("temp_profile");
        let temp = "[default]\naws_access_key_id = ASIATEMPEXAMPLE\naws_secret_access_key = test-secret\naws_session_token = test-token\n";
        let (endpoint, req) = fake_sts("200 OK", sts_ok(s3::now_unix() + MFA_SESSION_SECS));
        let mut app = mfa_app(files(&dir, Some(temp), Some(SERIAL)));
        app.sts_endpoint = Some(endpoint);
        app.reload_mfa_info();
        app.mfa_code = "123456".into();
        app.refresh_mfa();
        assert!(app.mfa_code.is_empty());
        let MfaJob::Error(msg) = wait_job(&app) else { panic!("오류여야 한다") };
        assert!(msg.contains("임시 자격증명") && msg.contains("s3_profile"), "{msg}");
        assert!(!msg.contains("123456") && !msg.contains("test-secret") && !msg.contains("test-token"), "{msg}");
        thread::sleep(Duration::from_millis(200));
        assert!(req.lock().unwrap().is_empty(), "STS 에 요청하지 않는다");
        assert!(!dir.join("s3_session.json").exists());
    }

    // 기기 ARN 칸: AWS 설정에서 온 기기는 칸·버튼 없음, 설정 파일의 기기는 [기기 변경] 버튼, 비었거나 고치는 중이면 칸
    #[test]
    fn mfa_serial_field_states() {
        assert_eq!(serial_ui(true, "", false), SerialUi::Hidden);
        assert_eq!(serial_ui(true, SERIAL, true), SerialUi::Hidden);
        assert_eq!(serial_ui(false, "", false), SerialUi::Field);
        assert_eq!(serial_ui(false, "  ", false), SerialUi::Field);
        assert_eq!(serial_ui(false, SERIAL, false), SerialUi::ChangeButton);
        assert_eq!(serial_ui(false, SERIAL, true), SerialUi::Field);
    }

    // (vi) MFA 줄 상태 글자: 남은 시간 포맷과 각 경우. 파일에서 기기 번호·세션을 읽는 순서
    #[test]
    fn mfa_status_text_cases_and_loading() {
        let now = 1_800_000_000u64;
        let info = |serial: bool, long_term: bool, session: Option<(SessionSource, Option<u64>)>| MfaInfo {
            serial: serial.then(|| SERIAL.to_string()),
            serial_in_aws_config: serial,
            long_term,
            session,
        };
        let text = |i: &MfaInfo| mfa_status_text(&i.status(now)).0;
        assert_eq!(text(&info(true, true, Some((SessionSource::App, Some(now + 2 * 3600 + 3 * 60 + 30))))), "세션 2시간 3분 남음");
        assert_eq!(text(&info(false, false, Some((SessionSource::Profile, Some(now + 45 * 60))))), "세션 45분 남음 (프로필)");
        assert_eq!(text(&info(true, true, Some((SessionSource::Profile, None)))), "세션 사용 중 (만료 시각 모름, 프로필)");
        // 남은 시간이 60초 미만이면 세션 없음으로 본다 (프레임마다 캐시된 만료 시각으로 계산)
        let ending = info(true, true, Some((SessionSource::App, Some(now + 59))));
        assert_eq!(text(&ending), "세션 없음 — 코드를 입력하세요");
        assert!(ending.blocks_start(now));
        assert_eq!(text(&info(true, true, Some((SessionSource::App, Some(now + 61))))), "세션 1분 남음");
        assert_eq!(text(&info(false, true, None)), "MFA 미설정 (장기 키 사용)");
        assert!(!info(false, true, None).blocks_start(now));
        assert_eq!(text(&info(false, false, None)), "자격증명 없음");
        assert_eq!(text(&info(true, false, None)), "자격증명 없음");
        assert_eq!(fmt_left(12 * 3600), "12시간 0분");
        assert_eq!(fmt_left(59 * 60 + 59), "59분");

        // 파일에서 읽기: ~/.aws/config 의 mfa_serial 이 설정값보다 먼저
        let dir = tmp("load");
        let f = files(&dir, Some(CREDS), Some(SERIAL));
        let i = MfaInfo::load(&f, "default", None, "arn:aws:iam::123456789012:mfa/other", now);
        assert_eq!((i.serial.as_deref(), i.serial_in_aws_config, i.long_term, i.session), (Some(SERIAL), true, true, None));
        let f = files(&dir, Some(CREDS), None);
        let i = MfaInfo::load(&f, "default", None, " arn:aws:iam::123456789012:mfa/other ", now);
        assert_eq!((i.serial.as_deref(), i.serial_in_aws_config), (Some("arn:aws:iam::123456789012:mfa/other"), false));
        assert_eq!(MfaInfo::load(&f, "default", None, "", now).serial, None);
        // 세션 프로필(만료 전)도 유효 세션
        std::fs::write(
            dir.join("credentials"),
            format!("{CREDS}\n[example-session]\naws_access_key_id = ASIAEXAMPLE\naws_secret_access_key = s\naws_session_token = t\naws_session_expiration = {}\n", iso(now + 600)),
        )
        .unwrap();
        let i = MfaInfo::load(&f, "default", Some("example-session"), "", now);
        assert_eq!(i.session, Some((SessionSource::Profile, Some(now + 600))));
        // 자격증명 파일이 없으면 장기 키 없음
        let empty = S3Files { credentials: Some(dir.join("none")), aws_config: None, session: None };
        assert_eq!(MfaInfo::load(&empty, "default", None, "", now).status(now), MfaStatus::NoCreds);

        // 설정: s3_mfa_serial 저장·복원, 옛 설정에는 없음
        let app = App { s3_mfa_serial: SERIAL.into(), ..App::default() };
        let v: serde_json::Value = serde_json::from_str(&app.config_json()).unwrap();
        let mut loaded = App::default();
        loaded.apply_config(&v);
        assert_eq!(loaded.s3_mfa_serial, SERIAL);
        assert_eq!(loaded.config_json(), app.config_json());
        let mut old = App::default();
        old.apply_config(&serde_json::from_str(r#"{"bucket":"b"}"#).unwrap());
        assert_eq!(old.s3_mfa_serial, "");
    }
}
