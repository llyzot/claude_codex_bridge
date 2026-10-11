//! Explicit answers to Claude's structured questions; never a tool-policy grant.
use anyhow::{ensure, Context, Result};
use serde_json::Value;
use std::collections::HashSet;

fn bounded(value: &Value, max: usize) -> bool {
    value
        .as_str()
        .is_some_and(|s| !s.trim().is_empty() && s.len() <= max && !s.contains('\0'))
}
pub(crate) fn validate(input: &Value) -> Result<()> {
    let questions = input["questions"]
        .as_array()
        .context("Claude question list missing")?;
    ensure!(
        (1..=4).contains(&questions.len()),
        "Claude requires one to four questions"
    );
    let mut names = HashSet::new();
    for q in questions {
        ensure!(
            bounded(&q["question"], 4096) && names.insert(q["question"].as_str().unwrap()),
            "invalid or duplicate Claude question"
        );
        ensure!(
            q["multiSelect"].is_null() || q["multiSelect"].is_boolean(),
            "invalid Claude question selection mode"
        );
        let options = q["options"]
            .as_array()
            .context("Claude question options missing")?;
        ensure!(
            (2..=4).contains(&options.len()),
            "Claude question requires two to four options"
        );
        let mut labels = HashSet::new();
        for option in options {
            ensure!(
                bounded(&option["label"], 256) && labels.insert(option["label"].as_str().unwrap()),
                "invalid or duplicate Claude option"
            );
        }
    }
    Ok(())
}
pub(crate) fn answer(original: &Value, updated: &Value) -> Result<Value> {
    validate(original)?;
    ensure!(
        updated.is_object(),
        "Claude question answer requires updatedInput"
    );
    let mut expected = original.clone();
    let mut actual = updated.clone();
    for key in ["answers", "response"] {
        expected.as_object_mut().unwrap().remove(key);
        actual.as_object_mut().unwrap().remove(key);
    }
    ensure!(
        actual == expected,
        "Claude question answer cannot change questions or metadata"
    );
    if let Some(response) = updated.get("response") {
        ensure!(
            bounded(response, 8192),
            "invalid general Claude question response"
        );
        ensure!(
            updated.get("answers").is_none()
                || updated["answers"].as_object().is_some_and(|a| a.is_empty()),
            "general response conflicts with structured answers"
        );
    } else {
        let answers = updated["answers"]
            .as_object()
            .context("Claude question answers missing")?;
        let questions = original["questions"].as_array().unwrap();
        ensure!(
            answers.len() == questions.len(),
            "answer every Claude question exactly once"
        );
        for question in questions {
            let response = answers
                .get(question["question"].as_str().unwrap())
                .context("answer targets a different Claude question")?;
            if let Some(selected) = response.as_array() {
                let options = question["options"].as_array().unwrap();
                ensure!(
                    question["multiSelect"] == true
                        && !selected.is_empty()
                        && selected.len() <= options.len(),
                    "invalid multiple-choice Claude answer"
                );
                let mut distinct = HashSet::new();
                for choice in selected {
                    ensure!(
                        bounded(choice, 256)
                            && distinct.insert(choice.as_str().unwrap())
                            && options.iter().any(|o| o["label"] == *choice),
                        "unknown or duplicate Claude selected option"
                    );
                }
            } else {
                ensure!(
                    bounded(response, 8192),
                    "invalid Claude question text answer"
                );
            }
        }
    }
    Ok(updated.clone())
}
