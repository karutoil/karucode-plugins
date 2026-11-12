//! Research plans and deterministic query reformulation.
//!
//! Explicit aspects are user-supplied coverage obligations. Without them the
//! engine investigates the direct answer, context, evidence/applications and
//! limitations. A later round targets the weakest aspects with a different query
//! instead of blindly re-running the initial search.

use crate::model::Aspect;
use serde_json::Value;

pub const MAX_QUERY_ATTEMPTS: usize = 6;

pub fn requested_aspects(args: &Value) -> Result<Vec<String>, String> {
    let Some(value) = args.get("aspects").filter(|v| !v.is_null()) else {
        return Ok(Vec::new());
    };
    let values = value
        .as_array()
        .ok_or_else(|| "aspects must be an array of strings".to_string())?;
    if values.len() > 12 {
        return Err("at most 12 explicit aspects are allowed".into());
    }
    let mut result = Vec::new();
    for value in values {
        let aspect = value
            .as_str()
            .ok_or_else(|| "aspects must contain only strings".to_string())?
            .trim();
        if aspect.is_empty() {
            return Err("aspects cannot contain an empty string".into());
        }
        if aspect.len() > 500 {
            return Err("each aspect must be at most 500 bytes".into());
        }
        if !result
            .iter()
            .any(|existing: &String| existing.eq_ignore_ascii_case(aspect))
        {
            result.push(aspect.to_string());
        }
    }
    Ok(result)
}

pub fn build_aspects(question: &str, requested: &[String]) -> Vec<Aspect> {
    let mut aspects = vec![aspect(
        "core",
        "Direct answer to the research question",
        question.to_string(),
    )];
    if !requested.is_empty() {
        for (index, label) in requested.iter().enumerate() {
            aspects.push(aspect(
                &format!("aspect-{}", index + 1),
                label,
                format!("{question} {label}"),
            ));
        }
        return aspects;
    }

    aspects.push(aspect(
        "context",
        "Definitions, background, architecture, and how it works",
        format!("{question} definition background architecture how it works"),
    ));
    aspects.push(aspect(
        "evidence",
        "Evidence, examples, data, and practical applications",
        format!("{question} evidence examples data practical applications"),
    ));
    aspects.push(aspect(
        "limitations",
        "Limitations, criticisms, risks, and open questions",
        format!("{question} limitations criticisms risks challenges open questions"),
    ));
    let lower = question.to_lowercase();
    if ["current", "latest", "recent", "today", "state of the art"]
        .iter()
        .any(|word| lower.contains(word))
    {
        aspects.push(aspect(
            "current",
            "Current status, recent developments, and primary-source dates",
            format!("{question} latest developments announcement release primary source"),
        ));
    }
    if ["compare", "versus", " vs ", "alternative", "choose", "best"]
        .iter()
        .any(|word| lower.contains(word))
    {
        aspects.push(aspect(
            "comparison",
            "Alternatives, trade-offs, and decision criteria",
            format!("{question} alternatives tradeoffs decision criteria"),
        ));
    }
    aspects
}

fn aspect(id: &str, label: &str, seed_query: String) -> Aspect {
    Aspect {
        id: id.to_string(),
        label: label.to_string(),
        seed_query,
        attempts: 0,
        source_ids: Vec::new(),
    }
}

/// Query variant for the aspect's next attempt. Each version changes the kind of
/// source requested, not just punctuation, which gives stalled runs a real way
/// out while the attempt ceiling prevents an infinite paraphrase loop.
pub fn next_query(aspect: &Aspect, question: &str) -> Option<String> {
    let query = match aspect.attempts {
        0 => aspect.seed_query.clone(),
        1 if aspect.id == "core" => {
            format!("{question} official documentation specification primary source")
        }
        1 => format!(
            "{question} {} official documentation detailed guide",
            aspect.label
        ),
        2 if aspect.id == "core" => format!("{question} review analysis evidence comparison"),
        2 => format!("{question} {} research data case study", aspect.label),
        3 if aspect.id == "core" => {
            format!("{question} criticism limitations alternatives open questions")
        }
        3 => format!(
            "{question} {} limitations controversy criticism alternatives",
            aspect.label
        ),
        4 if aspect.id == "core" => format!("{question} reference documentation FAQ"),
        4 => format!(
            "{question} {} explained examples technical reference",
            aspect.label
        ),
        5 if aspect.id == "core" => format!("{question} recent developments expert analysis"),
        5 => format!("{question} {} recent developments update", aspect.label),
        _ => return None,
    };
    Some(query.chars().take(1_500).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_aspects_replace_the_generic_checklist() {
        let aspects = build_aspects("Compare A and B", &["Pricing".into(), "Security".into()]);
        assert_eq!(aspects.len(), 3);
        assert_eq!(aspects[0].id, "core");
        assert_eq!(aspects[1].label, "Pricing");
        assert!(!aspects.iter().any(|a| a.id == "limitations"));
    }

    #[test]
    fn default_plans_cover_more_than_the_direct_answer() {
        let aspects = build_aspects("What is the current state of X?", &[]);
        for id in ["core", "context", "evidence", "limitations", "current"] {
            assert!(aspects.iter().any(|a| a.id == id), "missing {id}");
        }
    }

    #[test]
    fn reformulations_are_distinct_and_finite() {
        let aspect = aspect("core", "Core", "question".into());
        let queries: Vec<_> = (0..20)
            .map(|attempt| {
                let mut a = aspect.clone();
                a.attempts = attempt;
                next_query(&a, "question")
            })
            .collect();
        assert!(queries[MAX_QUERY_ATTEMPTS].is_none());
        let present: Vec<_> = queries.into_iter().flatten().collect();
        let unique: std::collections::BTreeSet<_> = present.iter().collect();
        assert_eq!(present.len(), MAX_QUERY_ATTEMPTS);
        assert_eq!(unique.len(), present.len());
    }
}
