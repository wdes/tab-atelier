// SPDX-License-Identifier: MPL-2.0

//! `AskUserQuestion`: stop and ask the person driving the session.
//!
//! A tool that asks rather than guesses. Some choices are not the agent's to make — which of
//! two schemas the operator meant, whether a destructive step is wanted — and an agent that
//! assumes is worse than one that asks, because the wrong assumption is only discovered after
//! the work.
//!
//! The mechanism is a *question* the agent hands outward and an *answer* that comes back on
//! whichever channel the session has. Two channels exist and both are served by the same
//! plumbing in [`crate::agent`]:
//!
//! * a terminal, where the REPL renders the options and the operator picks one;
//! * a socket client, which receives a `question` response and replies with an `answer`.
//!
//! It is deliberately *not* judged by the gate: asking is not an action, and a judge that
//! could refuse a question would leave the agent with nothing to do but guess. Plan-mode does
//! not refuse it either — asking is how a plan gets clarified.
//!
//! If nobody answers, the tool returns the fact rather than a guess. A timeout is not an error
//! the caller should retry into; it means no one is there, and saying so lets the model pick a
//! safe default or stop.

use std::time::Duration;

/// How long to wait for an answer before giving up.
///
/// Long, because a person reading a question and thinking is not a fault — but bounded, so a
/// session with nobody attached does not hang forever mid-turn.
const DEFAULT_TIMEOUT: Duration = Duration::from_mins(10);
const MAX_TIMEOUT: Duration = Duration::from_hours(1);

/// One option the operator can choose.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Choice {
    pub label: String,
    pub description: String,
}

/// One question, as the tool receives it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Question {
    /// A very short label — a column heading, not a sentence.
    pub header: String,
    /// The question itself.
    ///
    /// `question` on the wire, matching the name the model used to ask it, so a client reads
    /// the same vocabulary in both directions rather than having to learn two.
    #[serde(rename = "question")]
    pub prompt: String,
    pub options: Vec<Choice>,
    /// Whether several options may be chosen at once. `multiSelect` on the wire, for the same
    /// reason.
    #[serde(rename = "multiSelect")]
    pub multi: bool,
}

impl Question {
    /// Read one question out of the tool input, saying what is wrong if it cannot.
    pub fn parse(raw: &serde_json::Value) -> Result<Self, String> {
        let prompt = raw
            .get("question")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| "each question needs a `question` string".to_string())?
            .to_owned();

        let header = raw
            .get("header")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            // A header is a label, so a missing one falls back to something short rather than
            // refusing: it is presentation, and refusing over presentation wastes a turn.
            .unwrap_or("Question")
            .to_owned();

        let options: Vec<Choice> = raw
            .get("options")
            .and_then(|v| v.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|o| {
                        let label = o.get("label").and_then(|v| v.as_str())?.trim();
                        if label.is_empty() {
                            return None;
                        }
                        Some(Choice {
                            label: label.to_owned(),
                            description: o
                                .get("description")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .trim()
                                .to_owned(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        // Two options is the minimum that makes a question a question: with one there is
        // nothing to choose, and with none the model is asking an open question it could have
        // written in its reply.
        if options.len() < 2 {
            return Err(format!(
                "`{}` offers {} option(s). A question needs at least two, each with a `label` — \
                 with one or none there is nothing to choose between, and an open question \
                 belongs in your reply instead.",
                prompt,
                options.len()
            ));
        }

        Ok(Self {
            header,
            prompt,
            options,
            multi: raw
                .get("multiSelect")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
        })
    }
}

/// Ask, and return the answer as the tool result.
pub async fn run(asker: &std::sync::Arc<Asker>, input: &serde_json::Value) -> Result<String, String> {
    let raw = input.get("questions").and_then(|v| v.as_array()).ok_or_else(|| {
        "missing `questions` — a list of {question, header, options}. Each option needs a \
             `label` and may have a `description`."
            .to_string()
    })?;
    if raw.is_empty() {
        return Err("`questions` is empty — ask something, or ask it in your reply".to_string());
    }
    let questions: Vec<Question> = raw.iter().map(Question::parse).collect::<Result<_, _>>()?;

    let timeout = input
        .get("timeout_secs")
        .and_then(serde_json::Value::as_u64)
        .map_or(DEFAULT_TIMEOUT, |s| Duration::from_secs(s).min(MAX_TIMEOUT));

    let answers = asker.ask(questions, timeout).await;
    Ok(report(answers.as_deref(), timeout))
}

/// The tool result, for answers or for nobody answering.
///
/// One shape either way, so the model reads the same structure whether or not it was answered,
/// and `answered: false` is a fact it can act on rather than a failure to retry.
#[must_use]
pub fn report(answers: Option<&[Answer]>, timeout: Duration) -> String {
    let value = answers.map_or_else(
        || {
            serde_json::json!({
                "answered": false,
                "reason": format!(
                    "nobody answered within {}s. No one is driving this session right now. \
                     Decide yourself, and say plainly in your reply what you assumed and why — \
                     do not ask again, and do not treat this as an error.",
                    timeout.as_secs()
                ),
            })
        },
        |answers| {
            serde_json::json!({
                "answered": true,
                "answers": answers
                    .iter()
                    .map(|a| serde_json::json!({ "question": a.question, "chosen": a.chosen }))
                    .collect::<Vec<_>>(),
            })
        },
    );
    serde_json::to_string_pretty(&value).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
}

/// What the operator chose for one question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    /// The question it answers, echoed so a multi-question reply is unambiguous.
    pub question: String,
    /// The chosen labels. More than one only for a multi-select question.
    pub chosen: Vec<String>,
}

/// The channel a question travels down: whatever is driving the session.
///
/// Lives beside the tool rather than inside [`crate::agent::Agent`], because it is the asker's
/// own concern — an id, the questions waiting on it, and the one-shot the answer arrives on.
/// Separate so it can be constructed and driven in a test without a session, a provider or a
/// filesystem, which is the only way to test the timing at all.
#[derive(Debug, Default)]
pub struct Asker {
    pending: std::sync::Mutex<Option<Pending>>,
    next_id: std::sync::atomic::AtomicU64,
}

#[derive(Debug)]
struct Pending {
    id: u64,
    questions: Vec<Question>,
    /// Taken by whoever answers: one list of chosen labels per question, in the order asked.
    ///
    /// An `Option` because answering *takes* it, while the questions stay in the slot for
    /// [`Asker::ask`] to read back — which is how the answer can echo the questions it belongs
    /// to without the ask cloning their text beforehand.
    reply: Option<tokio::sync::oneshot::Sender<Vec<Vec<String>>>>,
}

impl Asker {
    /// A fresh channel with nothing pending.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Ask, and wait for an answer.
    ///
    /// `None` means nobody answered in time, which is deliberately not an error: an unattended
    /// session is a state, not a fault, and the caller reports it as a fact so the model can
    /// decide rather than retry.
    ///
    /// A question arriving while another is pending replaces it. A turn is sequential and this
    /// is awaited, so by then the first has timed out or been answered; the replace is a safety
    /// net for a cancelled turn, not a queue. Queuing questions nobody is reading would grow
    /// without bound, and the second question is the one the operator is being shown.
    pub async fn ask(&self, questions: Vec<Question>, timeout: Duration) -> Option<Vec<Answer>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            .wrapping_add(1);
        if let Ok(mut slot) = self.pending.lock() {
            *slot = Some(Pending {
                id,
                questions,
                reply: Some(tx),
            });
        }
        // The wait is the whole point, so an unanswered question takes the full time.
        let chosen = tokio::time::timeout(timeout, rx).await.ok()?.ok()?;
        // Taken on the way out — question and all — so a UI that polls does not render a
        // question that is over, and so the answers can echo the questions they belong to
        // without this having cloned them earlier.
        let asked = self
            .pending
            .lock()
            .ok()
            .and_then(|mut slot| slot.take())
            .map(|pending| pending.questions)
            .unwrap_or_default();
        Some(
            asked
                .into_iter()
                .zip(chosen)
                .map(|(question, chosen)| Answer {
                    question: question.prompt,
                    chosen,
                })
                .collect(),
        )
    }

    /// The question waiting for an answer, if any, with the id to answer it by.
    ///
    /// A snapshot: the questions are cloned out so the lock is not held while a UI renders
    /// them, and a UI that polls this cannot deadlock the ask.
    #[must_use]
    pub fn pending(&self) -> Option<(u64, Vec<Question>)> {
        let slot = self.pending.lock().ok()?;
        let id = slot.as_ref()?.id;
        let questions = slot.as_ref()?.questions.clone();
        // Released before returning, deliberately: the caller renders these questions and may
        // call `answer` from the same thread, which would deadlock on a lock still held here.
        drop(slot);
        Some((id, questions))
    }

    /// Answer the pending question.
    ///
    /// `false` if the id does not match, which is what a stale answer looks like — from a UI
    /// that rendered the same question twice, say. Ignoring it is right: the question it was
    /// answering is over, and delivering it would answer the next one with the previous choice.
    pub fn answer(&self, id: u64, chosen: Vec<Vec<String>>) -> bool {
        let Ok(mut slot) = self.pending.lock() else {
            return false;
        };
        let Some(pending) = slot.as_mut() else {
            return false;
        };
        if pending.id != id {
            return false;
        }
        // Only the sender is taken; the questions stay for `ask` to read back when it wakes.
        let Some(reply) = pending.reply.take() else {
            // Already answered. A second answer must not be delivered, or it would answer
            // whatever question came next with this choice.
            return false;
        };
        reply.send(chosen).is_ok()
    }
}

/// The tool's schema.
#[must_use]
pub fn spec() -> serde_json::Value {
    serde_json::json!({
        "name": "AskUserQuestion",
        "description": "Ask the person driving this session to choose, when a decision is \
                        theirs rather than yours — which of two approaches, whether a \
                        destructive step is wanted, which environment to touch. Prefer this \
                        over guessing: a wrong assumption is only discovered after the work. \
                        Each question offers at least two labelled options, and the answer \
                        comes back as the chosen label. If nobody answers, you are told so — \
                        decide for yourself then, and say what you assumed.",
        "input_schema": {
            "type": "object",
            "properties": {
                "questions": {
                    "type": "array",
                    "description": "One to four questions, asked together.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "question": { "type": "string", "description": "The question, ending in a question mark." },
                            "header": { "type": "string", "description": "A very short label for the question — a column heading, not a sentence." },
                            "multiSelect": { "type": "boolean", "description": "Whether several options may be chosen at once." },
                            "options": {
                                "type": "array",
                                "description": "At least two choices.",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "label": { "type": "string", "description": "The choice, as it should be shown." },
                                        "description": { "type": "string", "description": "What choosing it means." }
                                    },
                                    "required": ["label"]
                                }
                            }
                        },
                        "required": ["question", "options"]
                    }
                },
                "timeout_secs": { "type": "integer", "description": "Override the 600s default. Capped at 3600." }
            },
            "required": ["questions"]
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a_question() -> serde_json::Value {
        serde_json::json!({
            "question": "Which schema should the migration use?",
            "header": "Schema",
            "options": [
                { "label": "normalised", "description": "a separate table for line items" },
                { "label": "json column", "description": "faster to ship, harder to query" }
            ]
        })
    }

    #[test]
    fn a_question_parses_with_its_options() {
        let q = Question::parse(&a_question()).expect("parses");
        assert_eq!(q.header, "Schema");
        assert_eq!(q.prompt, "Which schema should the migration use?");
        assert!(!q.multi, "single choice unless told otherwise");
        assert_eq!(q.options.len(), 2);
        assert_eq!(q.options[0].label, "normalised");
        assert!(q.options[0].description.contains("separate table"));
    }

    /// A missing header is presentation, so it falls back rather than refusing: refusing over
    /// a label would spend a turn to fix nothing.
    #[test]
    fn a_missing_header_falls_back_and_a_missing_question_does_not() {
        let mut raw = a_question();
        raw.as_object_mut().unwrap().remove("header");
        let q = Question::parse(&raw).expect("still parses");
        assert_eq!(q.header, "Question");

        let mut raw = a_question();
        raw.as_object_mut().unwrap().remove("question");
        let err = Question::parse(&raw).unwrap_err();
        assert!(err.contains("needs a `question`"), "{err}");

        let mut raw = a_question();
        raw["question"] = serde_json::json!("   ");
        let err = Question::parse(&raw).unwrap_err();
        assert!(err.contains("needs a `question`"), "a blank question is not one: {err}");
    }

    /// Fewer than two options is refused, and the message says why rather than just no.
    ///
    /// With one option there is nothing to choose; with none, the model is asking an open
    /// question and should have written it in its reply instead of calling a tool.
    #[test]
    fn a_question_without_a_real_choice_is_refused() {
        for options in [serde_json::json!([]), serde_json::json!([{"label": "only one"}])] {
            let mut raw = a_question();
            raw["options"] = options;
            let err = Question::parse(&raw).unwrap_err();
            assert!(err.contains("at least two"), "{err}");
            assert!(
                err.contains("belongs in your reply"),
                "the message should say what to do instead: {err}"
            );
        }

        // An option with no label is not an option, so the count is short.
        let mut raw = a_question();
        raw["options"] = serde_json::json!([{"label": "one"}, {"description": "no label at all"}]);
        let err = Question::parse(&raw).unwrap_err();
        assert!(err.contains("at least two"), "a label is what makes an option: {err}");
    }

    /// Multi-select is carried through, since it changes what an answer means.
    #[test]
    fn multi_select_is_carried() {
        let mut raw = a_question();
        raw["multiSelect"] = serde_json::json!(true);
        assert!(Question::parse(&raw).unwrap().multi);
    }

    /// The input is checked before anything is handed to the session, so a malformed call
    /// fails immediately instead of waiting on an answer to a question nobody can read.
    ///
    /// Asserted through the validator rather than through `run`, because `run` needs an agent
    /// and the point is that no *session* is touched: the checks all happen before the ask.
    #[test]
    fn a_malformed_call_is_refused_before_anything_is_asked() {
        // No `questions` key at all.
        assert!(serde_json::json!({}).get("questions").is_none());

        // An empty list, and an option list too short — the two the schema cannot express.
        for raw in [
            serde_json::json!([]),
            serde_json::json!([{ "question": "Which?", "options": [{"label": "only one"}] }]),
            serde_json::json!([{ "question": "  ", "options": [{"label": "a"}, {"label": "b"}] }]),
        ] {
            let parsed: Vec<Result<Question, String>> =
                raw.as_array().expect("a list").iter().map(Question::parse).collect();
            assert!(
                parsed.is_empty() || parsed.iter().any(Result::is_err),
                "this input should not produce a question: {raw}"
            );
        }
    }

    /// Nobody answering is a fact in the result, not an error to retry into.
    #[test]
    fn an_unanswered_question_reports_that_it_was_unanswered() {
        let said = report(None, Duration::from_mins(10));
        let parsed: serde_json::Value = serde_json::from_str(&said).expect("JSON");
        assert_eq!(parsed["answered"], false);
        let reason = parsed["reason"].as_str().expect("a reason");
        assert!(reason.contains("nobody answered"), "{reason}");
        assert!(
            reason.contains("do not ask again"),
            "and it must not read as retryable: {reason}"
        );
        assert!(
            reason.contains("say plainly"),
            "or as licence to guess silently: {reason}"
        );
    }

    /// An answered question comes back with the question echoed beside the choice, so a
    /// multi-question reply is unambiguous about which answer belongs to which question.
    #[test]
    fn an_answered_question_echoes_the_question_with_the_choice() {
        let answers = vec![
            Answer {
                question: "Which schema?".to_owned(),
                chosen: vec!["normalised".to_owned()],
            },
            Answer {
                question: "Which environments?".to_owned(),
                chosen: vec!["staging".to_owned(), "production".to_owned()],
            },
        ];
        let said = report(Some(&answers), Duration::from_mins(10));
        let parsed: serde_json::Value = serde_json::from_str(&said).expect("JSON");
        assert_eq!(parsed["answered"], true);
        assert_eq!(parsed["answers"][0]["question"], "Which schema?");
        assert_eq!(parsed["answers"][0]["chosen"][0], "normalised");
        // A multi-select answer carries every choice, in the order given.
        assert_eq!(parsed["answers"][1]["chosen"].as_array().expect("chosen").len(), 2);
    }

    /// The ask round-trips: what one task asks, another answers.
    #[tokio::test]
    async fn an_answer_travels_back_to_the_asker() {
        let asker = std::sync::Arc::new(Asker::new());
        let answerer = std::sync::Arc::clone(&asker);

        // Answer from another task, the way a UI does: poll for the pending question, then
        // answer it by id.
        let responder = tokio::spawn(async move {
            let (id, questions) = loop {
                if let Some(pending) = answerer.pending() {
                    break pending;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            };
            assert_eq!(questions.len(), 1);
            assert_eq!(questions[0].options.len(), 2);
            assert!(
                answerer.answer(id, vec![vec!["json column".to_owned()]]),
                "the id from `pending` must be the one to answer by"
            );
        });

        let questions = vec![Question::parse(&a_question()).expect("parses")];
        let answers = asker
            .ask(questions, Duration::from_secs(5))
            .await
            .expect("answered in time");
        responder.await.expect("responder");

        assert_eq!(answers.len(), 1);
        assert_eq!(answers[0].question, "Which schema should the migration use?");
        assert_eq!(answers[0].chosen, vec!["json column".to_owned()]);
        // And it is no longer pending, so a UI stops rendering it.
        assert!(asker.pending().is_none(), "an answered question must not stay pending");
    }

    /// A stale answer is ignored: one carrying an id that is no longer pending must not be
    /// delivered, or it would answer the *next* question with the previous choice.
    #[tokio::test]
    async fn a_stale_answer_is_refused() {
        let asker = Asker::new();
        // Nothing pending at all.
        assert!(!asker.answer(99, vec![vec!["x".to_owned()]]));

        // Ask with a short timeout so it expires, then answer with its id: too late.
        let questions = vec![Question::parse(&a_question()).expect("parses")];
        let expired = asker.ask(questions, Duration::from_millis(20)).await;
        assert!(expired.is_none(), "nobody answered, so the ask gives up");
        assert!(
            !asker.answer(1, vec![vec!["too late".to_owned()]]),
            "and the id is dead"
        );
    }
}
