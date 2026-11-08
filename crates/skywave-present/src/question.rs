//! Answering a question the agent asks ([`QuestionPrompt`]): what a head
//! keeps while the user picks, the rules a pick follows, and how an answer
//! reads. A single-select question takes one option or the user's own
//! text, the one replacing the other; a multi-select one takes any number
//! of options and, besides them, text. Every question must be answered
//! before the answers go out.

use skywave_core::{ApprovalId, Question, QuestionAnswer, QuestionPrompt};
use skywave_proto::answers_fit;

/// What the user has picked so far for one question prompt, one question
/// in view at a time. Keyed by the prompt's id: a head starts a new draft
/// when the id changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionDraft {
    id: ApprovalId,
    answers: Vec<QuestionAnswer>,
    current: usize,
}

impl QuestionDraft {
    /// Nothing picked yet, the first question in view.
    pub fn new(prompt: &QuestionPrompt) -> QuestionDraft {
        let blank = QuestionAnswer {
            picked: Vec::new(),
            other: None,
        };
        QuestionDraft {
            id: prompt.id,
            answers: vec![blank; prompt.questions.len()],
            current: 0,
        }
    }

    /// The prompt this draft answers.
    pub fn id(&self) -> ApprovalId {
        self.id
    }

    /// The question in view, by index.
    pub fn current(&self) -> usize {
        self.current
    }

    /// Brings question `index` into view; past the last, the last.
    pub fn show(&mut self, index: usize) {
        self.current = index.min(self.answers.len().saturating_sub(1));
    }

    /// What is picked for question `index` so far.
    pub fn answer(&self, index: usize) -> Option<&QuestionAnswer> {
        self.answers.get(index)
    }

    /// Picks option `option` of question `index`: for a single-select
    /// question it replaces whatever was picked or written; for a
    /// multi-select one it toggles. An option the question does not have
    /// changes nothing.
    pub fn toggle(&mut self, questions: &[Question], index: usize, option: usize) {
        let (Some(question), Some(answer)) = (questions.get(index), self.answers.get_mut(index))
        else {
            return;
        };
        let Ok(pick) = u8::try_from(option) else {
            return;
        };
        if option >= question.options.len() {
            return;
        }
        if !question.multi_select {
            answer.picked = vec![pick];
            answer.other = None;
            return;
        }
        match answer.picked.binary_search(&pick) {
            Ok(at) => {
                answer.picked.remove(at);
            }
            Err(at) => answer.picked.insert(at, pick),
        }
    }

    /// Sets the user's own text for question `index`; blank text clears
    /// it. For a single-select question, text replaces the option picked.
    pub fn write(&mut self, questions: &[Question], index: usize, text: &str) {
        let (Some(question), Some(answer)) = (questions.get(index), self.answers.get_mut(index))
        else {
            return;
        };
        if text.trim().is_empty() {
            answer.other = None;
            return;
        }
        answer.other = Some(text.to_owned());
        if !question.multi_select {
            answer.picked.clear();
        }
    }

    /// Whether question `index` has an answer that fits it.
    pub fn is_answered(&self, questions: &[Question], index: usize) -> bool {
        questions
            .get(index)
            .zip(self.answers.get(index))
            .is_some_and(|(question, answer)| question.takes(answer))
    }

    /// The first question with no answer that fits, if any.
    pub fn first_unanswered(&self, questions: &[Question]) -> Option<usize> {
        (0..questions.len()).find(|&index| !self.is_answered(questions, index))
    }

    /// The answers to send, once every question has one that fits.
    pub fn answers(&self, questions: &[Question]) -> Option<Vec<QuestionAnswer>> {
        answers_fit(questions, &self.answers).then(|| self.answers.clone())
    }
}

/// How `answer` reads for `question`: the labels picked, in the options'
/// order, then the user's own text, joined by `, `; `no answer` for none.
pub fn answer_text(question: &Question, answer: &QuestionAnswer) -> String {
    let picked = answer
        .picked
        .iter()
        .filter_map(|&i| question.options.get(usize::from(i)))
        .map(|option| option.label.as_str());
    let parts: Vec<&str> = picked.chain(answer.other.as_deref()).collect();
    if parts.is_empty() {
        "no answer".to_owned()
    } else {
        parts.join(", ")
    }
}

/// This device's answer to `prompt` in words, before how it travels:
/// `you answered: Rye · Seeds, Oats`, or `you declined`; `None` when it
/// gave none.
pub fn reply_text(prompt: &QuestionPrompt) -> Option<String> {
    prompt.answer.as_ref()?;
    if prompt.answers.is_empty() {
        return Some("you declined".to_owned());
    }
    let picked: Vec<String> = prompt
        .questions
        .iter()
        .zip(&prompt.answers)
        .map(|(q, a)| answer_text(q, a))
        .collect();
    Some(format!("you answered: {}", picked.join(" · ")))
}

/// How a question is named in one line: its header, else its text.
pub fn question_label(question: &Question) -> &str {
    if question.header.trim().is_empty() {
        &question.text
    } else {
        &question.header
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use skywave_core::QuestionOption;

    fn question(multi_select: bool) -> Question {
        let option = |label: &str| QuestionOption {
            label: label.into(),
            description: String::new(),
            preview: None,
        };
        Question {
            header: "Bread".into(),
            text: "Which bread?".into(),
            options: vec![option("Rye"), option("Spelt"), option("Wheat")],
            multi_select,
        }
    }

    fn draft(questions: &[Question]) -> QuestionDraft {
        QuestionDraft::new(&QuestionPrompt {
            id: ApprovalId::new(),
            call: None,
            from: None,
            questions: questions.to_vec(),
            answer: None,
            answers: Vec::new(),
        })
    }

    #[test]
    fn a_single_select_pick_replaces_the_last_and_text_replaces_a_pick() {
        let qs = [question(false)];
        let mut d = draft(&qs);
        assert_eq!(d.answers(&qs), None, "nothing picked");
        d.toggle(&qs, 0, 0);
        d.toggle(&qs, 0, 2);
        assert_eq!(answer_text(&qs[0], d.answer(0).unwrap()), "Wheat");
        d.write(&qs, 0, "Barley");
        assert_eq!(d.answer(0).unwrap().picked, Vec::<u8>::new());
        d.toggle(&qs, 0, 1);
        assert_eq!(d.answer(0).unwrap().other, None);
        assert_eq!(
            d.answers(&qs),
            Some(vec![QuestionAnswer {
                picked: vec![1],
                other: None,
            }])
        );
    }

    #[test]
    fn a_multi_select_pick_toggles_and_keeps_text_beside() {
        let qs = [question(true)];
        let mut d = draft(&qs);
        d.toggle(&qs, 0, 2);
        d.toggle(&qs, 0, 0);
        d.toggle(&qs, 0, 1);
        d.toggle(&qs, 0, 1);
        d.write(&qs, 0, "seeds");
        d.toggle(&qs, 0, 9);
        assert_eq!(
            answer_text(&qs[0], d.answer(0).unwrap()),
            "Rye, Wheat, seeds"
        );
        d.write(&qs, 0, "  ");
        d.toggle(&qs, 0, 0);
        d.toggle(&qs, 0, 2);
        assert_eq!(d.answer(0).unwrap().other, None);
        assert!(!d.is_answered(&qs, 0));
        assert_eq!(answer_text(&qs[0], d.answer(0).unwrap()), "no answer");
    }

    #[test]
    fn every_question_needs_an_answer() {
        let qs = [question(false), question(true)];
        let mut d = draft(&qs);
        d.toggle(&qs, 1, 0);
        assert_eq!(d.first_unanswered(&qs), Some(0));
        assert_eq!(d.answers(&qs), None);
        d.toggle(&qs, 0, 0);
        assert_eq!(d.first_unanswered(&qs), None);
        assert!(d.answers(&qs).is_some());
        d.show(7);
        assert_eq!(d.current(), 1);
    }
}
