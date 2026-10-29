use eframe::egui;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;

#[derive(Clone)]
enum Status {
    Waiting,
    Downloading { done: u64, total: Option<u64> },
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

#[derive(Default)]
struct App {
    input: String,
    folder: Option<PathBuf>,
    items: Shared,
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([640.0, 560.0]),
        ..Default::default()
    };
    eframe::run_native(
        "URL Downloader",
        options,
        Box::new(|cc| {
            setup_fonts(&cc.egui_ctx);
            Ok(Box::new(App::default()))
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

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("링크 목록 (한 줄에 하나)");
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
                                .hint_text("https://... 링크를 붙여넣으세요"),
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
                if let Some(dir) = self.folder.clone() {
                    if ui.button("폴더 열기").clicked() {
                        let _ = std::process::Command::new("open").arg(&dir).spawn();
                    }
                    ui.label(egui::RichText::new(dir.display().to_string()).color(egui::Color32::GRAY));
                }
            });

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
                            Status::Downloading { done, total } => {
                                let bar = match total {
                                    Some(t) if *t > 0 => egui::ProgressBar::new(*done as f32 / *t as f32)
                                        .text(format!("{} / {}", fmt_bytes(*done), fmt_bytes(*t))),
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

impl App {
    fn is_running(&self) -> bool {
        self.items.lock().unwrap().iter().any(|i| {
            matches!(i.status, Status::Waiting | Status::Downloading { .. })
        })
    }

    fn start(&mut self, urls: Vec<(usize, String)>, dir: PathBuf) {
        let mut names: Vec<String> = Vec::new();
        let items: Vec<Item> = urls
            .iter()
            .map(|(line, url)| {
                let name = unique_name(&dir, &file_name_from_url(url, *line), &mut names);
                Item { line: *line, url: url.clone(), name, status: Status::Waiting, selected: false }
            })
            .collect();
        *self.items.lock().unwrap() = items;

        for idx in 0..urls.len() {
            let shared = Arc::clone(&self.items);
            let dir = dir.clone();
            thread::spawn(move || download(shared, idx, dir));
        }
    }
}

impl App {
    /// 선택된 항목만 같은 폴더에 다시 받는다
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
        for idx in idxs {
            let shared = Arc::clone(&self.items);
            let dir = dir.clone();
            thread::spawn(move || download(shared, idx, dir));
        }
    }
}

fn download(shared: Shared, idx: usize, dir: PathBuf) {
    let (url, name) = {
        let items = shared.lock().unwrap();
        (items[idx].url.clone(), items[idx].name.clone())
    };
    let set = |s: Status| shared.lock().unwrap()[idx].status = s;

    let result = (|| -> Result<(), String> {
        let client = reqwest::blocking::Client::builder()
            .user_agent("Mozilla/5.0")
            .build()
            .map_err(|e| e.to_string())?;
        let mut resp = client.get(&url).send().map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("HTTP {}", resp.status()));
        }
        let total = resp.content_length();
        set(Status::Downloading { done: 0, total });

        let mut file = File::create(dir.join(&name)).map_err(|e| e.to_string())?;
        let mut buf = [0u8; 64 * 1024];
        let mut done = 0u64;
        loop {
            let n = resp.read(&mut buf).map_err(|e| e.to_string())?;
            if n == 0 {
                break;
            }
            file.write_all(&buf[..n]).map_err(|e| e.to_string())?;
            done += n as u64;
            set(Status::Downloading { done, total });
        }
        Ok(())
    })();

    match result {
        Ok(()) => set(Status::Finished),
        Err(e) => {
            let mut items = shared.lock().unwrap();
            items[idx].status = Status::Failed(e);
            items[idx].selected = true;
        }
    }
}

/// (입력창 줄 번호(1부터), URL)
fn parse_urls(input: &str) -> Vec<(usize, String)> {
    input
        .lines()
        .enumerate()
        .map(|(i, l)| (i + 1, l.trim()))
        .filter(|(_, l)| l.starts_with("http://") || l.starts_with("https://"))
        .map(|(n, l)| (n, l.to_string()))
        .collect()
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
    let cleaned: String = decoded
        .chars()
        .map(|c| if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') { '_' } else { c })
        .collect();
    if cleaned.is_empty() {
        format!("file_{idx}")
    } else {
        cleaned
    }
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
    if b >= KB * KB {
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

    #[test]
    fn names() {
        assert_eq!(file_name_from_url("https://a.com/x/y/photo%20one.jpg?x=1", 0), "photo one.jpg");
        assert_eq!(file_name_from_url("https://a.com/", 3), "file_3");
        assert_eq!(parse_urls("https://a\n\nfoo\nhttps://b"), vec![(1, "https://a".into()), (4, "https://b".into())]);
        let mut used = vec![];
        let d = std::env::temp_dir();
        assert_eq!(unique_name(&d, "a.jpg", &mut used), "a.jpg");
        assert_eq!(unique_name(&d, "a.jpg", &mut used), "a (1).jpg");
    }

    #[test]
    fn failed_download_is_auto_selected() {
        let shared: Shared = Arc::new(Mutex::new(vec![Item {
            line: 1,
            selected: false,
            url: "https://www.rust-lang.org/this-does-not-exist-404".into(),
            name: "x".into(),
            status: Status::Waiting,
        }]));
        download(Arc::clone(&shared), 0, std::env::temp_dir());
        let items = shared.lock().unwrap();
        assert!(matches!(items[0].status, Status::Failed(_)));
        assert!(items[0].selected);
    }

    #[test]
    fn downloads_real_file() {
        let dir = std::env::temp_dir().join(format!("ud_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let shared: Shared = Arc::new(Mutex::new(vec![Item {
            line: 1,
            selected: false,
            url: "https://www.rust-lang.org/static/images/rust-logo-blk.svg".into(),
            name: "logo.svg".into(),
            status: Status::Waiting,
        }]));
        download(Arc::clone(&shared), 0, dir.clone());
        let st = shared.lock().unwrap()[0].status.clone();
        assert!(matches!(st, Status::Finished), "status not finished");
        assert!(std::fs::metadata(dir.join("logo.svg")).unwrap().len() > 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
