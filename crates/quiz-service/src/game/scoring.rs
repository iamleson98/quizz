//! Points for an answer.

/// `1000 x (0.5 + 0.5 x remaining/limit)`, rounded, scaled by the credit
/// (1.0 for a right single-choice answer, a share for multiple choice,
/// 0.0 for a wrong or missing one).
pub fn points_for(credit: f64, elapsed_ms: i64, limit_ms: i64) -> i64 {
    if credit <= 0.0 || limit_ms <= 0 {
        return 0;
    }
    let remaining = (limit_ms - elapsed_ms).clamp(0, limit_ms) as f64 / limit_ms as f64;
    (1000.0 * (0.5 + 0.5 * remaining) * credit).round() as i64
}

/// Extra points for answering fully correctly several questions in a row:
/// `min(100 x (streak - 1), 500)`, where `streak` counts the question just
/// answered (nothing for the first, 100 for the second, up to 500).
pub fn streak_bonus(streak: u32) -> i64 {
    (100 * i64::from(streak.saturating_sub(1))).min(500)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn faster_earns_more_between_500_and_1000() {
        assert_eq!(points_for(1.0, 0, 20_000), 1000);
        assert_eq!(points_for(1.0, 10_000, 20_000), 750);
        assert_eq!(points_for(1.0, 20_000, 20_000), 500);
        assert_eq!(points_for(1.0, 25_000, 20_000), 500);
    }

    #[test]
    fn the_streak_bonus_grows_by_100_and_stops_at_500() {
        let bonuses: Vec<i64> = (0..9).map(streak_bonus).collect();
        assert_eq!(bonuses, vec![0, 0, 100, 200, 300, 400, 500, 500, 500]);
        assert_eq!(streak_bonus(1000), 500);
    }

    #[test]
    fn credit_scales_and_wrong_earns_nothing() {
        assert_eq!(points_for(0.5, 0, 20_000), 500);
        assert_eq!(points_for(0.0, 0, 20_000), 0);
    }
}
