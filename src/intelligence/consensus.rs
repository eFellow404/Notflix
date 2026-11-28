//! Sibling-level numeric consensus passes for episode ordering.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use super::patterns::{DIGITS_RE, SHORT_NUM_RE, YEAR_RE};
use super::AnalyzedEpisode;

const CONSTANT_SIBLING_MIN_RATIO: f64 = 0.75;
const SEE_MIN_CODE: i64 = 101;
const SEE_MAX_CODE: i64 = 2999;
const SEE_MAX_EPISODE: i64 = 45;

/// Identify numeric tokens that appear identically across >=75% of siblings.
///
/// Prevents show titles containing numbers (e.g. '24', 'The 100', 'Brooklyn 99',
/// or a series year like '2004') from being mistaken for episode numbers.
pub(super) fn find_constant_sibling_numbers(cleaned_stems: &[&str]) -> HashSet<String> {
    if cleaned_stems.len() < 2 {
        return HashSet::new();
    }

    let mut token_doc_freq: HashMap<&str, usize> = HashMap::new();
    let mut unique_nums: HashSet<&str> = HashSet::new();
    for cleaned in cleaned_stems {
        unique_nums.clear();
        unique_nums.extend(DIGITS_RE.find_iter(cleaned).map(|m| m.as_str()));
        for &num in &unique_nums {
            *token_doc_freq.entry(num).or_insert(0) += 1;
        }
    }

    let threshold =
        (((cleaned_stems.len() as f64) * CONSTANT_SIBLING_MIN_RATIO + 0.999) as usize).max(2);
    token_doc_freq
        .into_iter()
        .filter(|&(_, freq)| freq >= threshold)
        .map(|(num, _)| num.to_string())
        .collect()
}

/// For unresolved siblings sharing a naming template, extract the varying number.
pub(super) fn extract_sibling_diff_numbers(
    items: &mut [AnalyzedEpisode],
    cleaned_stems: &[String],
    indices_by_parent: &HashMap<&Path, Vec<usize>>,
    constant_by_parent: &HashMap<&Path, HashSet<String>>,
) {
    for (&parent, sibling_indices) in indices_by_parent {
        if sibling_indices.len() < 2 {
            continue;
        }
        let Some(constant_nums) = constant_by_parent.get(parent) else {
            continue;
        };

        for &idx in sibling_indices {
            let item = &mut items[idx];
            if item.explicit_season_episode || item.episode_number.is_some() {
                continue;
            }

            let cleaned = YEAR_RE.replace_all(&cleaned_stems[idx], " ");
            let last_candidate = SHORT_NUM_RE
                .find_iter(&cleaned)
                .map(|m| m.as_str())
                .filter(|tok| !constant_nums.contains(*tok))
                .last();

            if let Some(chosen) = last_candidate.and_then(|tok| tok.parse::<i64>().ok()) {
                item.episode_number = Some(chosen);
                item.raw_numeric_candidate = Some(chosen);
                item.confidence = item.confidence.max(0.84);
                item.sort_reason = format!("Sibling diff token #{chosen}");
            }
        }
    }
}

/// Detect whether 3-digit episode numbers (101, 102, 201, 202) encode Season+Episode.
pub(super) fn apply_three_digit_see_consensus(items: &mut [AnalyzedEpisode]) {
    let candidates: Vec<(usize, i64)> = items
        .iter()
        .enumerate()
        .filter_map(|(idx, it)| {
            let raw = it.raw_numeric_candidate?;
            (!it.explicit_season_episode
                && !it.is_extra
                && (SEE_MIN_CODE..=SEE_MAX_CODE).contains(&raw))
            .then_some((idx, raw))
        })
        .collect();

    if candidates.is_empty() {
        return;
    }

    let mut min_ep_by_season: HashMap<i64, i64> = HashMap::new();
    let mut total_grouped = 0usize;
    let mut folder_matches = 0usize;

    for &(idx, raw) in &candidates {
        let h = raw / 100;
        let ep = raw % 100;
        if (1..=SEE_MAX_EPISODE).contains(&ep) {
            min_ep_by_season
                .entry(h)
                .and_modify(|min_ep| *min_ep = (*min_ep).min(ep))
                .or_insert(ep);
            total_grouped += 1;
            if items[idx].season == Some(h) {
                folder_matches += 1;
            }
        }
    }

    // Valid SEE pattern if:
    // (a) multiple seasons are represented and episode offsets stay in a realistic season range, OR
    // (b) numbers consistently match their enclosing Season folder (e.g. Season 2/201.mp4).
    let multi_season = min_ep_by_season.len() >= 2
        && total_grouped == candidates.len()
        && min_ep_by_season.values().all(|&min_ep| min_ep <= 5);
    let folder_confirmed = folder_matches >= (candidates.len() / 2).max(1) && folder_matches > 0;

    if !(multi_season || folder_confirmed) {
        return;
    }

    for &(idx, raw) in &candidates {
        let s = raw / 100;
        let e = raw % 100;
        if (1..=SEE_MAX_EPISODE).contains(&e) {
            let it = &mut items[idx];
            it.season = Some(s);
            it.episode_number = Some(e);
            it.confidence = it.confidence.max(0.88);
            it.sort_reason = format!("Inferred S{s:02}E{e:02} from {raw} numbering");
        }
    }
}
