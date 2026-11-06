//! Server-side grading of one question.
//!
//! The client never sends correctness data; the key lives in the service.
//!
//! Rules:
//!  * `single` / `boolean`: full credit only for the exact correct option.
//!  * `multi`: partial credit `(selected_correct - selected_wrong) /
//!    total_correct`, floored at 0; `correct` only when the exact set is
//!    chosen.

use crate::model::{QuestionKind, QuestionWithOptions};
use std::collections::HashSet;

/// The outcome of grading one answer.
#[derive(Clone, Debug, PartialEq)]
pub struct Graded {
    /// Distinct chosen option ids that belong to the question, sorted.
    pub selected: Vec<i64>,
    /// Exactly the right set was chosen.
    pub correct: bool,
    /// Share of the full score earned, 0.0 to 1.0.
    pub credit: f64,
}

/// Ids of the correct options, in position order.
pub fn correct_ids(question: &QuestionWithOptions) -> Vec<i64> {
    question
        .options
        .iter()
        .filter(|o| o.is_correct)
        .map(|o| o.id)
        .collect()
}

/// Grade the chosen option ids against a question. Ids that do not belong
/// to it are ignored.
pub fn grade_question(question: &QuestionWithOptions, chosen: &[i64]) -> Graded {
    let valid: HashSet<i64> = question.options.iter().map(|o| o.id).collect();
    let mut selected: Vec<i64> = chosen
        .iter()
        .copied()
        .filter(|id| valid.contains(id))
        .collect();
    selected.sort_unstable();
    selected.dedup();

    let correct = correct_ids(question);
    match question.question.kind {
        QuestionKind::Single | QuestionKind::Boolean => {
            let ok = correct.len() == 1 && selected.len() == 1 && selected[0] == correct[0];
            Graded {
                selected,
                correct: ok,
                credit: if ok { 1.0 } else { 0.0 },
            }
        }
        QuestionKind::Multi => {
            let total = correct.len().max(1) as f64;
            let correct_set: HashSet<i64> = correct.iter().copied().collect();
            let right = selected
                .iter()
                .filter(|id| correct_set.contains(*id))
                .count() as f64;
            let wrong = selected.len() as f64 - right;
            let credit = ((right - wrong) / total).clamp(0.0, 1.0);
            Graded {
                selected,
                correct: credit >= 1.0,
                credit,
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::model::{OptionRow, QuestionRow};

    pub fn question(
        id: i64,
        kind: QuestionKind,
        time_limit_sec: i64,
        options: &[(i64, bool)],
    ) -> QuestionWithOptions {
        QuestionWithOptions {
            question: QuestionRow {
                id,
                quiz_id: 1,
                position: id,
                kind,
                text: format!("q{id}"),
                time_limit_sec,
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

    #[test]
    fn single_exact_match_only() {
        let q = question(
            1,
            QuestionKind::Single,
            20,
            &[(10, true), (11, false), (12, false)],
        );
        assert!(grade_question(&q, &[10]).correct);
        assert_eq!(grade_question(&q, &[10]).credit, 1.0);
        assert!(!grade_question(&q, &[11]).correct);
        assert_eq!(grade_question(&q, &[10, 11]).credit, 0.0);
        assert_eq!(grade_question(&q, &[]).credit, 0.0);
    }

    #[test]
    fn boolean_grades_like_single() {
        let q = question(1, QuestionKind::Boolean, 20, &[(10, false), (11, true)]);
        assert_eq!(grade_question(&q, &[11]).credit, 1.0);
        assert_eq!(grade_question(&q, &[10]).credit, 0.0);
    }

    #[test]
    fn multi_partial_credit() {
        let q = question(
            1,
            QuestionKind::Multi,
            20,
            &[(10, true), (11, false), (12, true), (13, false)],
        );
        let all = grade_question(&q, &[10, 12]);
        assert!(all.correct);
        assert_eq!(all.credit, 1.0);

        // one right, one wrong: (1 - 1) / 2 = 0
        assert_eq!(grade_question(&q, &[10, 11]).credit, 0.0);

        // only one of two right: 0.5
        let half = grade_question(&q, &[10]);
        assert!(!half.correct);
        assert!((half.credit - 0.5).abs() < 1e-9);

        // everything right plus one wrong: (2 - 1) / 2
        assert!((grade_question(&q, &[10, 12, 13]).credit - 0.5).abs() < 1e-9);

        // more wrong than right clamps at zero
        assert_eq!(grade_question(&q, &[11, 13]).credit, 0.0);
    }

    #[test]
    fn foreign_and_duplicate_ids_are_ignored() {
        let q = question(1, QuestionKind::Single, 20, &[(10, true), (11, false)]);
        let g = grade_question(&q, &[10, 10, 999]);
        assert!(g.correct);
        assert_eq!(g.selected, vec![10]);
    }
}
