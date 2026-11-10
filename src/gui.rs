//! egui 메인 창: 설정, 영역 선택 오버레이, 캡처 진행 표시, 결과 미리보기
//!
//! 흐름
//! 1. 대기: 설정을 고르고 "스크롤 캡처 시작"
//! 2. 선택: 메인 창이 있는 모니터를 캡처하고, 메인 창을 전체 화면으로 바꿔 그 위에서 드래그
//! 3. 캡처: 메인 창을 최소화하고, 백그라운드 스레드에서 capture::run_capture 실행
//! 4. 완료: 창을 되살려 결과 미리보기, 저장/복사
//!
//! 메인 창은 시작할 때 "화면 캡처에서 제외"(ContentProtected)로 설정해서,
//! 스크린샷에 자기 자신이 찍히지 않게 한다.

use std::borrow::Cow;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result};
use eframe::egui::{
    self, Color32, FontId, Pos2, Rect, Stroke, StrokeKind, TextureHandle, TextureOptions,
    ViewportCommand, WindowLevel, pos2, vec2,
};
use xcap::Monitor;
use xcap::image::RgbaImage;

use crate::capture::{self, CaptureOptions, Selection};

/// 프로그램 시작: 메인 창을 띄운다
pub fn run() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Scroll Capture")
            .with_inner_size([560.0, 640.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Scroll Capture",
        options,
        Box::new(|cc| Ok(Box::new(App::new(&cc.egui_ctx)))),
    )
}

/// 프로그램의 현재 단계
enum State {
    /// 대기 중 (설정 화면)
    Idle,
    /// 전체 화면 오버레이에서 영역 선택 중
    Selecting(Overlay),
    /// 백그라운드 스레드에서 캡처 중
    Capturing {
        /// 캡처 결과를 받을 채널
        rx: Receiver<Result<RgbaImage>>,
        /// 지금까지 이어 붙인 높이 (스레드가 갱신)
        height: Arc<AtomicU32>,
    },
    /// 캡처 완료, 결과 미리보기
    Done(Preview),
}

/// 영역 선택 오버레이의 상태
struct Overlay {
    /// 모니터 스크린샷 (화면에 그릴 텍스처)
    texture: TextureHandle,
    /// 모니터 왼쪽 위 (화면 절대 좌표, 물리 픽셀)
    origin: (i32, i32),
    /// 스크린샷 크기 (물리 픽셀)
    size: (u32, u32),
    /// 드래그 시작점 (egui 좌표). 드래그 중이 아니면 None
    drag_start: Option<Pos2>,
}

/// 오버레이에서 사용자가 한 일
enum OverlayAction {
    Selected(Selection),
    Cancelled,
}

/// 캡처 결과 미리보기
struct Preview {
    image: RgbaImage,
    /// GPU 텍스처는 한 변의 최대 크기가 정해져 있어서, 긴 이미지를 여러 조각으로 나눠 올린다.
    /// (텍스처, 조각 높이 px). 처음 그릴 때 만든다.
    tiles: Option<Vec<(TextureHandle, u32)>>,
}

pub struct App {
    state: State,
    options: CaptureOptions,
    status: String,
}

impl App {
    fn new(ctx: &egui::Context) -> Self {
        setup_korean_font(ctx);
        // 메인 창이 스크린샷에 찍히지 않게 (Windows 10 2004 이상)
        ctx.send_viewport_cmd(ViewportCommand::ContentProtected(true));
        Self {
            state: State::Idle,
            options: CaptureOptions::default(),
            status: "캡처할 프로그램을 띄워 두고 시작을 누르세요.".to_owned(),
        }
    }

    /// 대기/진행/결과 화면
    fn main_ui(&mut self, ui: &mut egui::Ui) {
        ui.heading("Scroll Capture");
        ui.add_space(8.0);

        let busy = matches!(self.state, State::Capturing { .. });
        let mut start = false;

        ui.add_enabled_ui(!busy, |ui| {
            if ui
                .add_sized([200.0, 36.0], egui::Button::new("스크롤 캡처 시작"))
                .clicked()
            {
                start = true;
            }
            ui.add_space(8.0);
            egui::Grid::new("options").num_columns(2).show(ui, |ui| {
                ui.label("스크롤 대기 (ms)");
                ui.add(
                    egui::DragValue::new(&mut self.options.delay_ms)
                        .range(50..=5000)
                        .speed(10),
                );
                ui.end_row();

                ui.label("최대 높이 (px)");
                ui.add(
                    egui::DragValue::new(&mut self.options.max_height)
                        .range(1000..=100_000)
                        .speed(100),
                );
                ui.end_row();

                ui.label("휠 칸 수");
                ui.add(egui::DragValue::new(&mut self.options.wheel_notches).range(1..=10));
                ui.end_row();
            });
        });

        ui.add_space(8.0);
        if let State::Capturing { height, .. } = &self.state {
            ui.label(format!("캡처 중... {} px", height.load(Ordering::Relaxed)));
            ui.ctx().request_repaint_after(Duration::from_millis(200));
        } else {
            ui.label(&self.status);
        }

        if start {
            self.start_selection(ui.ctx());
        }

        if let State::Done(preview) = &mut self.state {
            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("PNG로 저장").clicked() {
                    self.status = preview.save();
                }
                if ui.button("클립보드에 복사").clicked() {
                    self.status = preview.copy();
                }
            });
            ui.add_space(4.0);
            preview.show(ui);
        }
    }

    /// 메인 창이 있는 모니터를 캡처하고, 메인 창을 전체 화면 오버레이로 바꾼다
    fn start_selection(&mut self, ctx: &egui::Context) {
        match Overlay::new(ctx) {
            Ok(overlay) => {
                ctx.send_viewport_cmd(ViewportCommand::WindowLevel(WindowLevel::AlwaysOnTop));
                ctx.send_viewport_cmd(ViewportCommand::Fullscreen(true));
                ctx.send_viewport_cmd(ViewportCommand::Focus);
                self.state = State::Selecting(overlay);
            }
            Err(e) => self.status = format!("화면 캡처 실패: {e:#}"),
        }
    }

    /// 오버레이를 끝내고 원래 창으로 되돌린다
    fn leave_overlay(ctx: &egui::Context) {
        ctx.send_viewport_cmd(ViewportCommand::Fullscreen(false));
        ctx.send_viewport_cmd(ViewportCommand::WindowLevel(WindowLevel::Normal));
    }

    /// 창을 최소화하고 백그라운드 스레드에서 캡처를 시작한다
    fn start_capture(&mut self, ctx: &egui::Context, sel: Selection) {
        ctx.send_viewport_cmd(ViewportCommand::Minimized(true));

        let (tx, rx) = mpsc::channel();
        let height = Arc::new(AtomicU32::new(0));

        let thread_height = Arc::clone(&height);
        let thread_ctx = ctx.clone();
        let options = self.options.clone();

        std::thread::spawn(move || {
            // 창이 최소화되어 화면에서 사라질 때까지 기다림
            std::thread::sleep(Duration::from_millis(500));
            let result = capture::run_capture(sel, &options, |h| {
                thread_height.store(h, Ordering::Relaxed);
                thread_ctx.request_repaint();
            });
            let _ = tx.send(result);
            thread_ctx.request_repaint(); // 최소화된 상태에서도 logic()이 불리게
        });

        self.state = State::Capturing { rx, height };
    }

    /// 캡처 스레드의 결과 확인 (창이 최소화되어 있어도 호출됨)
    fn poll_capture(&mut self, ctx: &egui::Context) {
        let State::Capturing { rx, .. } = &self.state else {
            return;
        };
        let received = match rx.try_recv() {
            Ok(result) => result,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => Err(anyhow::anyhow!("캡처 스레드가 비정상 종료됨")),
        };

        ctx.send_viewport_cmd(ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(ViewportCommand::Focus);

        self.state = match received {
            Ok(image) => {
                self.status = format!("완료: {} x {} px", image.width(), image.height());
                State::Done(Preview { image, tiles: None })
            }
            Err(e) => {
                self.status = format!("캡처 실패: {e:#}");
                State::Idle
            }
        };
    }
}

impl eframe::App for App {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_capture(ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if let State::Selecting(overlay) = &mut self.state {
            let action = overlay.show(ui);
            match action {
                Some(OverlayAction::Selected(sel)) => {
                    Self::leave_overlay(ui.ctx());
                    self.start_capture(ui.ctx(), sel);
                }
                Some(OverlayAction::Cancelled) => {
                    Self::leave_overlay(ui.ctx());
                    self.status = "취소됨".to_owned();
                    self.state = State::Idle;
                }
                None => {}
            }
            return;
        }

        egui::CentralPanel::default().show(ui, |ui| self.main_ui(ui));
    }
}

impl Overlay {
    /// 메인 창이 있는 모니터(못 찾으면 주 모니터)를 캡처해서 오버레이를 준비한다
    fn new(ctx: &egui::Context) -> Result<Self> {
        let monitor = monitor_of_window(ctx).or_else(primary_monitor)?;
        let shot = monitor.capture_image().context("모니터 캡처 실패")?;
        let size = (shot.width(), shot.height());
        let image = egui::ColorImage::from_rgba_unmultiplied(
            [size.0 as usize, size.1 as usize],
            shot.as_raw(),
        );
        let texture = ctx.load_texture("overlay-screenshot", image, TextureOptions::LINEAR);
        Ok(Self {
            texture,
            origin: (monitor.x()?, monitor.y()?),
            size,
            drag_start: None,
        })
    }

    /// 스크린샷을 그리고 드래그를 처리한다. 선택이 끝나거나 취소되면 Some
    fn show(&mut self, ui: &mut egui::Ui) -> Option<OverlayAction> {
        let screen = ui.max_rect();
        ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);

        let (pressed, released, pointer, cancel) = ui.input(|i| {
            (
                i.pointer.primary_pressed(),
                i.pointer.primary_released(),
                i.pointer.interact_pos(),
                i.key_pressed(egui::Key::Escape) || i.pointer.secondary_clicked(),
            )
        });
        if cancel {
            return Some(OverlayAction::Cancelled);
        }
        if pressed {
            self.drag_start = pointer;
        }
        let sel = match (self.drag_start, pointer) {
            (Some(a), Some(b)) => Some(Rect::from_two_pos(a, b).intersect(screen)),
            _ => None,
        };

        // 그리기: 스크린샷 → 선택 영역 바깥 어둡게 → 빨간 테두리 → 안내 문구
        let painter = ui.painter();
        let full_uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
        painter.image(self.texture.id(), screen, full_uv, Color32::WHITE);

        let dim = Color32::from_black_alpha(128);
        match sel {
            Some(s) => {
                for r in outside_rects(screen, s) {
                    painter.rect_filled(r, 0.0, dim);
                }
                painter.rect_stroke(s, 0.0, Stroke::new(2.0, Color32::RED), StrokeKind::Outside);
            }
            None => {
                painter.rect_filled(screen, 0.0, dim);
            }
        }

        let help = "스크롤할 영역을 드래그하세요  (Esc / 오른쪽 클릭: 취소)";
        let galley =
            painter.layout_no_wrap(help.to_owned(), FontId::proportional(16.0), Color32::WHITE);
        let pos = screen.min + vec2(16.0, 16.0);
        painter.rect_filled(
            Rect::from_min_size(pos, galley.size()).expand(8.0),
            4.0,
            Color32::from_black_alpha(200),
        );
        painter.galley(pos, galley, Color32::WHITE);

        if released {
            self.drag_start = None;
            if let Some(s) = sel
                && let Some(selection) = self.to_selection(screen, s)
            {
                return Some(OverlayAction::Selected(selection));
            }
        }
        None
    }

    /// egui 좌표의 사각형을 화면 절대 좌표(물리 픽셀)로 바꾼다. 너무 작으면 None
    fn to_selection(&self, screen: Rect, s: Rect) -> Option<Selection> {
        // 화면(egui 좌표) 1 단위가 스크린샷 몇 픽셀인지
        let sx = self.size.0 as f32 / screen.width();
        let sy = self.size.1 as f32 / screen.height();

        let x0 = ((s.min.x - screen.min.x) * sx).round() as i32;
        let y0 = ((s.min.y - screen.min.y) * sy).round() as i32;
        let x1 = ((s.max.x - screen.min.x) * sx).round() as i32;
        let y1 = ((s.max.y - screen.min.y) * sy).round() as i32;
        let (x0, x1) = (
            x0.clamp(0, self.size.0 as i32),
            x1.clamp(0, self.size.0 as i32),
        );
        let (y0, y1) = (
            y0.clamp(0, self.size.1 as i32),
            y1.clamp(0, self.size.1 as i32),
        );

        let (w, h) = ((x1 - x0) as u32, (y1 - y0) as u32);
        (w >= 20 && h >= 20).then_some(Selection {
            x: self.origin.0 + x0,
            y: self.origin.1 + y0,
            width: w,
            height: h,
        })
    }
}

impl Preview {
    /// 미리보기 표시 (창 너비에 맞춰 축소, 원본보다 크게 확대하지는 않음)
    fn show(&mut self, ui: &mut egui::Ui) {
        let image = &self.image;
        let tiles = self
            .tiles
            .get_or_insert_with(|| make_tiles(ui.ctx(), image));

        let ppp = ui.ctx().pixels_per_point();
        let full_w = image.width() as f32 / ppp; // 원본 1:1 크기 (egui 좌표)

        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            let w = full_w.min(ui.available_width());
            let scale = w / full_w;
            for (texture, h) in tiles.iter() {
                ui.image((texture.id(), vec2(w, *h as f32 / ppp * scale)));
            }
        });
    }

    /// 저장 대화상자를 띄워 PNG로 저장한다. 상태 메시지를 돌려준다
    fn save(&self) -> String {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let Some(path) = rfd::FileDialog::new()
            .add_filter("PNG 이미지", &["png"])
            .set_file_name(format!("scroll_capture_{secs}.png"))
            .save_file()
        else {
            return "저장 취소됨".to_owned();
        };
        match self.image.save(&path) {
            Ok(()) => format!("저장 완료: {}", path.display()),
            Err(e) => format!("저장 실패: {e}"),
        }
    }

    /// 클립보드에 이미지 복사. 상태 메시지를 돌려준다
    fn copy(&self) -> String {
        let data = arboard::ImageData {
            width: self.image.width() as usize,
            height: self.image.height() as usize,
            bytes: Cow::Borrowed(self.image.as_raw()),
        };
        match arboard::Clipboard::new().and_then(|mut c| c.set_image(data)) {
            Ok(()) => "클립보드에 복사했습니다.".to_owned(),
            Err(e) => format!("복사 실패: {e}"),
        }
    }
}

/// 긴 이미지를 GPU 텍스처 최대 크기 이하의 조각들로 나눠 올린다
fn make_tiles(ctx: &egui::Context, image: &RgbaImage) -> Vec<(TextureHandle, u32)> {
    let max_side = ctx.input(|i| i.max_texture_side).min(4096) as u32;
    let (w, h) = (image.width(), image.height());
    let row_bytes = w as usize * 4;
    let raw = image.as_raw();

    (0..h)
        .step_by(max_side as usize)
        .enumerate()
        .map(|(i, y)| {
            let tile_h = max_side.min(h - y);
            let bytes = &raw[y as usize * row_bytes..(y + tile_h) as usize * row_bytes];
            let tile =
                egui::ColorImage::from_rgba_unmultiplied([w as usize, tile_h as usize], bytes);
            let texture = ctx.load_texture(format!("preview-{i}"), tile, TextureOptions::LINEAR);
            (texture, tile_h)
        })
        .collect()
}

/// 화면에서 선택 사각형을 뺀 나머지(위, 아래, 왼쪽, 오른쪽) 사각형들
fn outside_rects(screen: Rect, s: Rect) -> [Rect; 4] {
    [
        Rect::from_min_max(screen.min, pos2(screen.max.x, s.min.y)),
        Rect::from_min_max(pos2(screen.min.x, s.max.y), screen.max),
        Rect::from_min_max(pos2(screen.min.x, s.min.y), pos2(s.min.x, s.max.y)),
        Rect::from_min_max(pos2(s.max.x, s.min.y), pos2(screen.max.x, s.max.y)),
    ]
}

/// 메인 창의 가운데가 있는 모니터
fn monitor_of_window(ctx: &egui::Context) -> Result<Monitor> {
    let (rect, ppp) = ctx.input(|i| {
        (
            i.viewport().outer_rect,
            i.viewport().native_pixels_per_point,
        )
    });
    let (Some(rect), Some(ppp)) = (rect, ppp) else {
        anyhow::bail!("창 위치를 알 수 없음");
    };
    let c = rect.center();
    Ok(Monitor::from_point((c.x * ppp) as i32, (c.y * ppp) as i32)?)
}

/// 주 모니터
fn primary_monitor(_: anyhow::Error) -> Result<Monitor> {
    Monitor::all()?
        .into_iter()
        .find(|m| m.is_primary().unwrap_or(false))
        .context("주 모니터를 찾을 수 없습니다")
}

/// 한글이 보이도록 Windows 기본 한글 글꼴(맑은 고딕)을 대체 글꼴로 추가
fn setup_korean_font(ctx: &egui::Context) {
    let candidates = [
        r"C:\Windows\Fonts\malgun.ttf",
        r"C:\Windows\Fonts\gulim.ttc",
    ];
    let Some(bytes) = candidates.iter().find_map(|p| std::fs::read(p).ok()) else {
        return;
    };
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "korean".to_owned(),
        Arc::new(egui::FontData::from_owned(bytes)),
    );
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts
            .families
            .entry(family)
            .or_default()
            .push("korean".to_owned());
    }
    ctx.set_fonts(fonts);
}
