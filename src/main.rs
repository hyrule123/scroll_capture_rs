mod analyzer;
mod overlay;

use anyhow::{Context, Result};
use xcap::Monitor;
use xcap::image::RgbaImage;

use enigo::{Axis, Coordinate, Enigo, Mouse, Settings};
use std::{thread, time::Duration};

fn grab(monitor: &Monitor, x: u32, y: u32, w: u32, h: u32) -> Result<RgbaImage> {
    monitor
        .capture_region(x, y, w, h)
        .context("화면 캡처에 실패했습니다")
}

fn main() -> Result<()> {
    // DPI 인식 설정
    let _ = enigo::set_dpi_awareness();

    // 주 모니터 찾기
    let monitor = Monitor::all()?
        .into_iter()
        .find(|m| m.is_primary().unwrap_or(false))
        .context("주 모니터를 찾을 수 없습니다")?;

    let screenshot = monitor.capture_image()?;
    if let Some(selection) = overlay::select_region(&screenshot, (monitor.x()?, monitor.y()?))?{
        std::thread::sleep(std::time::Duration::from_millis(300)); // 오버레이 닫히기 전에 잠시 대기
        run_capture(selection, 2000)?;
    }

    println!("오버레이 닫힘");
    Ok(())
}

fn run_capture(sel: overlay::Selection, max_height: u32) -> Result<()> {
    // 1. 인수 출력
    let (x, y, width, height) = (sel.x, sel.y, sel.width, sel.height);
    println!(
        "캡처 영역: x={}, y={}, width={}, height={}, max_height={}",
        x, y, width, height, max_height
    );

    // 2-1. 모니터 찾기
    let monitor = Monitor::from_point(x, y).context("해당 좌표에 모니터가 없습니다")?;
    let local_x = (x - monitor.x()?) as u32;
    let local_y = (y - monitor.y()?) as u32;

    // 3. Enigo 생성, 마우스를 스크롤 중앙으로 이동
    let mut enigo = Enigo::new(&Settings::default())?;

    let center_x = x + (width as i32) / 2;
    let center_y = y + (height as i32) / 2;

    enigo.move_mouse(center_x, center_y, Coordinate::Abs)?; // 화면 절대 좌표 (i32)

    let mut prev = grab(&monitor, local_x, local_y, width, height)?;
    let mut prev_sig = analyzer::analyze(prev.as_raw(), width as usize, height as usize, 24);
    let mut no_move = 0;
    let mut footer = None;
    let mut out: Vec<u8> = Vec::new();
    let row_bytes = (width * 4) as usize; // RGBA 이미지 한 행의 바이트 수

    loop {
        enigo.scroll(1, Axis::Vertical)?; // 양수 = 아래로  

        thread::sleep(Duration::from_millis(500)); // 스크롤 후 잠시 대기

        // 새 이미지 캡처
        let cur = grab(&monitor, local_x, local_y, width, height)?;

        let cur_sig = analyzer::analyze(cur.as_raw(), width as usize, height as usize, 24);

        if analyzer::identical(&prev_sig, &cur_sig) {
            no_move += 1;
            println!("동일한 이미지가 {}번 연속으로 나타났습니다", no_move);

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
                let raw = prev.as_raw();
                out.extend_from_slice(&raw[..(height as usize - f) * row_bytes]);
                footer = Some(f);
                f // 블록의 마지막 식 → match의 값
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

        // 이미지의 시작 영역 / 끝 영역을 row_bytes에 넣음
        let start = (content_h - dy) * row_bytes;
        let end = content_h * row_bytes;
        let raw = cur.as_raw();

        if out.len() / row_bytes + dy > max_height as usize {
            println!("최대 높이를 초과하여 스크롤을 중단합니다");
            break;
        }

        out.extend_from_slice(&raw[start..end]);

        prev = cur;
        prev_sig = cur_sig;
    }

    // footer 붙여넣기
    match footer {
        Some(f) => {
            // 마지막 화면의 푸터 붙이기 (f가 0이면 빈 범위라 아무것도 안 붙음)
            out.extend_from_slice(&prev.as_raw()[(height as usize - f) * row_bytes..]);
        }
        None => {
            // 한 번도 안 움직임 → 화면 전체
            out.extend_from_slice(prev.as_raw());
        }
    }
    let image = RgbaImage::from_raw(width, (out.len() / row_bytes) as u32, out)
        .context("결과 이미지를 만들 수 없습니다")?;

    image
        .save("out.png")
        .context("out.png 저장에 실패했습니다")?;
    println!(
        "저장 완료: out.png ({} x {})",
        image.width(),
        image.height()
    );

    Ok(())
}
