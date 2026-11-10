//! 화면을 어둡게 덮고 마우스 드래그로 영역을 선택하는 오버레이

use std::num::NonZeroU32;
use std::rc::Rc;

use anyhow::{Result, anyhow};
use softbuffer::{Context, Surface};
use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, MouseButton};
use winit::event::{KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::keyboard::{Key, NamedKey};
use winit::window::{CursorIcon, Window, WindowId, WindowLevel};
use xcap::image::RgbaImage;

/// 선택된 영역 (화면 절대 좌표, 물리 픽셀)
#[derive(Debug, Clone, Copy)]
pub struct Selection {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// 오버레이 창의 상태
struct App {
    /// 창을 띄울 위치 (모니터 왼쪽 위, 화면 절대 좌표)
    origin: (i32, i32),
    /// 창 크기 = 스크린샷 크기
    size: (u32, u32),
    /// 원본 밝기 픽셀 (0x00RRGGBB)
    bright: Vec<u32>,
    /// 어둡게 만든 픽셀 (0x00RRGGBB)
    dimmed: Vec<u32>,
    /// 창과 그리기 대상. 이벤트 루프가 시작된 뒤(resumed)에 만들어지므로 처음엔 None
    window: Option<Rc<Window>>,
    surface: Option<Surface<Rc<Window>, Rc<Window>>>,

    cursor: (i32, i32), // 마우스 커서 위치 (창 기준)

    drag_start: Option<(i32, i32)>, // 드래그 시작점, 드래그 중이 아니면 None

    result: Option<Selection>, // 선택 결과. 취소하면 None으로 남음
}

impl App {
    fn new(screenshot: &RgbaImage, origin: (i32, i32)) -> Self {
        // screenshot의 RGBA 바이트를 픽셀 하나당 u32(0x00RRGGBB)로 바꿔 bright 만들기
        let bright: Vec<u32> = screenshot
            .as_raw()
            .chunks_exact(4)
            .map(|p| {
                let r = p[0] as u32;
                let g = p[1] as u32;
                let b = p[2] as u32;
                (r << 16) | (g << 8) | b
            })
            .collect();

        //bright의 각 픽셀을 어둡게(예: 밝기 절반) 만들어 dimmed 만들기
        // 한 칸씩 옮기고, 넘어간 최상위 비트는 제거(0b 0111 1111)
        let dimmed: Vec<u32> = bright.iter().map(|&c| (c >> 1) & 0x7F7F7F).collect();

        Self {
            origin,
            size: (screenshot.width(), screenshot.height()),
            bright,
            dimmed,
            window: None,
            surface: None,
            cursor: (0, 0),
            drag_start: None,
            result: None,
        }
    }

    /// 화면 그리기
    fn draw(&mut self) -> Result<()> {
        let (w, h) = self.size;
        let rect = self.rect();

        let Some(surface) = self.surface.as_mut() else {
            return Ok(());
        };
        surface
            .resize(NonZeroU32::new(w).unwrap(), NonZeroU32::new(h).unwrap())
            .map_err(|e| anyhow!("{e}"))?;

        // buf: 창의 픽셀 배열 (&mut [u32] 처럼 쓸 수 있음, 길이 w * h)
        let mut buf = surface.buffer_mut().map_err(|e| anyhow!("{e}"))?;
        buf.copy_from_slice(&self.dimmed);

        if let Some((x, y, rw, rh)) = rect
            && rw > 0
            && rh > 0
        {
            let (x, y, rw, rh) = (x as usize, y as usize, rw as usize, rh as usize);

            let x_end = x + rw;
            let y_end = y + rh;

            // 선택 영역을 밝은 부분으로 칠하기
            for row in y..y_end {
                let offset = row * w as usize;
                buf[offset + x..offset + x_end]
                    .copy_from_slice(&self.bright[offset + x..offset + x_end]);
            }

            // 선택 영역 외곽 테두리를 빨간색으로 표시
            const THICKNESS: usize = 3;
            const RED: u32 = 0xFF0000;

            let w = w as usize;
            let h = h as usize;
            let red_x_start = x.saturating_sub(THICKNESS);
            let red_y_start = y.saturating_sub(THICKNESS);
            let red_x_end = (x_end + THICKNESS).min(w);
            let red_y_end = (y_end + THICKNESS).min(h);

            for row in red_y_start..red_y_end {
                let offset = row * w;

                // 위쪽 테두리 or 아래쪽 테두리
                if row < y || row >= y_end {
                    buf[offset + red_x_start..offset + red_x_end].fill(RED); // 빨간색으로 채우기
                }
                // 왼쪽 테두리 or 오른쪽 테두리
                else {
                    buf[offset + red_x_start..offset + x].fill(RED); // 왼쪽 테두리
                    buf[offset + x_end..offset + red_x_end].fill(RED); // 오른쪽 테두리
                }
            }
        }

        buf.present().map_err(|e| anyhow!("{e}"))?;
        Ok(())
    }

    /// 드래그 시작점과 현재 커서로 만든 사각형 (창 기준 x, y, 너비, 높이)
    fn rect(&self) -> Option<(u32, u32, u32, u32)> {
        let (sx, sy) = self.drag_start?;
        let (cx, cy) = self.cursor;
        let (w, h) = (self.size.0 as i32, self.size.1 as i32);

        let x0 = sx.min(cx).clamp(0, w) as u32;
        let y0 = sy.min(cy).clamp(0, h) as u32;
        let x1 = sx.max(cx).clamp(0, w) as u32;
        let y1 = sy.max(cy).clamp(0, h) as u32;

        Some((x0, y0, x1 - x0, y1 - y0))
    }
}

/// winit이 이벤트가 생길 때마다 호출하는 메서드들
impl ApplicationHandler for App {
    /// 이벤트 루프가 준비되면 호출됨 → 여기서 창을 만든다
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let attrs = Window::default_attributes()
            .with_title("영역 선택")
            .with_decorations(false)
            .with_resizable(false)
            .with_window_level(WindowLevel::AlwaysOnTop)
            .with_position(PhysicalPosition::new(self.origin.0, self.origin.1))
            .with_inner_size(PhysicalSize::new(self.size.0, self.size.1));
        let window = Rc::new(event_loop.create_window(attrs).expect("창 생성 실패"));
        window.set_cursor(CursorIcon::Crosshair);

        let context = Context::new(window.clone()).expect("softbuffer 생성 실패");
        let surface = Surface::new(&context, window.clone()).expect("surface 생성 실패");
        self.surface = Some(surface);
        self.window = Some(window);
    }

    /// 창에 이벤트(마우스, 키보드, 다시 그리기 요청 등)가 오면 호출됨
    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::KeyboardInput {
                event:
                    KeyEvent {
                        logical_key: Key::Named(NamedKey::Escape),
                        ..
                    },
                ..
            } => event_loop.exit(),
            WindowEvent::RedrawRequested => {
                if let Err(e) = self.draw() {
                    eprintln!("그리기 실패: {e}");
                    event_loop.exit();
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                //커서 위치 갱신 후
                self.cursor = (position.x as i32, position.y as i32);

                // 드래그 중이면 창을 다시 그리도록 요청
                if self.drag_start.is_some()
                    && let Some(w) = &self.window
                {
                    w.request_redraw();
                }
            }
            WindowEvent::MouseInput {
                state,
                button: MouseButton::Left,
                ..
            } => match state {
                ElementState::Pressed => {
                    self.drag_start = Some(self.cursor);
                }
                ElementState::Released => {
                    if let Some((x, y, w, h)) = self.rect()
                        && w >= 20
                        && h >= 20
                    {
                        // 화면의 절대 좌표로 변환
                        self.result = Some(Selection {
                            x: self.origin.0 + x as i32,
                            y: self.origin.1 + y as i32,
                            width: w,
                            height: h,
                        });
                        event_loop.exit();
                    }
                    self.drag_start = None;
                }
            },
            WindowEvent::MouseInput {
                button: MouseButton::Right,
                ..
            } => event_loop.exit(),
            _ => {}
        }
    }
}

/// 스크린샷을 화면에 띄운다 (3-1: Esc로 닫힘)
pub fn select_region(screenshot: &RgbaImage, origin: (i32, i32)) -> Result<Option<Selection>> {
    let event_loop = EventLoop::new()?;
    let mut app = App::new(screenshot, origin);
    event_loop.run_app(&mut app)?;
    Ok(app.result)
}
