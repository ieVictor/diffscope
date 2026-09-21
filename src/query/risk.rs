//! Rank changed functions by how much review attention they are likely to need.
//!
//! Two additive models score every changed function.
//!
//! *Intrinsic risk* is what the change did to the function's own code:
//! complexity growth, the complexity it ends up with, churn, and the caveat
//! that a match was not exact. Two identical edits to identically sized
//! functions score the same wherever they live.
//!
//! *Review priority* starts from intrinsic risk and adds how exposed the
//! function is: whether its name moved in or out of the module's public
//! surface, how many modules import the module containing it, and whether that
//! module is production source. Splitting the two keeps exposure out of the
//! code-only number, so a caller can rank on either.
//!
//! Neither model judges whether code is good, and neither can know what a
//! change was for. Every rule that applies contributes points and one
//! [`Reason`], and the total is bucketed into a level. Keeping the rules
//! additive, rather than letting any single signal decide, stops one large but
//! harmless number, such as a reformatted file's churn, from dominating the
//! ranking.
//!
//! An added function has no earlier revision to grow from, so its complexity is
//! scored as the absolute value it starts at, never as an increase.

use serde::{Serialize, Serializer};

use crate::{
    analysis::{FunctionChangeStatus, MatchConfidence},
    query::classify::FileClassification,
};

/// Identifier of the intrinsic-risk model.
pub const RISK_MODEL: &str = "diffscope-risk-v1";
/// Identifier of the review-priority model.
pub const REVIEW_PRIORITY_MODEL: &str = "diffscope-review-priority-v1";

/// Score at or above which either model reports a function as high.
pub const HIGH_SCORE: u32 = 5;
/// Score at or above which either model reports a function as medium.
pub const MEDIUM_SCORE: u32 = 2;

/// Largest score the intrinsic-risk model can produce.
///
/// The top tier of every intrinsic rule: cognitive complexity increase (3),
/// cyclomatic complexity increase (2), resulting cognitive complexity (2), and
/// churn (1).
pub const RISK_MAXIMUM_SCORE: u32 = 3 + 2 + 2 + 1;

/// Largest score the review-priority model can produce.
///
/// [`RISK_MAXIMUM_SCORE`] plus the top tier of every exposure signal: a removed
/// export (3), a heavily imported containing module (2), and production source
/// classification (1).
pub const REVIEW_PRIORITY_MAXIMUM_SCORE: u32 = RISK_MAXIMUM_SCORE + 3 + 2 + 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RiskLevel {
    Low,
    Medium,
    High,
}

impl RiskLevel {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            _ => None,
        }
    }
}

impl Serialize for RiskLevel {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// The measured inputs both models score.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RiskSignals {
    pub classification: FileClassification,
    pub status: FunctionChangeStatus,
    pub cyclomatic_delta: i64,
    pub cognitive_delta: i64,
    /// Cognitive complexity the function ends up with.
    pub cognitive_after: u32,
    /// Cyclomatic complexity the function ends up with.
    pub cyclomatic_after: u32,
    /// Changed lines inside the function, on either side.
    pub churned_lines: u32,
    pub match_confidence: MatchConfidence,
    pub exported: ExportStatus,
    /// Modules importing the module containing this function directly, when the
    /// import graph is available. `None` means it was not built, not that
    /// nothing imports it.
    pub direct_importers: Option<u32>,
}

/// What the change did to this function's name in the module's exports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportStatus {
    /// The name was not added to or removed from the public surface.
    Unchanged,
    /// The name is newly exported.
    Added,
    /// The name is no longer exported, which breaks every importer of it.
    Removed,
}

/// One rule's contribution, in structured form.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Reason {
    /// Stable `snake_case` identifier of the rule that produced this reason.
    pub code: &'static str,
    /// The rule's explanation, including the measured value.
    pub message: String,
    /// The measured quantity behind the reason, when the rule has an integer
    /// one. Rules that judge a state, such as an export or a classification,
    /// have none, and the field is then left out of the serialized reason.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<i64>,
}

/// One model's verdict on a function.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScoreAssessment {
    /// Model identifier, such as `diffscope-risk-v1`.
    pub model: &'static str,
    /// Largest score this model can produce.
    pub maximum_score: u32,
    /// Points this function accumulated.
    pub score: u32,
    /// Bucketed form of `score`.
    pub level: RiskLevel,
    /// Every rule that applied, in rule order.
    pub reasons: Vec<Reason>,
}

/// Both models' verdicts on one changed function.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RiskAssessment {
    /// What the change did to the function's own code.
    pub risk: ScoreAssessment,
    /// Intrinsic risk plus how exposed the function is: its place in the
    /// module's public surface, the reach of that module, and its
    /// classification. `reasons` repeats the intrinsic reasons first, then the
    /// exposure reasons, then the match caveat the intrinsic model carries.
    pub review_priority: ScoreAssessment,
}

/// Score one changed function with both models.
#[must_use]
pub fn assess(signals: &RiskSignals) -> RiskAssessment {
    let mut risk = intrinsic(signals);
    let confidence = confidence_reason(signals);

    let mut review_priority = risk.clone();
    review_priority.merge(exposure(signals));

    if let Some(reason) = confidence {
        risk.reasons.push(reason.clone());
        review_priority.reasons.push(reason);
    }

    RiskAssessment {
        risk: verdict(RISK_MODEL, RISK_MAXIMUM_SCORE, risk),
        review_priority: verdict(
            REVIEW_PRIORITY_MODEL,
            REVIEW_PRIORITY_MAXIMUM_SCORE,
            review_priority,
        ),
    }
}

/// Score what the change did to the function's own code.
fn intrinsic(signals: &RiskSignals) -> Tally {
    let mut tally = Tally::default();

    if signals.status == FunctionChangeStatus::Added {
        added_complexity(signals, &mut tally);
    } else {
        changed_complexity(signals, &mut tally);
    }

    let churned = signals.churned_lines;
    if churned >= 30 {
        tally.add(
            1,
            "high_churn",
            format!("{churned} lines changed inside the function"),
            Some(i64::from(churned)),
        );
    }

    tally
}

/// Complexity of a function the change added.
///
/// There is no earlier revision to grow from, so the measured complexity is the
/// absolute value the function starts at, and the reasons say so.
fn added_complexity(signals: &RiskSignals, tally: &mut Tally) {
    match signals.cognitive_after {
        after if after >= 30 => tally.add(
            3,
            "added_function_cognitive_complexity",
            format!("new function has cognitive complexity {after}"),
            Some(i64::from(after)),
        ),
        after if after >= 15 => tally.add(
            2,
            "added_function_cognitive_complexity",
            format!("new function has cognitive complexity {after}"),
            Some(i64::from(after)),
        ),
        after if after >= 10 => tally.add(
            1,
            "added_function_cognitive_complexity",
            format!("new function has cognitive complexity {after}"),
            Some(i64::from(after)),
        ),
        _ => {}
    }
    match signals.cyclomatic_after {
        after if after >= 20 => tally.add(
            2,
            "added_function_cyclomatic_complexity",
            format!("new function has cyclomatic complexity {after}"),
            Some(i64::from(after)),
        ),
        after if after >= 10 => tally.add(
            1,
            "added_function_cyclomatic_complexity",
            format!("new function has cyclomatic complexity {after}"),
            Some(i64::from(after)),
        ),
        _ => {}
    }
}

/// Complexity the change added to a function that already existed.
fn changed_complexity(signals: &RiskSignals, tally: &mut Tally) {
    match signals.cognitive_delta {
        delta if delta >= 10 => tally.add(
            3,
            "cognitive_complexity_increased",
            format!("cognitive complexity increased by {delta}"),
            Some(delta),
        ),
        delta if delta >= 5 => tally.add(
            2,
            "cognitive_complexity_increased",
            format!("cognitive complexity increased by {delta}"),
            Some(delta),
        ),
        delta if delta >= 2 => tally.add(
            1,
            "cognitive_complexity_increased",
            format!("cognitive complexity increased by {delta}"),
            Some(delta),
        ),
        _ => {}
    }
    match signals.cyclomatic_delta {
        delta if delta >= 5 => tally.add(
            2,
            "cyclomatic_complexity_increased",
            format!("cyclomatic complexity increased by {delta}"),
            Some(delta),
        ),
        delta if delta >= 2 => tally.add(
            1,
            "cyclomatic_complexity_increased",
            format!("cyclomatic complexity increased by {delta}"),
            Some(delta),
        ),
        _ => {}
    }
    match signals.cognitive_after {
        after if after >= 30 => tally.add(
            2,
            "cognitive_complexity_after",
            format!("cognitive complexity is {after} after the change"),
            Some(i64::from(after)),
        ),
        after if after >= 15 => tally.add(
            1,
            "cognitive_complexity_after",
            format!("cognitive complexity is {after} after the change"),
            Some(i64::from(after)),
        ),
        _ => {}
    }
}

/// Score how far the function's change reaches beyond its own body.
fn exposure(signals: &RiskSignals) -> Tally {
    let mut tally = Tally::default();

    match signals.exported {
        ExportStatus::Removed => tally.add(
            3,
            "export_removed",
            "no longer exported; every importer of this name breaks".to_owned(),
            None,
        ),
        ExportStatus::Added => tally.add(
            1,
            "export_added",
            "newly part of the module's public surface".to_owned(),
            None,
        ),
        ExportStatus::Unchanged => {}
    }
    match signals.direct_importers {
        Some(importers) if importers >= 20 => tally.add(
            2,
            "containing_module_direct_importers",
            format!("containing module is imported directly by {importers} modules"),
            Some(i64::from(importers)),
        ),
        Some(importers) if importers >= 5 => tally.add(
            1,
            "containing_module_direct_importers",
            format!("containing module is imported directly by {importers} modules"),
            Some(i64::from(importers)),
        ),
        _ => {}
    }
    if signals.classification.is_source() {
        tally.add(
            1,
            "production_source",
            "production source file".to_owned(),
            None,
        );
    }

    tally
}

/// The match caveat carries no points: it asks the reader to verify the pair, it
/// does not make the change riskier.
fn confidence_reason(signals: &RiskSignals) -> Option<Reason> {
    let confidence = signals.match_confidence;
    (confidence != MatchConfidence::Exact).then(|| Reason {
        code: "match_confidence",
        message: format!(
            "matched with {:.1} confidence; verify it is the same function",
            confidence.as_fraction()
        ),
        value: None,
    })
}

fn verdict(model: &'static str, maximum_score: u32, tally: Tally) -> ScoreAssessment {
    ScoreAssessment {
        model,
        maximum_score,
        score: tally.score,
        level: level_for(tally.score),
        reasons: tally.reasons,
    }
}

fn level_for(score: u32) -> RiskLevel {
    if score >= HIGH_SCORE {
        RiskLevel::High
    } else if score >= MEDIUM_SCORE {
        RiskLevel::Medium
    } else {
        RiskLevel::Low
    }
}

/// A score under construction: points accumulated together with the reasons
/// that earned them, in rule order.
#[derive(Debug, Clone, Default)]
struct Tally {
    score: u32,
    reasons: Vec<Reason>,
}

impl Tally {
    fn add(&mut self, points: u32, code: &'static str, message: String, value: Option<i64>) {
        self.score += points;
        self.reasons.push(Reason {
            code,
            message,
            value,
        });
    }

    fn merge(&mut self, other: Self) {
        self.score += other.score;
        self.reasons.extend(other.reasons);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ExportStatus, HIGH_SCORE, MEDIUM_SCORE, REVIEW_PRIORITY_MAXIMUM_SCORE,
        REVIEW_PRIORITY_MODEL, RISK_MAXIMUM_SCORE, RISK_MODEL, Reason, RiskLevel, RiskSignals,
        ScoreAssessment, assess,
    };
    use crate::{
        analysis::{FunctionChangeStatus, MatchConfidence},
        query::classify::FileClassification,
    };

    /// Signals with nothing for either model to score.
    fn quiet() -> RiskSignals {
        RiskSignals {
            classification: FileClassification::Test,
            status: FunctionChangeStatus::Modified,
            cyclomatic_delta: 0,
            cognitive_delta: 0,
            cognitive_after: 0,
            cyclomatic_after: 0,
            churned_lines: 0,
            match_confidence: MatchConfidence::Exact,
            exported: ExportStatus::Unchanged,
            direct_importers: None,
        }
    }

    fn codes(assessment: &ScoreAssessment) -> Vec<&'static str> {
        assessment
            .reasons
            .iter()
            .map(|reason| reason.code)
            .collect()
    }

    fn reason<'a>(assessment: &'a ScoreAssessment, code: &str) -> &'a Reason {
        assessment
            .reasons
            .iter()
            .find(|reason| reason.code == code)
            .unwrap_or_else(|| panic!("no {code} reason in {:?}", codes(assessment)))
    }

    #[test]
    fn quiet_function_scores_zero_and_carries_the_model_metadata() {
        let assessment = assess(&quiet());

        assert_eq!(assessment.risk.model, RISK_MODEL);
        assert_eq!(assessment.risk.maximum_score, RISK_MAXIMUM_SCORE);
        assert_eq!(assessment.risk.score, 0);
        assert_eq!(assessment.risk.level, RiskLevel::Low);
        assert!(assessment.risk.reasons.is_empty());

        assert_eq!(assessment.review_priority.model, REVIEW_PRIORITY_MODEL);
        assert_eq!(
            assessment.review_priority.maximum_score,
            REVIEW_PRIORITY_MAXIMUM_SCORE
        );
        assert_eq!(assessment.review_priority.score, 0);
        assert_eq!(assessment.review_priority.level, RiskLevel::Low);
        assert!(assessment.review_priority.reasons.is_empty());
    }

    #[test]
    fn every_rule_at_its_top_tier_reaches_the_model_maximum() {
        let mut signals = quiet();
        signals.status = FunctionChangeStatus::Modified;
        signals.cognitive_delta = 12;
        signals.cognitive_after = 40;
        signals.cyclomatic_delta = 6;
        signals.cyclomatic_after = 25;
        signals.churned_lines = 40;
        signals.exported = ExportStatus::Removed;
        signals.direct_importers = Some(25);
        signals.classification = FileClassification::Source;

        let assessment = assess(&signals);
        assert_eq!(assessment.risk.score, RISK_MAXIMUM_SCORE);
        assert_eq!(
            assessment.review_priority.score,
            REVIEW_PRIORITY_MAXIMUM_SCORE
        );
        assert_eq!(assessment.risk.level, RiskLevel::High);
        assert_eq!(assessment.review_priority.level, RiskLevel::High);

        // An added function cannot reach the intrinsic maximum: it has no
        // increase to score, only the absolute complexity it starts at.
        signals.status = FunctionChangeStatus::Added;
        signals.exported = ExportStatus::Added;
        assert_eq!(assess(&signals).risk.score, 3 + 2 + 1);
    }

    #[test]
    fn added_functions_use_absolute_complexity_never_increased_wording() {
        let mut added = quiet();
        added.status = FunctionChangeStatus::Added;
        added.cognitive_delta = 12;
        added.cognitive_after = 12;
        added.cyclomatic_delta = 11;
        added.cyclomatic_after = 11;

        let assessment = assess(&added);
        let cognitive = reason(&assessment.risk, "added_function_cognitive_complexity");
        assert_eq!(
            cognitive.message,
            "new function has cognitive complexity 12"
        );
        assert_eq!(cognitive.value, Some(12));
        let cyclomatic = reason(&assessment.risk, "added_function_cyclomatic_complexity");
        assert_eq!(
            cyclomatic.message,
            "new function has cyclomatic complexity 11"
        );
        assert_eq!(cyclomatic.value, Some(11));
        assert_eq!(assessment.risk.score, 2);
        assert!(assessment.review_priority.reasons.iter().all(|reason| {
            !reason.message.contains("increased") && !reason.code.contains("increased")
        }));

        let mut modified = quiet();
        modified.cognitive_delta = 12;
        modified.cognitive_after = 12;
        modified.cyclomatic_delta = 11;

        let assessment = assess(&modified);
        assert_eq!(
            reason(&assessment.risk, "cognitive_complexity_increased").message,
            "cognitive complexity increased by 12"
        );
        assert_eq!(assessment.risk.score, 3 + 2);
        assert!(!codes(&assessment.risk).contains(&"added_function_cognitive_complexity"));
    }

    #[test]
    fn containing_module_importers_raise_review_priority_alone() {
        let mut signals = quiet();
        signals.direct_importers = Some(7);
        signals.classification = FileClassification::Source;

        let assessment = assess(&signals);
        assert_eq!(assessment.risk.score, 0);
        assert_eq!(assessment.risk.level, RiskLevel::Low);
        assert!(assessment.risk.reasons.is_empty());

        let importers = reason(
            &assessment.review_priority,
            "containing_module_direct_importers",
        );
        assert_eq!(
            importers.message,
            "containing module is imported directly by 7 modules"
        );
        assert_eq!(importers.value, Some(7));
        let source = reason(&assessment.review_priority, "production_source");
        assert_eq!(source.message, "production source file");
        assert_eq!(source.value, None);
        assert_eq!(assessment.review_priority.score, 1 + 1);
        assert_eq!(assessment.review_priority.level, RiskLevel::Medium);

        let mut crowded = quiet();
        crowded.direct_importers = Some(25);
        let crowded = assess(&crowded);
        assert_eq!(
            reason(
                &crowded.review_priority,
                "containing_module_direct_importers"
            )
            .value,
            Some(25)
        );
        assert_eq!(crowded.review_priority.score, 2);

        let mut rare = quiet();
        rare.direct_importers = Some(4);
        assert!(assess(&rare).review_priority.reasons.is_empty());
    }

    #[test]
    fn review_priority_extends_intrinsic_risk_with_exposure() {
        let mut signals = quiet();
        signals.cognitive_delta = 2;
        signals.churned_lines = 31;
        signals.exported = ExportStatus::Removed;

        let assessment = assess(&signals);
        assert_eq!(assessment.risk.score, 1 + 1);
        assert_eq!(assessment.risk.level, RiskLevel::Medium);
        assert!(!codes(&assessment.risk).contains(&"export_removed"));
        assert_eq!(assessment.review_priority.score, 1 + 1 + 3);
        assert_eq!(assessment.review_priority.level, RiskLevel::High);
        assert!(
            assessment
                .review_priority
                .reasons
                .starts_with(&assessment.risk.reasons)
        );
        assert_eq!(
            assessment.review_priority.reasons.len(),
            assessment.risk.reasons.len() + 1
        );
        assert_eq!(
            reason(&assessment.review_priority, "export_removed").message,
            "no longer exported; every importer of this name breaks"
        );

        signals.exported = ExportStatus::Added;
        let assessment = assess(&signals);
        assert_eq!(
            reason(&assessment.review_priority, "export_added").message,
            "newly part of the module's public surface"
        );
        assert_eq!(assessment.review_priority.score, 1 + 1 + 1);
    }

    #[test]
    fn confidence_is_a_caveat_in_both_models_and_carries_no_points() {
        let mut signals = quiet();
        signals.match_confidence = MatchConfidence::Positional;

        let assessment = assess(&signals);
        assert_eq!(assessment.risk.score, 0);
        let confidence = reason(&assessment.risk, "match_confidence");
        assert_eq!(
            confidence.message,
            "matched with 0.6 confidence; verify it is the same function"
        );
        assert_eq!(confidence.value, None);
        assert_eq!(
            assessment.risk.reasons.last().map(|reason| reason.code),
            Some("match_confidence")
        );
        assert_eq!(assessment.review_priority.score, 0);
        assert_eq!(
            assessment
                .review_priority
                .reasons
                .last()
                .map(|reason| reason.code),
            Some("match_confidence")
        );
    }

    #[test]
    fn levels_bucket_at_the_documented_thresholds_and_parse_round_trips() {
        let mut low = quiet();
        low.cognitive_delta = 2;
        assert_eq!(assess(&low).risk.level, RiskLevel::Low);

        let mut medium = quiet();
        medium.cognitive_delta = 5;
        let assessment = assess(&medium);
        assert_eq!(assessment.risk.score, MEDIUM_SCORE);
        assert_eq!(assessment.risk.level, RiskLevel::Medium);

        let mut high = quiet();
        high.cognitive_delta = 10;
        high.cyclomatic_delta = 5;
        let assessment = assess(&high);
        assert_eq!(assessment.risk.score, HIGH_SCORE);
        assert_eq!(assessment.risk.level, RiskLevel::High);

        for level in [RiskLevel::Low, RiskLevel::Medium, RiskLevel::High] {
            assert_eq!(RiskLevel::parse(level.as_str()), Some(level));
        }
        assert_eq!(RiskLevel::parse("High"), None);
        assert_eq!(RiskLevel::parse("extreme"), None);
    }

    #[test]
    fn serialized_shape_matches_the_documented_keys() {
        let mut signals = quiet();
        signals.cognitive_delta = 7;
        signals.direct_importers = Some(9);
        signals.classification = FileClassification::Source;
        signals.match_confidence = MatchConfidence::IdenticalBody;

        let value = serde_json::to_value(assess(&signals)).expect("assessment serializes");
        let risk = &value["risk"];
        assert_eq!(risk["model"], RISK_MODEL);
        assert_eq!(
            risk["maximum_score"].as_u64(),
            Some(u64::from(RISK_MAXIMUM_SCORE))
        );
        assert_eq!(risk["score"].as_u64(), Some(2));
        assert_eq!(risk["level"], "medium");
        assert_eq!(risk["reasons"][0]["code"], "cognitive_complexity_increased");
        assert_eq!(
            risk["reasons"][0]["message"],
            "cognitive complexity increased by 7"
        );
        assert_eq!(risk["reasons"][0]["value"].as_i64(), Some(7));
        assert_eq!(risk["reasons"][1]["code"], "match_confidence");
        assert!(risk["reasons"][1].get("value").is_none());

        let priority = &value["review_priority"];
        assert_eq!(priority["model"], REVIEW_PRIORITY_MODEL);
        assert_eq!(
            priority["maximum_score"].as_u64(),
            Some(u64::from(REVIEW_PRIORITY_MAXIMUM_SCORE))
        );
        assert_eq!(priority["score"].as_u64(), Some(2 + 1 + 1));
        assert_eq!(priority["level"], "medium");
        assert_eq!(
            priority["reasons"][1]["code"],
            "containing_module_direct_importers"
        );
        assert_eq!(priority["reasons"][2]["code"], "production_source");
    }
}
