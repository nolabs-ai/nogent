//! Question (source-agnostic Q&A triage) prompts.
//!
//! System instruction is loaded from out-of-tree Markdown (see
//! [`crate::prompts`]); the user prompt is assembled here. All caller-supplied
//! content — the question, the prior conversation, and every context source —
//! is wrapped in untrusted-content tags so the model treats it as data, never
//! instructions. We do not escape it: the canary (not tag integrity) is the
//! injection gate, matching `issue_triage`/`pr_review`.

use crate::events::{EventJob, QuestionInput};

/// System instruction (role + grounding rules + injection defense + output
/// contract), canary-substituted.
#[must_use]
pub fn system_instruction(canary: &str) -> String {
    crate::prompts::question_system(canary)
}

/// Assemble the user turn from a job's [`QuestionInput`]. Returns an empty
/// string for non-question jobs (they never take this path).
#[must_use]
pub fn user_prompt(job: &EventJob) -> String {
    match &job.question {
        Some(input) => render(input),
        None => String::new(),
    }
}

fn render(input: &QuestionInput) -> String {
    let question = input.question.trim();
    let question = if question.is_empty() {
        "(no question text)"
    } else {
        question
    };

    let mut conversation = String::new();
    if input.conversation.is_empty() {
        conversation.push_str("(no prior conversation)\n");
    } else {
        for turn in &input.conversation {
            let name = turn.name.trim();
            let who = if name.is_empty() {
                turn.role.clone()
            } else {
                format!("{} ({name})", turn.role)
            };
            conversation.push_str(&format!("[{who}] {}\n", turn.content));
        }
    }

    let mut context = String::new();
    if input.context.is_empty() {
        context.push_str("(no context sources provided)\n");
    } else {
        for source in &input.context {
            context.push_str(&format!(
                "<source id=\"{id}\" type=\"{stype}\" trust=\"{trust}\" version=\"{version}\">\n\
Title: {title}\nURL: {url}\n{content}\n</source>\n",
                id = source.source_id,
                stype = source.source_type,
                trust = source.trust,
                version = source.version,
                title = source.title,
                url = source.url,
                content = source.content,
            ));
        }
    }

    format!(
        "<untrusted_question>\n{question}\n</untrusted_question>\n\n\
<untrusted_conversation>\n{conversation}</untrusted_conversation>\n\n\
<untrusted_context>\n{context}</untrusted_context>",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{ContextSource, ConversationTurn};

    fn source(id: &str, trust: &str) -> ContextSource {
        ContextSource {
            source_id: id.into(),
            source_type: "closed_issue".into(),
            title: "allow /tmp".into(),
            url: "https://x".into(),
            version: "2.4".into(),
            content: "Use --allow to grant a path.".into(),
            trust: trust.into(),
        }
    }

    fn question_job(conversation: Vec<ConversationTurn>, context: Vec<ContextSource>) -> EventJob {
        EventJob::question(
            QuestionInput {
                question: "How do I allow /tmp?".into(),
                conversation,
                context,
            },
            "gemini-3.5-flash".into(),
        )
    }

    #[test]
    fn wraps_all_caller_content_as_untrusted() {
        let job = question_job(
            vec![ConversationTurn {
                role: "user".into(),
                content: "still stuck".into(),
                name: String::new(),
            }],
            vec![source("github:issue:1", "verified")],
        );
        let u = user_prompt(&job);
        assert!(u.contains("<untrusted_question>"));
        assert!(u.contains("<untrusted_conversation>"));
        assert!(u.contains("<untrusted_context>"));
        assert!(u.contains("How do I allow /tmp?"));
        assert!(u.contains("[user] still stuck"));
        assert!(u.contains("id=\"github:issue:1\""));
        assert!(u.contains("trust=\"verified\""));
        assert!(u.contains("Use --allow to grant a path."));
    }

    #[test]
    fn empty_conversation_and_context_are_marked() {
        let u = user_prompt(&question_job(vec![], vec![]));
        assert!(u.contains("(no prior conversation)"));
        assert!(u.contains("(no context sources provided)"));
    }

    #[test]
    fn non_question_job_yields_empty_prompt() {
        let mut job = question_job(vec![], vec![]);
        job.question = None;
        assert!(user_prompt(&job).is_empty());
    }
}
