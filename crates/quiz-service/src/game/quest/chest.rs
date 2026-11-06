//! The treasure chests: what can be inside, the odds, and the random source.
//!
//! After every correct answer the server draws three outcomes and keeps them
//! hidden; the player picks one. Nothing the client sends can change the draw.
//!
//! Odds at streak 0, per thousand (they sum to 1000):
//!
//! | Outcome | per thousand |
//! |---|---|
//! | +20 coins | 330 |
//! | +40 coins | 165 |
//! | +80 coins | 55 |
//! | Jackpot (+200) | 40 |
//! | Booster | 120 |
//! | Shield | 100 |
//! | Steal | 150 |
//! | Slip (-20) | 40 |
//!
//! "Coins" together are 55%. The rare outcomes (+80 coins and the Jackpot, 9.5%
//! together) are exactly twice as likely from a streak of 3 and three times as
//! likely from a streak of 6; everything else shares what is left in its
//! usual proportions, so the odds always sum to 100%.

/// Coins a jackpot is worth.
pub const JACKPOT: i64 = 200;
/// Answers a booster doubles.
pub const BOOSTER_ANSWERS: u8 = 3;
/// Coins a slip costs.
pub const SLIP: i64 = 20;
/// A steal takes this share (percent) of the victim's coins...
pub const STEAL_PERCENT: i64 = 10;
/// ...but at most this many.
pub const STEAL_CAP: i64 = 100;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Coins(i64),
    Jackpot,
    Booster,
    Shield,
    Steal,
    Slip,
}

impl Outcome {
    /// `(kind, amount)` as sent on the wire: kind 0 coins, 1 jackpot, 2 booster
    /// (amount: answers), 3 shield, 4 steal (amount: what was taken, 0 when
    /// shown unpicked), 5 slip.
    pub fn wire(self) -> (u8, i64) {
        match self {
            Outcome::Coins(n) => (0, n),
            Outcome::Jackpot => (1, JACKPOT),
            Outcome::Booster => (2, i64::from(BOOSTER_ANSWERS)),
            Outcome::Shield => (3, 1),
            Outcome::Steal => (4, 0),
            Outcome::Slip => (5, SLIP),
        }
    }
}

/// The rare outcomes' share of the table, per thousand (+80 coins, Jackpot).
const RARE_PER_THOUSAND: u64 = 55 + 40;

/// `(outcome, weight per thousand, is rare)`
const TABLE: [(Outcome, u32, bool); 8] = [
    (Outcome::Coins(20), 330, false),
    (Outcome::Coins(40), 165, false),
    (Outcome::Coins(80), 55, true),
    (Outcome::Jackpot, 40, true),
    (Outcome::Booster, 120, false),
    (Outcome::Shield, 100, false),
    (Outcome::Steal, 150, false),
    (Outcome::Slip, 40, false),
];

/// How much likelier the rare outcomes are at this streak.
pub fn rare_multiplier(streak: u32) -> u32 {
    match streak {
        0..=2 => 1,
        3..=5 => 2,
        _ => 3,
    }
}

/// The table as integer weights: the rare ones times the multiplier, the
/// others scaled so that they share what is left. The weights sum to
/// `1000 x (1000 - RARE)`, and a rare outcome's probability is exactly
/// `multiplier` times its probability at streak 0.
fn weights(streak: u32) -> Vec<(Outcome, u64)> {
    let m = u64::from(rare_multiplier(streak));
    TABLE
        .iter()
        .map(|(o, w, rare)| {
            let w = u64::from(*w);
            (
                *o,
                if *rare {
                    w * m * (1000 - RARE_PER_THOUSAND)
                } else {
                    w * (1000 - m * RARE_PER_THOUSAND)
                },
            )
        })
        .collect()
}

fn is_coins(o: Outcome) -> bool {
    matches!(o, Outcome::Coins(_))
}

/// The exact odds at a streak, as probabilities. Without steals (friendly
/// mode, or nobody to steal from) a steal that comes up is replaced by
/// coins, drawn like any coin outcome, so its share goes to them.
pub fn odds(streak: u32, allow_steal: bool) -> Vec<(Outcome, f64)> {
    let w = weights(streak);
    let total = w.iter().map(|(_, w)| *w as f64).sum::<f64>();
    let coin_total = w
        .iter()
        .filter(|(o, _)| is_coins(*o))
        .map(|(_, w)| *w as f64)
        .sum::<f64>();
    let steal = w
        .iter()
        .filter(|(o, _)| *o == Outcome::Steal)
        .map(|(_, w)| *w as f64)
        .sum::<f64>();
    w.into_iter()
        .filter(|(o, _)| allow_steal || *o != Outcome::Steal)
        .map(|(o, w)| {
            let mut p = w as f64 / total;
            if !allow_steal && is_coins(o) {
                p += steal / total * w as f64 / coin_total;
            }
            (o, p)
        })
        .collect()
}

/// A source of random numbers. The service uses a CSPRNG; tests seed one.
pub trait Draw: Send + Sync {
    fn next_u64(&mut self) -> u64;

    /// Uniform in `0..n` (`n > 0`), without modulo bias.
    fn below(&mut self, n: u64) -> u64 {
        let zone = u64::MAX - (u64::MAX % n);
        loop {
            let x = self.next_u64();
            if x < zone {
                return x % n;
            }
        }
    }

    /// Shuffle in place (Fisher-Yates).
    fn shuffle<T>(&mut self, items: &mut [T])
    where
        Self: Sized,
    {
        for i in (1..items.len()).rev() {
            let j = self.below(i as u64 + 1) as usize;
            items.swap(i, j);
        }
    }
}

/// The operating system's secure random source (through the thread-local
/// ChaCha generator the `rand` crate seeds from it).
pub struct SystemDraw;

impl Draw for SystemDraw {
    fn next_u64(&mut self) -> u64 {
        rand::random()
    }
}

/// A seeded generator (splitmix64) for tests and reproducible runs.
pub struct SeededDraw(pub u64);

impl Draw for SeededDraw {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

/// Draw one outcome from a weighted list.
fn pick(rng: &mut dyn Draw, w: &[(Outcome, u64)]) -> Outcome {
    let total: u64 = w.iter().map(|(_, w)| *w).sum();
    let mut r = rng.below(total);
    for (o, weight) in w {
        if r < *weight {
            return *o;
        }
        r -= *weight;
    }
    unreachable!("r is below the total weight")
}

/// Draw one outcome. A steal that comes up while steals are not allowed is
/// replaced by coins.
pub fn draw_one(rng: &mut dyn Draw, streak: u32, allow_steal: bool) -> Outcome {
    let w = weights(streak);
    match pick(rng, &w) {
        Outcome::Steal if !allow_steal => {
            let coins: Vec<(Outcome, u64)> = w.into_iter().filter(|(o, _)| is_coins(*o)).collect();
            pick(rng, &coins)
        }
        o => o,
    }
}

/// The three hidden outcomes of one chest pick.
pub fn draw_chests(rng: &mut dyn Draw, streak: u32, allow_steal: bool) -> [Outcome; 3] {
    [
        draw_one(rng, streak, allow_steal),
        draw_one(rng, streak, allow_steal),
        draw_one(rng, streak, allow_steal),
    ]
}

/// What a steal takes from a victim holding `victim_coins`.
pub fn steal_amount(victim_coins: i64) -> i64 {
    (victim_coins * STEAL_PERCENT / 100).clamp(0, STEAL_CAP)
}

/// The roll (`below(total)`) that makes [`draw_one`] return `target` at this
/// streak, for tests that script the random source.
#[cfg(test)]
pub fn roll_for(streak: u32, target: Outcome) -> u64 {
    let mut before = 0u64;
    for (o, w) in weights(streak) {
        if o == target {
            return before;
        }
        before += w;
    }
    panic!("{target:?} is not in the table");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(odds: &[(Outcome, f64)], o: Outcome) -> f64 {
        odds.iter().filter(|(x, _)| *x == o).map(|(_, p)| p).sum()
    }

    #[test]
    fn the_table_sums_to_one_thousand_and_matches_the_design_at_streak_zero() {
        assert_eq!(TABLE.iter().map(|t| t.1).sum::<u32>(), 1000);
        let o = odds(0, true);
        let coins =
            p(&o, Outcome::Coins(20)) + p(&o, Outcome::Coins(40)) + p(&o, Outcome::Coins(80));
        assert!((coins - 0.55).abs() < 1e-9, "coins together are 55%");
        assert!((p(&o, Outcome::Jackpot) - 0.04).abs() < 1e-9);
        assert!((p(&o, Outcome::Booster) - 0.12).abs() < 1e-9);
        assert!((p(&o, Outcome::Shield) - 0.10).abs() < 1e-9);
        assert!((p(&o, Outcome::Steal) - 0.15).abs() < 1e-9);
        assert!((p(&o, Outcome::Slip) - 0.04).abs() < 1e-9);
    }

    #[test]
    fn rare_outcomes_are_twice_as_likely_from_streak_3_and_three_times_from_6() {
        assert_eq!(
            [0, 1, 2, 3, 5, 6, 20].map(rare_multiplier),
            [1, 1, 1, 2, 2, 3, 3]
        );
        let (a, b, c) = (odds(0, true), odds(3, true), odds(6, true));
        for rare in [Outcome::Jackpot, Outcome::Coins(80)] {
            let at = [p(&a, rare), p(&b, rare), p(&c, rare)];
            assert!(
                (at[1] - 2.0 * at[0]).abs() < 1e-12,
                "{rare:?} twice as likely from 3"
            );
            assert!(
                (at[2] - 3.0 * at[0]).abs() < 1e-12,
                "{rare:?} three times from 6"
            );
        }
        for o in [b.clone(), c.clone()] {
            let sum: f64 = o.iter().map(|(_, p)| p).sum();
            assert!((sum - 1.0).abs() < 1e-9);
        }
        // the others share the rest in their usual proportions: the slip does
        // not grow
        assert!(p(&c, Outcome::Slip) < p(&a, Outcome::Slip));
        assert!(p(&c, Outcome::Steal) < p(&a, Outcome::Steal));
        assert!((p(&c, Outcome::Booster) / p(&c, Outcome::Shield) - 1.2).abs() < 1e-9);
    }

    #[test]
    fn friendly_odds_have_no_steal() {
        let o = odds(0, false);
        assert_eq!(p(&o, Outcome::Steal), 0.0);
        assert!((o.iter().map(|(_, p)| p).sum::<f64>() - 1.0).abs() < 1e-9);
        // the steal's 15% went to coins, the rest is as before
        let coins =
            p(&o, Outcome::Coins(20)) + p(&o, Outcome::Coins(40)) + p(&o, Outcome::Coins(80));
        assert!((coins - 0.70).abs() < 1e-9);
        assert!((p(&o, Outcome::Slip) - 0.04).abs() < 1e-9);
        let mut rng = SeededDraw(7);
        assert!((0..5000).all(|_| draw_one(&mut rng, 0, false) != Outcome::Steal));
    }

    #[test]
    fn a_seeded_draw_follows_the_odds_and_is_reproducible() {
        let count = |streak: u32| {
            let mut rng = SeededDraw(42);
            let mut jackpots = 0;
            let mut steals = 0;
            let n = 60_000;
            for _ in 0..n {
                match draw_one(&mut rng, streak, true) {
                    Outcome::Jackpot => jackpots += 1,
                    Outcome::Steal => steals += 1,
                    _ => {}
                }
            }
            (
                f64::from(jackpots) / f64::from(n),
                f64::from(steals) / f64::from(n),
            )
        };
        let (j0, s0) = count(0);
        assert!((j0 - 0.04).abs() < 0.004, "jackpot at streak 0: {j0}");
        assert!((s0 - 0.15).abs() < 0.006, "steal at streak 0: {s0}");
        let (j6, _) = count(6);
        let expected = p(&odds(6, true), Outcome::Jackpot);
        assert!(
            (j6 - expected).abs() < 0.006,
            "jackpot at streak 6: {j6} vs {expected}"
        );
        assert!(j6 > 2.4 * j0);
        assert_eq!(count(3), count(3), "same seed, same draw");
        let mut a = SeededDraw(1);
        let mut b = SeededDraw(1);
        assert_eq!(draw_chests(&mut a, 2, true), draw_chests(&mut b, 2, true));
    }

    #[test]
    fn below_is_uniform_and_shuffle_is_a_permutation() {
        let mut rng = SeededDraw(9);
        let mut seen = [0u32; 6];
        for _ in 0..60_000 {
            seen[rng.below(6) as usize] += 1;
        }
        assert!(
            seen.iter().all(|&c| (9_000..11_000).contains(&c)),
            "{seen:?}"
        );
        let mut v: Vec<u16> = (0..10).collect();
        rng.shuffle(&mut v);
        let mut sorted = v.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..10).collect::<Vec<u16>>());
        assert_ne!(v, sorted, "shuffled");
    }

    #[test]
    fn a_steal_takes_ten_percent_at_most_a_hundred() {
        assert_eq!(steal_amount(0), 0);
        assert_eq!(steal_amount(95), 9);
        assert_eq!(steal_amount(500), 50);
        assert_eq!(steal_amount(5_000), 100);
    }

    #[test]
    fn the_system_draw_gives_different_numbers() {
        let mut rng = SystemDraw;
        let a: Vec<u64> = (0..4).map(|_| rng.next_u64()).collect();
        assert!(a.windows(2).all(|w| w[0] != w[1]));
    }
}
