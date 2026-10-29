use eframe::egui;
use reqwest::blocking::{Client, Response};
use reqwest::header::{HeaderName, CONTENT_RANGE, ETAG, RANGE};
use reqwest::StatusCode;
use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

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
}

struct Item {
    line: usize,
    url: String,
    name: String,
    status: Status,
    selected: bool,
}

type Shared = Arc<Mutex<Vec<Item>>>;

struct App {
    input: String,
    folder: Option<PathBuf>,
    items: Shared,
    /// 한 번에 받는 파일 수
    concurrent: usize,
    /// 파일 하나를 몇 개 연결로 나눠 받을지
    segments: usize,
    help_open: bool,
    about_open: bool,
    /// 도움말에서 목차를 눌러 이동할 절 번호
    help_jump: Option<usize>,
    update: Arc<Mutex<UpdateState>>,
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
            help_open: false,
            about_open: false,
            help_jump: None,
            update: Arc::new(Mutex::new(UpdateState::Checking)),
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
        "URL Downloader",
        options,
        Box::new(|cc| {
            setup_fonts(&cc.egui_ctx);
            let app = App::default();
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
        let running = self.is_running();
        if running {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }

        egui::TopBottomPanel::top("menu").show(ctx, |ui| {
            egui::menu::bar(ui, |ui| {
                ui.menu_button("정보", |ui| {
                    if ui.button("URL Downloader 정보").clicked() {
                        self.about_open = true;
                        ui.close_menu();
                    }
                    if ui.button("GitHub 저장소 열기").clicked() {
                        ctx.open_url(egui::OpenUrl::new_tab(REPO_URL));
                        ui.close_menu();
                    }
                });
                ui.menu_button("도움말", |ui| {
                    if ui.button("사용법 전체 보기").clicked() {
                        self.help_open = true;
                        self.help_jump = Some(0);
                        ui.close_menu();
                    }
                    ui.separator();
                    for (i, sec) in HELP.iter().enumerate() {
                        if ui.button(format!("{}. {}", i + 1, sec.title)).clicked() {
                            self.help_open = true;
                            self.help_jump = Some(i);
                            ui.close_menu();
                        }
                    }
                });
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    self.update_bar(ui);
                });
            });
        });
        self.help_window(ctx);
        self.about_window(ctx);

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("링크 목록 (한 줄에 하나)");
            ui.label(
                egui::RichText::new("형식: URL  또는  파일명<Tab>URL  /  파일명 | URL   (확장자는 원본 그대로)")
                    .color(egui::Color32::GRAY)
                    .small(),
            );
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
                                egui::RichText::new(gutter).text_style(mono.clone()).color(egui::Color32::GRAY),
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
            ui.horizontal(|ui| {
                let urls = parse_urls(&self.input);
                let btn = ui.add_enabled(
                    !running && !urls.is_empty(),
                    egui::Button::new(format!("다운로드 ({}개)", urls.len())),
                );
                if btn.clicked() {
                    if let Some(dir) = rfd::FileDialog::new()
                        .set_title("저장할 폴더 선택")
                        .pick_folder()
                    {
                        self.folder = Some(dir.clone());
                        self.start(urls, dir);
                    }
                }
                if ui.add_enabled(!running, egui::Button::new("지우기")).clicked() {
                    self.input.clear();
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
            if let Some(dir) = self.folder.clone() {
                ui.horizontal(|ui| {
                    if ui.button("폴더 열기").clicked() {
                        let _ = std::process::Command::new("open").arg(&dir).spawn();
                    }
                    ui.label(egui::RichText::new(dir.display().to_string()).color(egui::Color32::GRAY));
                });
            }

            ui.add_space(8.0);
            ui.separator();

            let mut retry = false;
            {
                let mut items = self.items.lock().unwrap();
                let total = items.len();
                let finished = items.iter().filter(|i| matches!(i.status, Status::Finished)).count();
                let failed = items.iter().filter(|i| matches!(i.status, Status::Failed(_))).count();
                let selected = items.iter().filter(|i| i.selected).count();
                if total > 0 {
                    let (text, color) = if running {
                        (format!("진행 중  {} / {}", finished + failed, total), egui::Color32::LIGHT_BLUE)
                    } else if failed == 0 {
                        (format!("✅ 완료  {} / {}", finished, total), egui::Color32::LIGHT_GREEN)
                    } else {
                        (format!("⚠ 완료 {}개, 실패 {}개", finished, failed), egui::Color32::LIGHT_RED)
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
                            if ui
                                .add_enabled(!running && selected > 0, egui::Button::new(format!("선택한 것만 다시 받기 ({selected}개)")))
                                .clicked()
                            {
                                retry = true;
                            }
                            if ui
                                .add_enabled(!running && failed > 0, egui::Button::new(format!("실패한 것만 다시 받기 ({failed}개)")))
                                .clicked()
                            {
                                for i in items.iter_mut() {
                                    i.selected = matches!(i.status, Status::Failed(_));
                                }
                                retry = true;
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
                for item in items.iter_mut() {
                    let (mark, color) = match &item.status {
                        Status::Waiting => ("⏳", egui::Color32::GRAY),
                        Status::Downloading { .. } => ("⬇", egui::Color32::LIGHT_BLUE),
                        Status::Finished => ("✅", egui::Color32::LIGHT_GREEN),
                        Status::Failed(_) => ("❌", egui::Color32::LIGHT_RED),
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
                        ui.label(egui::RichText::new(&item.name).strong());
                        ui.label(
                            egui::RichText::new(shorten(&item.url, 60))
                                .color(egui::Color32::GRAY)
                                .small(),
                        )
                        .on_hover_text(&item.url);
                    });
                    ui.horizontal(|ui| {
                        ui.add_space(54.0);
                        match &item.status {
                            Status::Downloading { done, total, parts } => {
                                let suffix = if *parts > 1 { format!("  ·  {parts}분할") } else { String::new() };
                                let bar = match total {
                                    Some(t) if *t > 0 => egui::ProgressBar::new(*done as f32 / *t as f32)
                                        .text(format!("{} / {}{suffix}", fmt_bytes(*done), fmt_bytes(*t))),
                                    _ => egui::ProgressBar::new(0.0)
                                        .animate(true)
                                        .text(fmt_bytes(*done)),
                                };
                                ui.add(bar);
                            }
                            Status::Finished => {
                                ui.add(egui::ProgressBar::new(1.0).fill(egui::Color32::from_rgb(60, 160, 90)).text("완료"));
                            }
                            Status::Failed(e) => {
                                ui.colored_label(egui::Color32::LIGHT_RED, e);
                            }
                            Status::Waiting => {
                                ui.add(egui::ProgressBar::new(0.0).text("대기"));
                            }
                        }
                    });
                    ui.add_space(6.0);
                }
            });
        });
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
    Code(&'static str),
}
use Block::{Bullets, Code, Text};

const HELP: &[Section] = &[
    Section {
        title: "시작하기",
        blocks: &[
            Text("URL 목록을 붙여넣고 저장할 폴더만 고르면 한꺼번에 내려받는 프로그램입니다. 큰 파일은 여러 연결로 나눠 받고, 끊기면 받은 곳부터 이어서 받습니다."),
            Bullets(&[
                "① 위쪽 입력창에 링크를 한 줄에 하나씩 붙여넣습니다.",
                "② 필요하면 [동시 파일] / [파일당 분할] 숫자를 조정합니다.",
                "③ [다운로드 (N개)] 버튼을 누르고 저장할 폴더를 고릅니다.",
                "④ 아래 목록에서 진행 상황을 확인합니다. 끝나면 [폴더 열기] 로 바로 열 수 있습니다.",
            ]),
            Text("버튼의 N 은 입력창에서 URL 로 인식된 줄 수입니다. http:// 또는 https:// 로 시작하지 않는 줄은 무시됩니다."),
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
            ]),
        ],
    },
    Section {
        title: "진행 상황 보기",
        blocks: &[
            Bullets(&[
                "⏳ 대기: 차례를 기다리는 중",
                "⬇ 진행: 받는 중. 진행바에 받은 크기 / 전체 크기가 표시되고, 분할 중이면 'N분할' 이 함께 보입니다.",
                "✅ 완료: 최종 파일명으로 저장됨",
                "❌ 실패: 사유가 빨간 글씨로 표시되고 체크박스가 자동으로 켜집니다.",
            ]),
            Text("목록 위의 요약 줄에는 전체 / 완료 / 실패 개수가 표시됩니다. 전체 크기를 알려주지 않는 서버는 진행바 대신 받은 크기만 움직입니다."),
        ],
    },
    Section {
        title: "다시 받기",
        blocks: &[
            Bullets(&[
                "[실패한 것만 다시 받기]: 실패 항목을 모두 골라 다시 받습니다.",
                "[선택한 것만 다시 받기]: 체크박스를 켠 항목만 다시 받습니다. 맨 위 체크박스로 전체 선택/해제할 수 있습니다.",
                "다시 받기는 처음 고른 폴더에 저장합니다. 이어받기 기록이 있으면 받은 곳부터 이어서 받습니다(다음 절 참고).",
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
                "연결이 끊기면 구간별로 받은 지점부터 2초 간격으로 3회 자동 재시도합니다.",
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
                "같은 이름의 파일이 폴더에 이미 있거나 목록 안에서 겹치면 '이름 (1).jpg', '이름 (2).jpg' 처럼 번호를 붙입니다.",
            ]),
        ],
    },
    Section {
        title: "업데이트",
        blocks: &[
            Bullets(&[
                "앱을 켤 때 GitHub 릴리스를 확인해 새 버전이 있으면 상단 오른쪽에 [⬆ 업데이트 vX.Y.Z] 버튼이 나타납니다. 현재 버전은 그 옆에 항상 표시됩니다.",
                "버튼을 누르면 새 버전을 받아 지금 실행 중인 .app 을 같은 자리에서 교체하고 자동으로 다시 시작합니다. 진행 중인 다운로드는 끊기므로 끝난 뒤에 누르세요.",
                "[정보 → URL Downloader 정보] 창에서 [업데이트 확인] 으로 언제든 다시 확인할 수 있습니다.",
                "cargo run 처럼 .app 번들이 아닌 상태로 실행 중이면 업데이트 버튼은 동작하지 않습니다.",
                "교체에 실패하면 기존 앱을 그대로 되돌리고 상단에 ⚠ 표시가 뜹니다. 마우스를 올리면 사유가 보입니다.",
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
                "'연결 끊김': 3회 재시도 후에도 끊긴 상태입니다. 네트워크를 확인하고 [실패한 것만 다시 받기] 를 누르면 이어서 받습니다.",
                "폴더에 .part / .meta 파일이 남아 있다면 받다 만 흔적입니다. 다시 받을 계획이 없으면 지워도 됩니다.",
                "버그 제보나 제안은 [정보 → GitHub 저장소 열기] 의 Issues 에 남겨 주세요.",
            ]),
        ],
    },
];

impl App {
    fn help_window(&mut self, ctx: &egui::Context) {
        let mut open = self.help_open;
        egui::Window::new("도움말")
            .open(&mut open)
            .default_size([680.0, 500.0])
            .show(ctx, |ui| {
                ui.horizontal_top(|ui| {
                    ui.vertical(|ui| {
                        ui.set_width(150.0);
                        ui.strong("목차");
                        ui.add_space(4.0);
                        for (i, sec) in HELP.iter().enumerate() {
                            if ui.link(format!("{}. {}", i + 1, sec.title)).clicked() {
                                self.help_jump = Some(i);
                            }
                        }
                    });
                    ui.separator();
                    egui::ScrollArea::vertical()
                        .id_salt("help")
                        .max_height(ui.available_height())
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            for (i, sec) in HELP.iter().enumerate() {
                                let heading = ui.heading(format!("{}. {}", i + 1, sec.title));
                                if self.help_jump == Some(i) {
                                    heading.scroll_to_me(Some(egui::Align::TOP));
                                    self.help_jump = None;
                                }
                                ui.add_space(4.0);
                                for block in sec.blocks {
                                    match block {
                                        Text(t) => {
                                            ui.label(*t);
                                        }
                                        Bullets(items) => {
                                            for it in items.iter() {
                                                ui.horizontal_wrapped(|ui| {
                                                    ui.label("•");
                                                    ui.label(*it);
                                                });
                                            }
                                        }
                                        Code(c) => {
                                            egui::Frame::group(ui.style()).show(ui, |ui| {
                                                ui.set_width(ui.available_width());
                                                ui.label(egui::RichText::new(*c).monospace());
                                            });
                                        }
                                    }
                                    ui.add_space(4.0);
                                }
                                ui.add_space(14.0);
                            }
                        });
                });
            });
        self.help_open = open;
    }

    fn about_window(&mut self, ctx: &egui::Context) {
        let mut open = self.about_open;
        egui::Window::new("URL Downloader 정보")
            .open(&mut open)
            .resizable(false)
            .collapsible(false)
            .show(ctx, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(6.0);
                    ui.heading("URL Downloader");
                    ui.label(format!("버전 {VERSION}"));
                    ui.add_space(4.0);
                    let state = self.update.lock().unwrap().clone();
                    match state {
                        UpdateState::UpToDate => {
                            ui.label(egui::RichText::new("최신 버전입니다").color(egui::Color32::LIGHT_GREEN));
                        }
                        UpdateState::Available { version, .. } => {
                            ui.label(egui::RichText::new(format!("새 버전 v{version} 이 있습니다 (상단 버튼으로 업데이트)")).color(egui::Color32::LIGHT_BLUE));
                        }
                        UpdateState::Error(e) => {
                            ui.label(egui::RichText::new(format!("업데이트 확인 실패: {e}")).color(egui::Color32::GRAY).small());
                        }
                        _ => {}
                    }
                    if ui.button("업데이트 확인").clicked() {
                        self.check_update();
                    }
                    ui.add_space(8.0);
                    ui.label("URL 목록을 붙여넣고 폴더만 고르면 한꺼번에 내려받는 프로그램.");
                    ui.label("분할 다운로드와 이어받기를 지원합니다.");
                    ui.add_space(8.0);
                    ui.hyperlink_to("GitHub: bluelang1/url-downloader", REPO_URL);
                    ui.add_space(8.0);
                    ui.label(egui::RichText::new("MIT License  ·  © 2026 bluelang1").color(egui::Color32::GRAY));
                    ui.label(
                        egui::RichText::new("Rust · egui/eframe · reqwest · rfd")
                            .color(egui::Color32::GRAY)
                            .small(),
                    );
                    ui.add_space(6.0);
                });
            });
        self.about_open = open;
    }

    fn is_running(&self) -> bool {
        self.items.lock().unwrap().iter().any(|i| {
            matches!(i.status, Status::Waiting | Status::Downloading { .. })
        })
    }

    fn start(&mut self, urls: Vec<Entry>, dir: PathBuf) {
        let mut names: Vec<String> = Vec::new();
        let items: Vec<Item> = urls
            .iter()
            .map(|e| {
                let name = unique_name(&dir, &target_name(e), &mut names);
                Item { line: e.line, url: e.url.clone(), name, status: Status::Waiting, selected: false }
            })
            .collect();
        *self.items.lock().unwrap() = items;
        self.spawn_workers((0..urls.len()).collect(), dir);
    }

    /// 선택된 항목만 같은 폴더에 다시 받는다 (.meta 가 남아 있으면 이어받기)
    fn retry_selected(&mut self) {
        let Some(dir) = self.folder.clone() else { return };
        let idxs: Vec<usize> = {
            let mut items = self.items.lock().unwrap();
            let idxs: Vec<usize> = items
                .iter()
                .enumerate()
                .filter(|(_, i)| i.selected)
                .map(|(i, _)| i)
                .collect();
            for &i in &idxs {
                items[i].status = Status::Waiting;
                items[i].selected = false;
            }
            idxs
        };
        self.spawn_workers(idxs, dir);
    }

    /// 작업 큐에서 하나씩 꺼내 받는 워커를 `concurrent` 개만 띄운다
    fn spawn_workers(&self, idxs: Vec<usize>, dir: PathBuf) {
        let workers = self.concurrent.clamp(1, idxs.len().max(1));
        let queue = Arc::new(Mutex::new(VecDeque::from(idxs)));
        let segments = self.segments.max(1);
        for _ in 0..workers {
            let shared = Arc::clone(&self.items);
            let queue = Arc::clone(&queue);
            let dir = dir.clone();
            thread::spawn(move || loop {
                let next = queue.lock().unwrap().pop_front();
                let Some(idx) = next else { break };
                download(Arc::clone(&shared), idx, dir.clone(), segments);
            });
        }
    }
}

impl App {
    /// 메뉴 바 오른쪽: 버전 표시와 업데이트 버튼/진행 상태
    fn update_bar(&mut self, ui: &mut egui::Ui) {
        let state = self.update.lock().unwrap().clone();
        ui.label(egui::RichText::new(format!("v{VERSION}")).color(egui::Color32::GRAY));
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
                ui.label(egui::RichText::new("⚠").color(egui::Color32::LIGHT_RED)).on_hover_text(e);
            }
            UpdateState::Checking | UpdateState::UpToDate => {}
        }
    }

    /// GitHub 최신 릴리스를 조회해 현재 버전보다 새 것이면 Available 로 바꾼다
    fn check_update(&self) {
        let state = Arc::clone(&self.update);
        *state.lock().unwrap() = UpdateState::Checking;
        thread::spawn(move || {
            let result = fetch_latest_release();
            *state.lock().unwrap() = match result {
                Ok(Some((version, url))) => UpdateState::Available { version, url },
                Ok(None) => UpdateState::UpToDate,
                Err(e) => UpdateState::Error(e),
            };
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

fn download(shared: Shared, idx: usize, dir: PathBuf, segments: usize) {
    let (url, name) = {
        let items = shared.lock().unwrap();
        (items[idx].url.clone(), items[idx].name.clone())
    };
    let set = |s: Status| shared.lock().unwrap()[idx].status = s;

    match download_file(&url, &dir.join(&name), segments, &set) {
        Ok(()) => set(Status::Finished),
        Err(e) => {
            let mut items = shared.lock().unwrap();
            items[idx].status = Status::Failed(e);
            items[idx].selected = true;
        }
    }
}

/// `dest` 옆에 `<dest>.part`(받는 중인 본문)와 `<dest>.meta`(구간별 진행 상황)를 두고 받는다.
/// 서버가 Range 를 지원하면 분할·이어받기, 아니면 처음부터 한 줄기로 받는다.
fn download_file(url: &str, dest: &Path, segments: usize, set: &(dyn Fn(Status) + Sync)) -> Result<(), String> {
    let client = Client::builder()
        .user_agent("Mozilla/5.0")
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
            Some(total) => download_ranged(&client, url, &part, &meta, total, &etag, segments, set)?,
            None => {
                // 206 인데 전체 크기를 안 알려주는 서버: 일반 GET 으로 받는다
                let resp = client.get(url).send().map_err(|e| e.to_string())?;
                if !resp.status().is_success() {
                    return Err(format!("HTTP {}", resp.status()));
                }
                stream_whole(resp, &part, set)?;
            }
        }
    } else {
        // Range 무시(200): 이 응답이 곧 전체 본문
        stream_whole(probe, &part, set)?;
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
fn stream_whole(mut resp: Response, part: &Path, set: &(dyn Fn(Status) + Sync)) -> Result<(), String> {
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
    set: &(dyn Fn(Status) + Sync),
) -> Result<(), String> {
    // 같은 파일(크기·ETag 일치)의 진행 기록이 남아 있으면 거기서부터 이어받는다
    let resumed = load_meta(meta_path).filter(|m| m.total == total && m.etag == etag && part.exists());
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
    let save = || save_meta(meta_path, total, etag, &segs);
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
            .map(|seg| sc.spawn(move || fetch_segment(client, url, file, seg, tick)))
            .collect();
        handles
            .into_iter()
            .filter_map(|h| h.join().unwrap_or_else(|_| Err("스레드 오류".into())).err())
            .collect()
    });
    if let Some(e) = errors.into_iter().next() {
        let _ = save();
        return Err(e);
    }
    let _ = std::fs::remove_file(meta_path);
    Ok(())
}

/// 구간 하나를 받는다. 끊기면 받은 곳부터 다시 요청한다
fn fetch_segment(client: &Client, url: &str, file: &File, seg: &Seg, tick: &(dyn Fn() + Sync)) -> Result<(), String> {
    let len = seg.end - seg.start;
    let mut last_err = String::new();
    for attempt in 0..RETRIES {
        if seg.done.load(Relaxed) >= len {
            return Ok(());
        }
        if attempt > 0 {
            thread::sleep(Duration::from_secs(2));
        }
        match fetch_once(client, url, file, seg, tick) {
            Ok(()) => return Ok(()),
            Err(e) => last_err = e,
        }
    }
    Err(last_err)
}

fn fetch_once(client: &Client, url: &str, file: &File, seg: &Seg, tick: &(dyn Fn() + Sync)) -> Result<(), String> {
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
    }
    if done < len {
        return Err(format!("연결 끊김 ({} / {})", fmt_bytes(done), fmt_bytes(len)));
    }
    Ok(())
}

fn save_meta(path: &Path, total: u64, etag: &str, segs: &[Seg]) -> Result<(), String> {
    let mut s = format!("{total}\n{etag}\n");
    for seg in segs {
        s += &format!("{} {} {}\n", seg.start, seg.end, seg.done.load(Relaxed));
    }
    let tmp = path.with_extension("meta.tmp");
    std::fs::write(&tmp, s)
        .and_then(|_| std::fs::rename(&tmp, path))
        .map_err(|e| e.to_string())
}

fn load_meta(path: &Path) -> Option<Meta> {
    let s = std::fs::read_to_string(path).ok()?;
    let mut lines = s.lines();
    let total = lines.next()?.trim().parse().ok()?;
    let etag = lines.next()?.trim().to_string();
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
        Some(n) => {
            let ext = match original.rfind('.') {
                Some(p) if p > 0 => &original[p..],
                _ => "",
            };
            format!("{n}{ext}")
        }
        None => original,
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

fn sanitize(name: &str) -> String {
    name.trim()
        .chars()
        .map(|c| if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') { '_' } else { c })
        .collect()
}

fn unique_name(dir: &Path, name: &str, used: &mut Vec<String>) -> String {
    let (stem, ext) = match name.rfind('.') {
        Some(p) if p > 0 => (&name[..p], &name[p..]),
        _ => (name, ""),
    };
    let mut candidate = name.to_string();
    let mut n = 1;
    while used.contains(&candidate) || dir.join(&candidate).exists() {
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

    fn item(url: &str, name: &str) -> Shared {
        Arc::new(Mutex::new(vec![Item {
            line: 1,
            selected: false,
            url: url.into(),
            name: name.into(),
            status: Status::Waiting,
        }]))
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
        assert_eq!(target_name(&e(2, "https://a.com/x/photo.jpg", None)), "photo.jpg");
        let mut used = vec![];
        let d = std::env::temp_dir();
        assert_eq!(unique_name(&d, "a.jpg", &mut used), "a.jpg");
        assert_eq!(unique_name(&d, "a.jpg", &mut used), "a (1).jpg");
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
        let shared = item("https://www.rust-lang.org/this-does-not-exist-404", "x");
        download(Arc::clone(&shared), 0, tmp("404"), 4);
        let items = shared.lock().unwrap();
        assert!(matches!(items[0].status, Status::Failed(_)));
        assert!(items[0].selected);
    }

    #[test]
    fn downloads_real_file() {
        let dir = tmp("real");
        let shared = item("https://www.rust-lang.org/static/images/rust-logo-blk.svg", "logo.svg");
        download(Arc::clone(&shared), 0, dir.clone(), 4);
        let st = shared.lock().unwrap()[0].status.clone();
        assert!(matches!(st, Status::Finished), "status not finished");
        assert!(std::fs::metadata(dir.join("logo.svg")).unwrap().len() > 0);
        assert!(!dir.join("logo.svg.part").exists());
        assert!(!dir.join("logo.svg.meta").exists());
    }

    // ---- Range 를 지원하는 작은 테스트 서버. `fails` 가 0 보다 크면 본문 1000 바이트 뒤에 연결을 끊는다 ----
    fn serve(data: Arc<Vec<u8>>, fails: usize) -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let fails = Arc::new(AtomicUsize::new(fails));
        let f = Arc::clone(&fails);
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (data, f) = (Arc::clone(&data), Arc::clone(&f));
                thread::spawn(move || handle(stream, data, f));
            }
        });
        (format!("http://127.0.0.1:{port}/big.bin"), fails)
    }

    fn handle(mut s: TcpStream, data: Arc<Vec<u8>>, fails: Arc<AtomicUsize>) {
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
        let _ = s.write_all(if cut { &body[..1000] } else { body });
        let _ = s.shutdown(Shutdown::Both);
    }

    fn big_data() -> Arc<Vec<u8>> {
        Arc::new((0..3_500_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect())
    }

    #[test]
    fn segmented_download_survives_transient_disconnects() {
        let data = big_data();
        let (url, _fails) = serve(Arc::clone(&data), 2); // 처음 두 요청만 끊김 → 자동 재시도로 완료
        let dir = tmp("seg");
        let shared = item(&url, "big.bin");
        download(Arc::clone(&shared), 0, dir.clone(), 4);
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
        let shared = item(&url, "big.bin");
        download(Arc::clone(&shared), 0, dir.clone(), 4);
        assert!(matches!(shared.lock().unwrap()[0].status, Status::Failed(_)));
        assert!(dir.join("big.bin.part").exists());
        let meta = load_meta(&dir.join("big.bin.meta")).expect("meta saved");
        assert_eq!(meta.segs.len(), 3); // 3.5MB / 1MB 최소 구간 → 3분할
        assert!(meta.segs.iter().all(|(_, _, done)| *done > 0), "부분 수신이 기록돼야 한다");

        fails.store(0, SeqCst); // 서버 정상화 후 다시 받기 → 받은 곳부터 이어서 완료
        download(Arc::clone(&shared), 0, dir.clone(), 4);
        assert!(matches!(shared.lock().unwrap()[0].status, Status::Finished));
        assert_eq!(std::fs::read(dir.join("big.bin")).unwrap(), *data);
        assert!(!dir.join("big.bin.part").exists());
        assert!(!dir.join("big.bin.meta").exists());
    }
}
