//! 프레임 분석: 행 해시, 스크롤 이동량, 하단 고정 영역 감지
const FNV_OFFSET: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001b3;

/// 프레임의 각 행(row)을 해시로 요약한 정보
#[derive(Debug, Clone, PartialEq)]
pub struct RowSignature {
    /// 행별 해시값
    pub hash: Vec<u64>,
    /// 행 전체가 한 가지 색인지 (매칭에 쓸 정보가 없는 행)
    pub blank: Vec<bool>,
}

impl RowSignature {
    /// 행 개수
    pub fn height(&self) -> usize {
        self.hash.len()
    }
}

/// 픽셀 4바이트(RGBA)에서 RGB만 하나의 값으로 합친다
fn rgb(px: &[u8]) -> u64 {
    (px[0] as u64) << 16 | (px[1] as u64) << 8 | px[2] as u64
}

/// RGBA 픽셀 버퍼(픽셀당 4바이트: R, G, B, A)에서 행별 해시를 계산한다.
/// - 너비가 200px보다 크면 왼쪽 4px와 오른쪽 `ignore_right`px는 제외 (테두리, 스크롤바)
/// - 알파(A) 값은 무시
/// - 계산 범위의 픽셀이 모두 같은 색이면 blank
pub fn analyze(rgba: &[u8], width: usize, height: usize, ignore_right: usize) -> RowSignature {
    let (x_start, x_end) = if width > 200 {
        (4, width - ignore_right)
    } else {
        (0, width)
    };
    let mut hash = Vec::with_capacity(height);
    let mut blank = Vec::with_capacity(height);

    for row in rgba.chunks_exact(width * 4) {
        let part = &row[x_start * 4..x_end * 4];
        let mut row_hash = FNV_OFFSET; // 0이 아니라 정해진 시작값
        let mut is_blank = true;
        let first = rgb(&part[0..4]);

        for px in part.chunks_exact(4) {
            let color = rgb(px);
            row_hash = (row_hash ^ color).wrapping_mul(FNV_PRIME);
            if color != first {
                is_blank = false;
            }
        }
        hash.push(row_hash);
        blank.push(is_blank);
    }
    RowSignature { hash, blank }
}

/// 두 프레임의 모든 행 해시가 같은지
pub fn identical(a: &RowSignature, b: &RowSignature) -> bool {
    a.hash == b.hash
}

/// 화면 하단에 고정된(스크롤해도 안 변하는) 영역의 높이를 계산(footer 등).
/// - 맨 아래 행부터 위로 올라가며 prev와 cur의 해시가 같은 행을 센다 (최대 height / 3)
/// - 그 영역의 맨 위쪽에 있는 blank 행은 제외한다 (우연히 같은 빈 줄)
/// - 남은 영역에 blank가 아닌 행이 하나도 없으면 0
pub fn detect_static_bottom(prev: &RowSignature, cur: &RowSignature) -> usize {
    let h = cur.height(); // prev.height ==  cur.height (항상: 캡처 영역은 항상 동일)
    let mut n = 0;

    // 1단계: 다음 줄의 해시가 일치하는지 확인
    while n < h / 3 && prev.hash[h - 1 - n] == cur.hash[h - 1 - n] {
        n += 1;
    }

    // 2단계: 맨 위줄이 blank면 합산에서 제외
    while n > 0 && cur.blank[h - n] {
        n -= 1;
    }

    // return
    n
}

/// prev → cur 로 내용이 위로 몇 px 이동했는지(dy) 찾는다. 위쪽 content_height 행만 사용.
/// - 각 dy에 대해 cur[i] == prev[i + dy] 인 행 수(match)를 센다
///   (cur[i], prev[i + dy] 둘 다 blank인 행은 건너뛰고, 나머지 행 수가 total)
/// - 겹치는 행 수(content_height - dy)가 max(8, content_height / 12) 이상인 dy만 검사
/// - total < 4 이거나 match / total < 0.6 인 dy는 후보에서 제외
/// - 후보 중 match가 가장 큰 dy (같으면 비율이 높은 쪽)
/// - 반환: Some(0) = 이동 없음, None = 매칭 실패
pub fn find_scroll_offset(
    prev: &RowSignature,
    cur: &RowSignature,
    content_height: usize,
) -> Option<usize> {
    let min_overlap = (content_height / 12).max(8);
    let max_dy = content_height.saturating_sub(min_overlap);

    let mut best_dy: Option<usize> = None;
    let mut best_match = 0;
    let mut best_ratio = 0.0;

    for dy in 0..=max_dy {
        let mut match_cnt: i32 = 0;
        let mut total = 0;

        for i in 0..content_height - dy {
            if prev.blank[i + dy] && cur.blank[i] {
                continue;
            }

            total += 1;
            if prev.hash[i + dy] == cur.hash[i] {
                match_cnt += 1;
            }
        }

        // total < 4 이면 continue
        if total < 4 {
            continue;
        }

        // 비율 계산 (as f64), 0.6 미만이면 continue
        let ratio = match_cnt as f64 / total as f64;
        if ratio < 0.6 {
            continue;
        }

        // match_cnt이 best_match보다 크거나, 같은데 비율이 더 높으면 → best_* 갱신
        if match_cnt > best_match || (match_cnt == best_match && ratio > best_ratio) {
            best_dy = Some(dy);
            best_match = match_cnt;
            best_ratio = ratio;
        }
    }

    best_dy
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 테스트용 의사 난수 생성기 (외부 크레이트 없이)
    fn lcg(seed: &mut u64) -> u64 {
        *seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        *seed >> 1
    }

    const H: usize = 600; // 화면 높이
    const HEADER: usize = 40; // 상단 고정 헤더
    const FOOTER: usize = 30; // 하단 고정 푸터

    /// 가상 문서: 3000행, 약 30%는 빈 줄(해시 0)
    fn make_doc() -> Vec<u64> {
        let mut seed = 1;
        (0..3000)
            .map(|_| {
                let r = lcg(&mut seed);
                if r % 10 < 3 { 0 } else { r | 1 }
            })
            .collect()
    }

    /// 문서의 `top` 행부터 보여주는 가상 화면 (헤더·푸터는 고정)
    fn view(doc: &[u64], top: usize) -> RowSignature {
        let hash: Vec<u64> = (0..H)
            .map(|i| {
                if i < HEADER {
                    111 + i as u64
                } else if i >= H - FOOTER {
                    999 + i as u64
                } else {
                    doc[top + i]
                }
            })
            .collect();
        let blank = hash.iter().map(|&v| v == 0).collect();
        RowSignature { hash, blank }
    }

    #[test]
    fn height_is_row_count() {
        let doc = make_doc();
        assert_eq!(view(&doc, 0).height(), H);
    }

    #[test]
    fn analyze_detects_blank_rows_and_ignores_right_margin() {
        let (w, h) = (300, 3);
        let mut rgba = vec![255u8; w * h * 4]; // 전부 흰색
        // 1행 가운데 픽셀을 검은색으로
        let i = (w + 150) * 4;
        rgba[i..i + 3].copy_from_slice(&[0, 0, 0]);
        // 2행 오른쪽 끝 픽셀(스크롤바 영역)을 검은색으로
        let i = (2 * w + 299) * 4;
        rgba[i..i + 3].copy_from_slice(&[0, 0, 0]);

        let sig = analyze(&rgba, w, h, 24);
        assert_eq!(sig.height(), 3);
        assert_eq!(sig.blank, vec![true, false, true]);
        assert_eq!(sig.hash[0], sig.hash[2], "오른쪽 여백은 무시해야 함");
        assert_ne!(sig.hash[0], sig.hash[1]);
    }

    #[test]
    fn analyze_ignores_alpha() {
        let (w, h) = (300, 1);
        let a = vec![200u8; w * h * 4];
        let mut b = a.clone();
        for px in b.chunks_exact_mut(4) {
            px[3] = 0; // 알파만 변경
        }
        assert_eq!(analyze(&a, w, h, 24), analyze(&b, w, h, 24));
    }

    #[test]
    fn identical_frames() {
        let doc = make_doc();
        let a = view(&doc, 100);
        assert!(identical(&a, &a.clone()));
        assert!(!identical(&a, &view(&doc, 101)));
    }

    #[test]
    fn detects_static_footer() {
        let doc = make_doc();
        let prev = view(&doc, 0);
        let cur = view(&doc, 100);
        assert_eq!(detect_static_bottom(&prev, &cur), FOOTER);
    }

    #[test]
    fn detects_scroll_offset() {
        let doc = make_doc();
        for (top, dy) in [(0, 100), (100, 237), (500, 1), (900, 400)] {
            let prev = view(&doc, top);
            let cur = view(&doc, top + dy);
            assert_eq!(
                find_scroll_offset(&prev, &cur, H - FOOTER),
                Some(dy),
                "{dy}px 스크롤"
            );
        }
    }

    #[test]
    fn no_movement_is_zero() {
        let doc = make_doc();
        let a = view(&doc, 1200);
        assert_eq!(find_scroll_offset(&a, &a, H), Some(0));
    }

    #[test]
    fn unrelated_frames_fail() {
        let doc = make_doc();
        let prev = view(&doc, 0);
        let cur = view(&doc, 2000);
        assert_eq!(find_scroll_offset(&prev, &cur, H - FOOTER), None);
    }
}
