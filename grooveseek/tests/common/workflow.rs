//! The `run:` steps of a GitHub Actions workflow, read from its YAML rather
//! than from its text.
//!
//! Two guards ask "what does this workflow run?" -- the pin guard, of
//! `ci.yml`, to compare it with the block that claims to reproduce it, and the
//! bench guard, of `nightly.yml`, for the targets it passes to `--bench` --
//! and `AGENTS.md` says two places answering one question call one
//! implementation. The bench guard used to scan the file as text, which
//! counted a `--bench` name in a comment as a step; a `run:` is read here and
//! nothing else is.
//!
//! Every `run:` value goes through [`crate::common::docs::read_block`], the
//! reader the fenced blocks go through, so a comment, a continuation and a
//! heredoc mean the same
//! thing on both sides of a comparison. A block scalar (`|`, `>`) arrives from
//! the parser as one string -- `|` with its newlines, `>` folded onto one line
//! -- and is split the same way. A line the reader cannot place is kept beside
//! the commands, with the reason, for the caller to report: a line dropped in
//! silence shrinks the set of commands, and a smaller set agrees more easily.
//!
//! A shape this reader does not know -- no `jobs`, a job without `steps`, a
//! step with neither `run` nor `uses` -- is an error rather than a skip, for
//! the same reason. A `uses:` step is not read at all: in `ci.yml` every
//! `uses:` is setup (checkout, toolchain, cache) and every `run:` is a check,
//! which is why every `run:` counts and no allowlist exists, and also why a
//! check written as an action would be invisible to a caller. The pin guard's
//! module doc names that blind spot; the day such an action arrives, this is
//! where it is taught apart from setup, with a reason.
//!
//! Not read: `shell:`, `working-directory:`, `if:`, `continue-on-error:`,
//! `env:`, the matrix. A `run:` containing `${{ matrix.x }}` compares as the
//! literal text, and a step CI runs but no longer requires to pass compares
//! as a step CI runs.

use super::docs::{LineRead, read_block};
use serde_yaml_bw::Value;

/// One step that runs something, located well enough to name in a failure.
#[derive(Clone, Debug)]
pub struct RunStep {
    pub job: String,
    /// 0-based index among the job's `steps`, `uses:` steps included: a path
    /// index, the way `yq '.jobs.fmt.steps[2]'` would name the same step, so
    /// a failure message can be pasted into a query against the file.
    pub index: usize,
    pub name: Option<String>,
    /// The `run:` text through [`read_block`], instructions only. Empty when
    /// it read as nothing; the caller decides what that means.
    pub commands: Vec<String>,
    /// The lines of the `run:` the reader could not place: the line as
    /// written and why, in file order. Not dropped, because a caller comparing
    /// [`RunStep::commands`] with something has to know what is missing from
    /// them.
    pub unread: Vec<UnreadLine>,
}

/// A line of a `run:` that [`read_block`] could not place.
#[derive(Clone, Debug)]
pub struct UnreadLine {
    /// 0-based index into the `run:` text's lines.
    pub index: usize,
    /// The line as the workflow writes it.
    pub raw: String,
    /// The reader's reason, as [`LineRead::Unread`] carries it.
    pub why: String,
}

impl RunStep {
    /// `jobs.<job>.steps[<index>]`, plus the step's name when it has one.
    pub fn position(&self) -> String {
        match &self.name {
            Some(name) => format!("jobs.{}.steps[{}] ({name})", self.job, self.index),
            None => format!("jobs.{}.steps[{}]", self.job, self.index),
        }
    }
}

/// Every `run:` step of the workflow, in file order.
///
/// `Err` names the first shape the reader does not know, with its position,
/// and the caller reports it in place of a comparison: a workflow half read is
/// a smaller set of commands, not a workflow with fewer steps.
pub fn run_steps(yaml: &str) -> Result<Vec<RunStep>, String> {
    let doc: Value = serde_yaml_bw::from_str(yaml).map_err(|e| format!("is not YAML: {e}"))?;
    let jobs = doc
        .get("jobs")
        .and_then(Value::as_mapping)
        .ok_or_else(|| "has no `jobs` mapping, so no step of it was read".to_string())?;
    let mut out = Vec::new();
    for (key, job) in jobs {
        let job_name = key
            .as_str()
            .ok_or_else(|| format!("has a job key that is not a string: {key:?}"))?;
        let steps = job
            .get("steps")
            .and_then(Value::as_sequence)
            .ok_or_else(|| {
                format!(
                    "jobs.{job_name} has no `steps` sequence, a job shape this reader does not know"
                )
            })?;
        for (index, step) in steps.iter().enumerate() {
            let at = format!("jobs.{job_name}.steps[{index}]");
            let step = step
                .as_mapping()
                .ok_or_else(|| format!("{at} is not a mapping"))?;
            let name = step.get("name").and_then(Value::as_str).map(str::to_string);
            match (step.get("run"), step.get("uses")) {
                (Some(run), None) => {
                    let text = run
                        .as_str()
                        .ok_or_else(|| format!("{at}: `run` is not a string"))?;
                    let mut commands = Vec::new();
                    let mut unread = Vec::new();
                    for line in read_block(text) {
                        match line.read {
                            LineRead::Instruction(command) => commands.push(command),
                            LineRead::Unread(why) => unread.push(UnreadLine {
                                index: line.index,
                                raw: line.raw,
                                why,
                            }),
                            LineRead::Continued
                            | LineRead::HeredocBody
                            | LineRead::Blank
                            | LineRead::Comment
                            | LineRead::Syntax => {}
                        }
                    }
                    out.push(RunStep {
                        job: job_name.to_string(),
                        index,
                        name,
                        commands,
                        unread,
                    });
                }
                (None, Some(_)) => {}
                (Some(_), Some(_)) => {
                    return Err(format!(
                        "{at} has both `run` and `uses`; a step does one or the other, so this \
                         is the workflow to fix rather than the reader"
                    ));
                }
                (None, None) => {
                    return Err(format!(
                        "{at} has neither `run` nor `uses`, a step shape this reader does not know"
                    ));
                }
            }
        }
    }
    Ok(out)
}
