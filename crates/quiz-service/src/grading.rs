//! Server-side grading engine.
//!
//! Runs entirely in Rust over the question/option rows loaded inside the
//! submit transaction - the client never sends correctness data.
//!
//! Rules:
//!  * `single` / `boolean`: correct only for the exact correct option.
//!  * `multi`: partial credit
//!    (selected_correct - selected_wrong) / total_correct, floored at 0,
//!    full credit only when the exact set is chosen.

use crate::model::{QuestionKind, QuestionWithOptions};
use std::collections::HashMap;

/// Result of grading one question of an attempt.
#[derive(Clone, Debug)]
pub struct GradedAnswer {
    pub question_id: i64,
    pub selected: Vec<i64>,
    pub correct_ids: Vec<i64>,
    pub correct: bool,
    pub awarded: f64,
    pub possible: f64,
}

/// Result of grading a whole attempt.
#[derive(Clone, Debug, Default)]
pub struct GradedAttempt {
    pub answers: Vec<GradedAnswer>,
    pub score: f64,
    pub max_score: f64,
}

/// Grade `answers` (question_id -> selected option ids) against the quiz
/// questions. Questions with no submitted answer count as zero.
pub fn grade(questions: &[QuestionWithOptions], answers: &HashMap<i64, Vec<i64>>) -> GradedAttempt {
    let mut graded = Vec::with_capacity(questions.len());
    let mut score = 0.0f64;
    let mut max_score = 0.0f64;

    for qw in questions {
        let q = &qw.question;
        max_score += q.points;

        // Deduplicate and keep only options that belong to this question.
        let valid_ids: std::collections::HashSet<i64> = qw.options.iter().map(|o| o.id).collect();
        let selected: Vec<i64> = {
            let mut v: Vec<i64> = answers
                .get(&q.id)
                .map(|ids| {
                    ids.iter()
                        .copied()
                        .filter(|id| valid_ids.contains(id))
                        .collect()
                })
                .unwrap_or_default();
            v.sort_unstable();
            v.dedup();
            v
        };

        let correct_ids: Vec<i64> = qw
            .options
            .iter()
            .filter(|o| o.is_correct)
            .map(|o| o.id)
            .collect();

        let (correct, awarded) = match q.kind {
            QuestionKind::Single | QuestionKind::Boolean => {
                let ok =
                    correct_ids.len() == 1 && selected.len() == 1 && selected[0] == correct_ids[0];
                (ok, if ok { q.points } else { 0.0 })
            }
            QuestionKind::Multi => {
                let total_correct = correct_ids.len().max(1) as f64;
                let correct_set: std::collections::HashSet<i64> =
                    correct_ids.iter().copied().collect();
                let sel_correct = selected
                    .iter()
                    .filter(|id| correct_set.contains(*id))
                    .count() as f64;
                let sel_wrong = selected.len() as f64 - sel_correct;
                let ratio = ((sel_correct - sel_wrong) / total_correct).max(0.0);
                let awarded = (q.points * ratio).min(q.points);
                (ratio >= 1.0, awarded)
            }
        };

        score += awarded;
        graded.push(GradedAnswer {
            question_id: q.id,
            selected,
            correct_ids,
            correct,
            awarded,
            possible: q.points,
        });
    }

    // Clamp tiny float artifacts.
    score = round2(score);
    max_score = round2(max_score);
    GradedAttempt {
        answers: graded,
        score,
        max_score,
    }
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{OptionRow, QuestionRow};

    fn q(id: i64, kind: QuestionKind, points: f64, options: &[(i64, bool)]) -> QuestionWithOptions {
        QuestionWithOptions {
            question: QuestionRow {
                id,
                quiz_id: 1,
                position: 1,
                kind,
                text: format!("q{id}"),
                points,
                explanation: None,
            },
            options: options
                .iter()
                .enumerate()
                .map(|(i, (oid, correct))| OptionRow {
                    id: *oid,
                    question_id: id,
                    position: i as i64 + 1,
                    text: format!("o{oid}"),
                    is_correct: *correct,
                })
                .collect(),
        }
    }

    fn ans(map: &[(i64, Vec<i64>)]) -> HashMap<i64, Vec<i64>> {
        map.iter().cloned().collect()
    }

    #[test]
    fn single_exact_match_only() {
        let qs = vec![q(
            1,
            QuestionKind::Single,
            2.0,
            &[(10, true), (11, false), (12, false)],
        )];
        let g = grade(&qs, &ans(&[(1, vec![10])]));
        assert!(g.answers[0].correct);
        assert_eq!(g.score, 2.0);

        let g = grade(&qs, &ans(&[(1, vec![11])]));
        assert!(!g.answers[0].correct);
        assert_eq!(g.score, 0.0);

        let g = grade(&qs, &ans(&[]));
        assert_eq!(g.score, 0.0);
    }

    #[test]
    fn boolean_grades_like_single() {
        let qs = vec![q(1, QuestionKind::Boolean, 1.0, &[(10, false), (11, true)])];
        let g = grade(&qs, &ans(&[(1, vec![11])]));
        assert_eq!(g.score, 1.0);
    }

    #[test]
    fn multi_partial_credit() {
        // 2 correct of 4, 2 points
        let qs = vec![q(
            1,
            QuestionKind::Multi,
            2.0,
            &[(10, true), (11, false), (12, true), (13, false)],
        )];

        // all correct
        let g = grade(&qs, &ans(&[(1, vec![10, 12])]));
        assert!(g.answers[0].correct);
        assert_eq!(g.score, 2.0);

        // one correct, one wrong: (1 - 1)/2 = 0
        let g = grade(&qs, &ans(&[(1, vec![10, 11])]));
        assert!(!g.answers[0].correct);
        assert_eq!(g.score, 0.0);

        // only one correct selected: (1 - 0)/2 = 0.5
        let g = grade(&qs, &ans(&[(1, vec![10])]));
        assert!(!g.answers[0].correct);
        assert!((g.score - 1.0).abs() < 1e-9);

        // more wrong than correct clamps at zero
        let g = grade(&qs, &ans(&[(1, vec![11, 13])]));
        assert_eq!(g.score, 0.0);
    }

    #[test]
    fn foreign_option_ids_are_ignored() {
        let qs = vec![q(1, QuestionKind::Single, 1.0, &[(10, true), (11, false)])];
        let g = grade(&qs, &ans(&[(1, vec![10, 999])]));
        assert!(g.answers[0].correct, "unknown option ids must be ignored");
        assert_eq!(g.score, 1.0);
    }
}
