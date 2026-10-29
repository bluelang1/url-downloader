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
    url: String,
    name: String,
    status: Status,
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
            egui::ScrollArea::vertical()
                .id_salt("input")
                .max_height(180.0)
                .show(ui, |ui| {
                    ui.add_sized(
                        [ui.available_width(), 170.0],
                        egui::TextEdit::multiline(&mut self.input)
                            .hint_text("https://... 링크를 붙여넣으세요"),
                    );
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
                if let Some(dir) = &self.folder {
                    ui.label(format!("→ {}", dir.display()));
                }
            });

            ui.add_space(8.0);
            ui.separator();

            let items = self.items.lock().unwrap();
            let finished = items.iter().filter(|i| matches!(i.status, Status::Finished)).count();
            let failed = items.iter().filter(|i| matches!(i.status, Status::Failed(_))).count();
            if !items.is_empty() {
                let text = if running {
                    format!("진행 중  {} / {}", finished + failed, items.len())
                } else if failed == 0 {
                    format!("✅ 완료  {} / {}", finished, items.len())
                } else {
                    format!("⚠ 완료 {}개, 실패 {}개", finished, failed)
                };
                ui.strong(text);
                ui.add_space(4.0);
            }

            egui::ScrollArea::vertical().id_salt("list").show(ui, |ui| {
                for item in items.iter() {
                    ui.horizontal(|ui| {
                        ui.set_width(ui.available_width());
                        let (mark, color) = match &item.status {
                            Status::Waiting => ("⏳", egui::Color32::GRAY),
                            Status::Downloading { .. } => ("⬇", egui::Color32::LIGHT_BLUE),
                            Status::Finished => ("✅", egui::Color32::LIGHT_GREEN),
                            Status::Failed(_) => ("❌", egui::Color32::LIGHT_RED),
                        };
                        ui.colored_label(color, mark);
                        ui.label(&item.name).on_hover_text(&item.url);
                    });
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
                            ui.add(egui::ProgressBar::new(1.0).text("완료"));
                        }
                        Status::Failed(e) => {
                            ui.colored_label(egui::Color32::LIGHT_RED, e);
                        }
                        Status::Waiting => {
                            ui.add(egui::ProgressBar::new(0.0).text("대기"));
                        }
                    }
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

    fn start(&mut self, urls: Vec<String>, dir: PathBuf) {
        let mut names: Vec<String> = Vec::new();
        let items: Vec<Item> = urls
            .iter()
            .enumerate()
            .map(|(i, url)| {
                let name = unique_name(&dir, &file_name_from_url(url, i), &mut names);
                Item { url: url.clone(), name, status: Status::Waiting }
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
        Err(e) => set(Status::Failed(e)),
    }
}

fn parse_urls(input: &str) -> Vec<String> {
    input
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with("http://") || l.starts_with("https://"))
        .map(String::from)
        .collect()
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
        format!("file_{}", idx + 1)
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
        assert_eq!(file_name_from_url("https://a.com/", 2), "file_3");
        let mut used = vec![];
        let d = std::env::temp_dir();
        assert_eq!(unique_name(&d, "a.jpg", &mut used), "a.jpg");
        assert_eq!(unique_name(&d, "a.jpg", &mut used), "a (1).jpg");
    }

    #[test]
    fn downloads_real_file() {
        let dir = std::env::temp_dir().join(format!("ud_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let shared: Shared = Arc::new(Mutex::new(vec![Item {
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
