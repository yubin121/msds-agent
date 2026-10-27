//! 모델별 단가로 추정 비용을 계산한다. 단가 표에 없는 모델은 비용을 비워 둔다(None).

/// (모델, 입력, 출력) 100만 토큰당 USD
const PRICES: &[(&str, f64, f64)] = &[
    ("claude-sonnet-5-5", 2.0, 10.0),
    ("claude-haiku-5-5", 0.1, 0.5),
];

pub fn estimate(model: &str, input_tokens: u64, output_tokens: u64) -> Option<f64> {
    let model = model.trim();
    let (_, input_price, output_price) = PRICES.iter().find(|(m, _, _)| *m == model)?;
    Some((input_tokens as f64 * input_price + output_tokens as f64 * output_price) / 1_000_000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_and_unknown_models() {
        assert_eq!(estimate("claude-sonnet-5-5", 1_000_000, 100_000), Some(3.0));
        assert_eq!(estimate(" claude-haiku-5-5 ", 2_000_000, 1_000_000), Some(0.7));
        assert_eq!(estimate("claude-haiku-5-5", 0, 0), Some(0.0));
        assert_eq!(estimate("gpt-x", 10, 10), None);
        assert_eq!(estimate("", 10, 10), None);
    }
}
