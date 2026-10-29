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
}

impl Default for App {
    fn default() -> Self {
        Self { input: String::new(), folder: None, items: Default::default(), concurrent: 3, segments: 4 }
    }
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([720.0, 580.0]),
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

impl App {
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
