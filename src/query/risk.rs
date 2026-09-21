//! Rank changed functions by how much review attention they are likely to need.
//!
//! Risk is a deterministic score over measured quantities. It is a ranking aid
//! and nothing more: it does not judge whether code is good, and it cannot know
//! what a change was for. Every assessment carries the score it produced and
//! the reasons that produced it, so a caller who disagrees with the weighting
//! can ignore the level and rank on the underlying numbers instead.
//!
//! The rules are additive. Each one that applies contributes its points and one
//! reason, and the total is bucketed into a level. Keeping the rules additive,
//! rather than letting any single signal decide, stops one large but harmless
//! number, such as a reformatted file's churn, from dominating the ranking.

use crate::{
    analysis::{FunctionChangeStatus, MatchConfidence},
    query::classify::FileClassification,
};

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

/// Score at or above which a change is reported as high risk.
const HIGH_RISK_SCORE: u32 = 5;
/// Score at or above which a change is reported as medium risk.
const MEDIUM_RISK_SCORE: u32 = 2;

/// The measured inputs a risk score is computed from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RiskSignals {
    pub classification: FileClassification,
    pub status: FunctionChangeStatus,
    pub cyclomatic_delta: i64,
    pub cognitive_delta: i64,
    /// Cognitive complexity the function ends up with.
    pub cognitive_after: u32,
    /// Changed lines inside the function, on either side.
    pub churned_lines: u32,
    pub match_confidence: MatchConfidence,
    pub exported: ExportStatus,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RiskAssessment {
    pub level: RiskLevel,
    pub score: u32,
    pub reasons: Vec<String>,
}

/// Score one changed function.
#[must_use]
pub fn assess(signals: &RiskSignals) -> RiskAssessment {
    let mut score = 0_u32;
    let mut reasons = Vec::new();
    let mut add = |points: u32, reason: String| {
        score += points;
        reasons.push(reason);
    };

    match signals.cognitive_delta {
        delta if delta >= 10 => add(3, format!("cognitive complexity increased by {delta}")),
        delta if delta >= 5 => add(2, format!("cognitive complexity increased by {delta}")),
        delta if delta >= 2 => add(1, format!("cognitive complexity increased by {delta}")),
        _ => {}
    }
    match signals.cyclomatic_delta {
        delta if delta >= 5 => add(2, format!("cyclomatic complexity increased by {delta}")),
        delta if delta >= 2 => add(1, format!("cyclomatic complexity increased by {delta}")),
        _ => {}
    }
    match signals.cognitive_after {
        after if after >= 30 => add(
            2,
            format!("cognitive complexity is {after} after the change"),
        ),
        after if after >= 15 => add(
            1,
            format!("cognitive complexity is {after} after the change"),
        ),
        _ => {}
    }
    if signals.churned_lines >= 30 {
        add(
            1,
            format!(
                "{} lines changed inside the function",
                signals.churned_lines
            ),
        );
    }
    if signals.status == FunctionChangeStatus::Added && signals.cognitive_after >= 10 {
        add(1, "new function is already non-trivial".to_owned());
    }
    match signals.exported {
        ExportStatus::Removed => add(
            3,
            "no longer exported; every importer of this name breaks".to_owned(),
        ),
        ExportStatus::Added => add(1, "newly part of the module's public surface".to_owned()),
        ExportStatus::Unchanged => {}
    }
    if signals.classification.is_source() {
        add(1, "production source file".to_owned());
    }
    if signals.match_confidence != MatchConfidence::Exact {
        reasons.push(format!(
            "matched with {:.1} confidence; verify it is the same function",
            signals.match_confidence.as_fraction()
        ));
    }

    RiskAssessment {
        level: level_for(score),
        score,
        reasons,
    }
}

fn level_for(score: u32) -> RiskLevel {
    if score >= HIGH_RISK_SCORE {
        RiskLevel::High
    } else if score >= MEDIUM_RISK_SCORE {
        RiskLevel::Medium
    } else {
        RiskLevel::Low
    }
}
