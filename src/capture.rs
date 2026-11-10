//! 선택 영역을 자동으로 스크롤하며 캡처하고 이어 붙이기 (2단계 캡처 루프)

use anyhow::{Context, Result};
use enigo::{Axis, Coordinate, Enigo, Mouse, Settings};
use std::thread;
use std::time::{Duration, Instant};
use xcap::Monitor;
use xcap::image::RgbaImage;

use crate::analyzer;

/// 선택된 영역 (화면 절대 좌표, 물리 픽셀)
#[derive(Debug, Clone, Copy)]
pub struct Selection {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

/// 캡처 설정
#[derive(Debug, Clone)]
pub struct CaptureOptions {
    /// 휠을 굴린 뒤 화면이 멈출 때까지 기다리는 시간 (ms)
    pub delay_ms: u64,
    /// 결과 이미지의 최대 높이 (px)
    pub max_height: u32,
    /// 한 번에 굴릴 휠 칸 수
    pub wheel_notches: i32,
}

impl Default for CaptureOptions {
    fn default() -> Self {
        Self {
            delay_ms: 500,
            max_height: 20000,
            wheel_notches: 1,
        }
    }
}

/// 캡처 결과
pub struct CaptureOutcome {
    pub image: RgbaImage,
    /// 사용자가 Esc나 마우스 클릭으로 중단했는지
    pub stopped: bool,
}

/// Esc 또는 마우스 왼쪽/오른쪽 버튼이 지금 눌려 있는지.
/// 어느 창에 포커스가 있든 상관없이 키보드·마우스의 실제 상태를 읽는다.
fn stop_requested() -> bool {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, VK_ESCAPE, VK_LBUTTON, VK_RBUTTON,
    };
    [VK_ESCAPE, VK_LBUTTON, VK_RBUTTON].iter().any(|&vk| {
        // SAFETY: GetAsyncKeyState는 키 코드(정수) 하나만 받고 포인터를 다루지 않으므로
        // 어떤 값으로 호출해도 메모리 안전성 문제가 없다.
        let state = unsafe { GetAsyncKeyState(vk as i32) };
        state < 0 // 최상위 비트가 1(음수) = 지금 눌려 있음
    })
}

/// `ms`만큼 기다리되, 그 사이 중단 입력이 들어오면 바로 true를 돌려준다
fn wait_or_stop(ms: u64) -> bool {
    let deadline = Instant::now() + Duration::from_millis(ms);
    loop {
        if stop_requested() {
            return true;
        }
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        // 짧은 클릭도 놓치지 않도록 10ms마다 확인
        thread::sleep((deadline - now).min(Duration::from_millis(10)));
    }
}

fn grab(monitor: &Monitor, x: u32, y: u32, w: u32, h: u32) -> Result<RgbaImage> {
    monitor
        .capture_region(x, y, w, h)
        .context("화면 캡처에 실패했습니다")
}

/// 선택 영역을 스크롤하며 캡처해서 하나의 긴 이미지로 돌려준다.
/// `progress`는 지금까지 이어 붙인 높이(px)를 받는다.
/// Esc나 마우스 클릭이 들어오면 즉시 멈추고, 그때까지 찍은 부분을 돌려준다.
pub fn run_capture(
    sel: Selection,
    opt: &CaptureOptions,
    progress: impl Fn(u32),
) -> Result<CaptureOutcome> {
    let (x, y, width, height) = (sel.x, sel.y, sel.width, sel.height);
    println!("캡처 영역: {sel:?}, 설정: {opt:?}");

    // 모니터 찾기 (캡처는 모니터 기준 좌표를 사용)
    let monitor = Monitor::from_point(x, y).context("해당 좌표에 모니터가 없습니다")?;
    let local_x = (x - monitor.x()?) as u32;
    let local_y = (y - monitor.y()?) as u32;

    // 마우스를 선택 영역 중앙으로 이동 (휠은 커서 아래 창으로 전달됨)
    let mut enigo = Enigo::new(&Settings::default())?;
    let center_x = x + (width as i32) / 2;
    let center_y = y + (height as i32) / 2;
    enigo.move_mouse(center_x, center_y, Coordinate::Abs)?;

    let mut prev = grab(&monitor, local_x, local_y, width, height)?;
    let mut prev_sig = analyzer::analyze(prev.as_raw(), width as usize, height as usize, 24);
    let mut no_move = 0;
    let mut footer = None;
    let mut out: Vec<u8> = Vec::new();
    let row_bytes = (width * 4) as usize; // RGBA 이미지 한 행의 바이트 수
    let mut stopped = false;

    loop {
        if stop_requested() {
            stopped = true;
            break;
        }
        enigo.scroll(opt.wheel_notches, Axis::Vertical)?; // 양수 = 아래로

        // 스크롤 후 잠시 대기 (대기 중에도 중단 입력 확인)
        if wait_or_stop(opt.delay_ms) {
            stopped = true;
            break;
        }

        // 새 이미지 캡처
        let cur = grab(&monitor, local_x, local_y, width, height)?;
        let cur_sig = analyzer::analyze(cur.as_raw(), width as usize, height as usize, 24);

        if analyzer::identical(&prev_sig, &cur_sig) {
            no_move += 1;
            println!("동일한 이미지가 {no_move}번 연속으로 나타났습니다");
            if no_move >= 3 {
                println!("동일한 이미지가 3번 연속으로 나타나 스크롤이 멈춘 것으로 간주합니다");
                break;
            }
            continue;
        }
        no_move = 0; // 이미지가 달라졌으므로 연속 동일 횟수 초기화

        // 푸터 계산 & 첫 화면 붙이기
        let f = match footer {
            Some(f) => f, // 이미 정해짐 → 그 값
            None => {
                // 첫 이동 → 계산하고 첫 화면 붙이기
                let f = analyzer::detect_static_bottom(&prev_sig, &cur_sig);
                out.extend_from_slice(&prev.as_raw()[..(height as usize - f) * row_bytes]);
                footer = Some(f);
                f
            }
        };

        // 푸터 영역을 제외하면 컨텐츠 영역
        let content_h = height as usize - f;

        // 새로 찍힌 영역의 길이(footer 제외)
        let dy = match analyzer::find_scroll_offset(&prev_sig, &cur_sig, content_h) {
            Some(d) if d > 0 => d,
            _ => {
                println!("스크롤되지 않음 또는 매칭 실패");
                break;
            }
        };

        if out.len() / row_bytes + dy > opt.max_height as usize {
            println!("최대 높이를 초과하여 스크롤을 중단합니다");
            break;
        }

        // 새로 드러난 부분(본문의 맨 아래 dy줄) 붙이기
        let start = (content_h - dy) * row_bytes;
        let end = content_h * row_bytes;
        out.extend_from_slice(&cur.as_raw()[start..end]);
        progress((out.len() / row_bytes) as u32);

        prev = cur;
        prev_sig = cur_sig;
    }

    // footer 붙여넣기
    match footer {
        // 마지막 화면의 푸터 붙이기 (f가 0이면 빈 범위라 아무것도 안 붙음)
        Some(f) => out.extend_from_slice(&prev.as_raw()[(height as usize - f) * row_bytes..]),
        // 한 번도 안 움직임 → 화면 전체
        None => out.extend_from_slice(prev.as_raw()),
    }

    if stopped {
        println!("사용자가 중단했습니다");
    }

    let image = RgbaImage::from_raw(width, (out.len() / row_bytes) as u32, out)
        .context("결과 이미지를 만들 수 없습니다")?;
    Ok(CaptureOutcome { image, stopped })
}
