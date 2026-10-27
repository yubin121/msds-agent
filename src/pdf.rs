//! PDF 텍스트 추출.
//!
//! 엑셀에서 내보낸 PDF처럼 그리는 순서와 읽는 순서가 다른 문서가 있어서, 글자 좌표를 모아
//! 줄 단위로 다시 정렬한다(기존 서비스 hybrid_extract.py와 같은 방식).
//! 좌표 방식이 실패하거나 결과가 비면 pdf_extract의 기본 추출로 대신한다.

use pdf_extract::{Document, MediaBox, OutputDev, OutputError, Transform};

/// 공백을 뺀 글자 수가 이보다 적으면 텍스트가 없는 PDF로 본다
/// 글자 수 기준과 메시지는 업로드 검증과 같은 값이라 msds-validate에 있다
pub use msds_validate::{non_space_chars, MIN_TEXT_CHARS, NO_TEXT_ERROR};

pub struct Extracted {
    pub pages: Vec<String>,
    /// 좌표 방식이 실패해 기본 추출을 썼는지
    pub fallback: bool,
}

/// PDF 바이트에서 페이지별 텍스트를 뽑는다.
/// pdf_extract는 동기 함수이고 일부 PDF에서 panic을 내므로
/// spawn_blocking 안에서 실행하고 panic은 Err로 바꾼다.
pub async fn extract_pages(bytes: Vec<u8>) -> Result<Extracted, String> {
    let joined = tokio::task::spawn_blocking(move || {
        let layout = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| extract_by_layout(&bytes)));
        let reason = match layout {
            Ok(Ok(pages)) if non_space_chars(&pages) > 0 => return Ok(Extracted { pages, fallback: false }),
            Ok(Ok(_)) => "결과가 비어 있음".to_string(),
            Ok(Err(e)) => format!("오류: {e}"),
            Err(panic) => format!("panic: {}", panic_message(&panic)),
        };
        tracing::warn!("좌표 기반 추출 실패({reason}), 기본 추출로 대신합니다");

        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            pdf_extract::extract_text_from_mem_by_pages(&bytes)
        })) {
            Ok(Ok(pages)) => Ok(Extracted { pages, fallback: true }),
            Ok(Err(e)) => Err(format!("PDF 텍스트 추출 실패: {e}")),
            Err(panic) => Err(format!("PDF 텍스트 추출 중 panic: {}", panic_message(&panic))),
        }
    })
    .await;

    match joined {
        Ok(result) => result,
        Err(e) => Err(format!("추출 작업 실행 실패: {e}")),
    }
}

fn panic_message(panic: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = panic.downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = panic.downcast_ref::<String>() {
        s.clone()
    } else {
        "알 수 없는 panic".to_string()
    }
}

// ── 좌표 기반 추출 ─────────────────────────────────────────────────────────

fn extract_by_layout(bytes: &[u8]) -> Result<Vec<String>, OutputError> {
    let mut doc = Document::load_mem(bytes)?;
    if doc.is_encrypted() {
        // pdf_extract의 maybe_decrypt와 같이 빈 암호로만 시도한다
        doc.decrypt("").map_err(OutputError::PdfError)?;
    }
    let mut out = LayoutCollector::default();
    for page_num in doc.get_pages().keys().copied() {
        pdf_extract::output_doc_page(&doc, &mut out, page_num)?;
    }
    Ok(out.pages)
}

/// 화면 좌표의 글자 하나 (PDF 좌표: 아래가 y=0)
#[derive(Debug, Clone, PartialEq)]
pub struct Glyph {
    pub x: f64,
    /// 기준선 y
    pub y: f64,
    /// 글자 폭(화면 단위)
    pub width: f64,
    /// 화면 단위 글자 크기
    pub size: f64,
    pub text: String,
}

#[derive(Default)]
struct LayoutCollector {
    pages: Vec<String>,
    glyphs: Vec<Glyph>,
    /// 회전된 글자: 그리는 순서대로, 단어 사이는 공백
    rotated: String,
    new_word: bool,
}

impl OutputDev for LayoutCollector {
    fn begin_page(&mut self, _page_num: u32, _media_box: &MediaBox, _art_box: Option<(f64, f64, f64, f64)>) -> Result<(), OutputError> {
        self.glyphs.clear();
        self.rotated.clear();
        Ok(())
    }

    fn end_page(&mut self) -> Result<(), OutputError> {
        let mut text = layout_page(std::mem::take(&mut self.glyphs)).join("\n");
        let rotated = self.rotated.trim();
        if !rotated.is_empty() {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(rotated);
        }
        self.pages.push(text);
        Ok(())
    }

    fn output_character(&mut self, trm: &Transform, width: f64, _spacing: f64, font_size: f64, char: &str) -> Result<(), OutputError> {
        // trm = 글자 공간 → 페이지 공간. 크기는 면적이 같은 정사각형 한 변 (PlainTextOutput과 같은 기준)
        let scale = (trm.m11 * trm.m22 - trm.m12 * trm.m21).abs().sqrt();
        let size = font_size * scale;
        let rotated = scale > 0.0
            && (trm.m12.abs() > 1e-3 * scale || trm.m21.abs() > 1e-3 * scale || trm.m11 < 0.0);

        if rotated {
            if self.new_word && !self.rotated.is_empty() && !self.rotated.ends_with(' ') {
                self.rotated.push(' ');
            }
            self.rotated.push_str(char);
        } else {
            self.glyphs.push(Glyph { x: trm.m31, y: trm.m32, width: width * size, size, text: char.to_string() });
        }
        self.new_word = false;
        Ok(())
    }

    fn begin_word(&mut self) -> Result<(), OutputError> {
        self.new_word = true;
        Ok(())
    }
    fn end_word(&mut self) -> Result<(), OutputError> {
        Ok(())
    }
    fn end_line(&mut self) -> Result<(), OutputError> {
        Ok(())
    }
}

/// 글자들을 줄로 묶어 정렬한다 (순수 함수). `glyphs`는 그린 순서여야 한다.
/// - 기준선 y 차이가 글자 크기의 절반보다 작으면 같은 줄. 줄은 y가 큰 것(위)부터.
/// - 줄 안은 덩어리(그린 순서에서 이어지고 x가 뒤로 가지 않는 글자들)를 시작 x 순서로 놓는다.
///   일부 폰트는 글자 폭 정보가 틀려 글자별 x가 밀리므로, 글자 단위로 x 정렬하면 옆 칸 글자와 섞인다.
/// - 앞 글자 끝과 다음 글자 시작 간격이 크기의 0.25배를 넘으면 공백 1칸, 2배를 넘으면 2칸(엑셀 칸 구분).
///   문서에 그려진 공백 글자도 1칸으로 친다. 덩어리가 바뀌는데 앞 덩어리와 크게 겹치면(폭 정보 오류) 1칸.
pub fn layout_page(glyphs: Vec<Glyph>) -> Vec<String> {
    let glyphs: Vec<Glyph> =
        glyphs.into_iter().filter(|g| g.size.is_finite() && g.x.is_finite() && g.y.is_finite()).collect();
    let mut order: Vec<usize> = (0..glyphs.len()).collect();
    order.sort_by(|&a, &b| glyphs[b].y.total_cmp(&glyphs[a].y).then(glyphs[a].x.total_cmp(&glyphs[b].x)));

    let mut lines: Vec<(f64, f64, Vec<usize>)> = Vec::new(); // (기준 y, 줄 글자 크기, 글자 인덱스)
    for i in order {
        let g = &glyphs[i];
        match lines.last_mut() {
            Some((y, size, line)) if (*y - g.y).abs() < 0.5 * size.max(g.size) => {
                *size = size.max(g.size);
                line.push(i);
            }
            _ => lines.push((g.y, g.size, vec![i])),
        }
    }

    lines
        .into_iter()
        .map(|(_, _, mut line)| {
            // 그린 순서로 되돌려 덩어리(segment)로 묶는다: 그린 순서에서 바로 이어지고
            // x가 뒤로 가지 않으며 앞 글자 끝에서 1em 넘게 뛰지 않으면 같은 덩어리(엑셀 칸 하나).
            // 덩어리 안은 그린 순서, 덩어리끼리는 시작 x 순서.
            line.sort_unstable();
            let mut runs: Vec<Vec<&Glyph>> = Vec::new();
            let mut prev: Option<usize> = None;
            for i in line {
                let g = &glyphs[i];
                let continues = prev.is_some_and(|p| {
                    let pg = &glyphs[p];
                    p + 1 == i && g.x >= pg.x - 0.1 * g.size && g.x <= pg.x + pg.width + g.size
                });
                match runs.last_mut() {
                    Some(r) if continues => r.push(g),
                    _ => runs.push(vec![g]),
                }
                prev = Some(i);
            }
            runs.sort_by(|a, b| run_start(a).total_cmp(&run_start(b)));
            // 가짜 굵게: 같은 문자열을 거의 같은 위치에 다시 그린 run은 뺀다
            let mut kept: Vec<Vec<&Glyph>> = Vec::with_capacity(runs.len());
            for r in runs {
                let dup = kept.iter().rev().take(3).any(|k| {
                    k.len() == r.len()
                        && (run_start(k) - run_start(&r)).abs() < 0.2 * r[0].size
                        && k.iter().zip(&r).all(|(a, b)| a.text == b.text)
                });
                if !dup {
                    kept.push(r);
                }
            }
            join_line(&kept)
        })
        .filter(|l| !l.trim().is_empty())
        .collect()
}

fn run_start(run: &[&Glyph]) -> f64 {
    run.iter().map(|g| g.x).fold(f64::INFINITY, f64::min)
}

fn join_line(runs: &[Vec<&Glyph>]) -> String {
    let mut out = String::new();
    // 마지막으로 쓴 (공백이 아닌) 글자
    let mut last: Option<&Glyph> = None;
    let mut space_glyph = false;
    for run in runs {
        let mut run_first = true;
        for g in run {
            if g.text.trim().is_empty() {
                space_glyph = true;
                continue;
            }
            if let Some(prev) = last {
                let size = prev.size.max(g.size);
                let gap = g.x - (prev.x + prev.width);
                if gap > 2.0 * size {
                    out.push_str("  ");
                } else if gap > 0.25 * size || space_glyph || (run_first && gap < -0.25 * size) {
                    out.push(' ');
                }
            }
            out.push_str(&g.text);
            last = Some(g);
            space_glyph = false;
            run_first = false;
        }
    }
    out
}

// ── 언어 판별 ─────────────────────────────────────────────────────────────

/// 한글 글자가 전체 글자(공백·숫자·기호 제외)의 20% 이상이면 "ko", 아니면 "en".
pub fn detect_language(text: &str) -> &'static str {
    let mut letters = 0usize;
    let mut hangul = 0usize;
    for c in text.chars().filter(|c| c.is_alphabetic()) {
        letters += 1;
        if is_hangul(c) {
            hangul += 1;
        }
    }
    if letters > 0 && hangul * 5 >= letters {
        "ko"
    } else {
        "en"
    }
}

fn is_hangul(c: char) -> bool {
    matches!(c,
        '\u{AC00}'..='\u{D7A3}'   // 완성형 음절
        | '\u{1100}'..='\u{11FF}' // 자모
        | '\u{3130}'..='\u{318F}' // 호환 자모
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 글자 크기 10, 폭 5짜리 글자들을 x부터 차례로 놓는다
    fn word(s: &str, x: f64, y: f64) -> Vec<Glyph> {
        word_step(s, x, y, 5.0)
    }

    fn word_step(s: &str, x: f64, y: f64, step: f64) -> Vec<Glyph> {
        s.chars()
            .enumerate()
            .map(|(i, c)| Glyph { x: x + i as f64 * step, y, width: step, size: 10.0, text: c.to_string() })
            .collect()
    }

    /// 칸(단어) 단위로 그리는 순서를 섞는다. 칸 안의 글자는 실제 PDF처럼 순서대로 그린다
    fn shuffle(mut words: Vec<Vec<Glyph>>) -> Vec<Glyph> {
        // 결정적인 섞기: 뒤집고 홀짝 교차
        words.reverse();
        let (even, odd): (Vec<_>, Vec<_>) = words.into_iter().enumerate().partition(|(i, _)| i % 2 == 0);
        odd.into_iter().chain(even).flat_map(|(_, w)| w).collect()
    }

    #[test]
    fn detects_korean_and_english() {
        assert_eq!(detect_language("물질안전보건자료 MSDS"), "ko");
        assert_eq!(detect_language("Safety Data Sheet 1. Identification"), "en");
        assert_eq!(detect_language(""), "en");
    }

    #[tokio::test]
    async fn invalid_pdf_returns_error() {
        assert!(extract_pages(b"not a pdf".to_vec()).await.is_err());
    }

    #[test]
    fn shuffled_glyphs_are_sorted_into_lines() {
        // 엑셀 표처럼 칸을 뒤섞인 순서로 그림: 번호 칸과 제목 칸, 줄 순서 모두 섞임
        let words = vec![
            word("1.", 50.0, 700.0),
            word("PRODUCT", 70.0, 700.0),
            word("2.", 50.0, 680.0),
            word("HAZARDS", 70.0, 680.0),
            word("3.", 50.0, 660.0),
            word("COMP", 70.0, 660.0),
        ];
        assert_eq!(layout_page(shuffle(words)), vec!["1. PRODUCT", "2. HAZARDS", "3. COMP"]);
    }

    #[test]
    fn baseline_jitter_stays_on_one_line() {
        // y 차이 3 < 크기 10의 절반 → 같은 줄, x 순서로
        let mut g = word("AB", 0.0, 100.0);
        g.extend(word("CD", 10.0, 103.0));
        assert_eq!(layout_page(g), vec!["ABCD"]);
        // y 차이 6 ≥ 5 → 다른 줄
        let mut g = word("AB", 0.0, 100.0);
        g.extend(word("CD", 10.0, 94.0));
        assert_eq!(layout_page(g), vec!["AB", "CD"]);
    }

    #[test]
    fn gaps_become_spaces() {
        // "AB" 끝 x=10. 간격 2(0.2배) → 붙임, 5(0.5배) → 1칸, 25(2.5배) → 2칸
        let mut g = word("AB", 0.0, 0.0);
        g.extend(word("CD", 12.0, 0.0));
        assert_eq!(layout_page(g), vec!["ABCD"]);

        let mut g = word("AB", 0.0, 0.0);
        g.extend(word("CD", 15.0, 0.0));
        assert_eq!(layout_page(g), vec!["AB CD"]);

        let mut g = word("AB", 0.0, 0.0);
        g.extend(word("CD", 35.0, 0.0));
        assert_eq!(layout_page(g), vec!["AB  CD"]);
    }

    #[test]
    fn drawn_space_glyph_counts_as_one_space() {
        // 공백 글자가 그려져 있고 간격은 작음 → 1칸, 공백이 겹쳐도 1칸
        let g = word("AB  CD", 0.0, 0.0);
        assert_eq!(layout_page(g), vec!["AB CD"]);
    }

    #[test]
    fn korean_line() {
        let words = vec![word("3.", 85.0, 500.0), word("구성성분의", 100.0, 500.0), word("명칭", 128.0, 500.0)];
        assert_eq!(layout_page(shuffle(words)), vec!["3. 구성성분의 명칭"]);
    }

    #[test]
    fn two_cells_on_same_y() {
        // 엑셀 표: 같은 y의 두 칸이 거꾸로 그려짐 → x 순서 + 2칸 공백
        let mut g = word("구분2", 200.0, 300.0);
        g.extend(word("피부자극성", 50.0, 300.0));
        g.extend(word("눈자극성", 50.0, 285.0));
        g.extend(word("구분2A", 200.0, 285.0));
        assert_eq!(layout_page(g), vec!["피부자극성  구분2", "눈자극성  구분2A"]);
    }

    #[test]
    fn wrong_widths_do_not_interleave_cells() {
        // 폰트 폭 정보가 실제보다 커서 첫 칸 글자 x가 옆 칸(x=60)까지 밀린 경우 (ROYCO 43)
        let mut g = word_step("aquatic", 0.0, 0.0, 12.0); // 끝 x=84로 계산됨
        g.extend(word(": Cat 3", 60.0, 0.0));
        assert_eq!(layout_page(g), vec!["aquatic : Cat 3"]);
    }

    #[test]
    fn far_cells_drawn_back_to_back_are_split() {
        // 칸 A(x=50) → 칸 C(x=300)를 이어서 그리고 칸 B(x=150)를 나중에 그림
        let mut g = word("AAA", 50.0, 0.0);
        g.extend(word("CCC", 300.0, 0.0));
        g.extend(word("BBB", 150.0, 0.0));
        assert_eq!(layout_page(g), vec!["AAA  BBB  CCC"]);
    }

    #[test]
    fn label_drawn_before_value_with_wrong_widths() {
        // ROYCO 43: 글자마다 따로 그리고 폭이 1em로 잘못 잡혀 라벨 끝이 값(x=230)보다 오른쪽까지 밀림
        let mut g = word_step("Short-term (acute) aquatic", 92.0, 204.0, 10.0);
        g.extend(word_step("hazard", 92.0, 192.0, 10.0));
        g.extend(word("  : Category 3", 230.0, 204.0));
        let laid = layout_page(g);
        assert_eq!(laid, vec!["Short-term (acute) aquatic : Category 3", "hazard"]);
    }

    #[test]
    fn glyph_by_glyph_cells_stay_intact() {
        // 글자마다 따로 그려도 그린 순서가 이어지면 한 덩어리로 묶인다
        let mut g = Vec::new();
        for (i, c) in "aquatic".chars().enumerate() {
            g.extend(word_step(&c.to_string(), i as f64 * 10.0, 0.0, 10.0));
        }
        for (i, c) in ": Cat".chars().enumerate() {
            g.extend(word_step(&c.to_string(), 32.0 + i as f64 * 10.0, 0.0, 10.0));
        }
        assert_eq!(layout_page(g), vec!["aquatic : Cat"]);
    }

    #[test]
    fn double_printed_run_is_dropped() {
        // 가짜 굵게: 같은 문자열을 0.5만큼 밀어 다시 그림
        let mut g = word("AB", 0.0, 0.0);
        g.extend(word("AB", 0.5, 0.0));
        assert_eq!(layout_page(g), vec!["AB"]);
    }

    #[test]
    fn empty_page() {
        assert!(layout_page(vec![]).is_empty());
        assert!(layout_page(word("   ", 0.0, 0.0)).is_empty());
    }
}
