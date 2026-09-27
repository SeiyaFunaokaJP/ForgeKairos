//! Histogram diff (as in jgit / `git diff --histogram`) over interned token ids.
//!
//! Anchoring on the rarest common tokens keeps unrelated `(`, `,` and `<STR>` tokens from being
//! paired just because they happen to line up, which matters more here than a minimal edit script.

use std::collections::HashMap;

/// Tokens that occur more often than this in a region are never used as anchors.
const MAX_CHAIN_LENGTH: usize = 64;
/// Regions without a usable anchor fall back to an exact LCS when they are at most this big
/// (product of both lengths).
const MAX_FALLBACK_CELLS: usize = 4_000_000;

/// Returns the matched `(old_index, new_index)` pairs in increasing order.
pub fn diff(old: &[u32], new: &[u32]) -> Vec<(usize, usize)> {
    let mut matches = Vec::new();
    let mut pending = vec![(0, old.len(), 0, new.len())];
    while let Some((old_start, old_end, new_start, new_end)) = pending.pop() {
        let (mut old_start, mut old_end, mut new_start, mut new_end) =
            (old_start, old_end, new_start, new_end);
        while old_start < old_end && new_start < new_end && old[old_start] == new[new_start] {
            matches.push((old_start, new_start));
            old_start += 1;
            new_start += 1;
        }
        while old_start < old_end && new_start < new_end && old[old_end - 1] == new[new_end - 1] {
            old_end -= 1;
            new_end -= 1;
            matches.push((old_end, new_end));
        }
        if old_start == old_end || new_start == new_end {
            continue;
        }
        let old_region = &old[old_start..old_end];
        let new_region = &new[new_start..new_end];
        match longest_rare_run(old_region, new_region) {
            Some(run) => {
                for offset in 0..run.length {
                    matches.push((
                        old_start + run.old_start + offset,
                        new_start + run.new_start + offset,
                    ));
                }
                pending.push((
                    old_start,
                    old_start + run.old_start,
                    new_start,
                    new_start + run.new_start,
                ));
                pending.push((
                    old_start + run.old_start + run.length,
                    old_end,
                    new_start + run.new_start + run.length,
                    new_end,
                ));
            }
            None => {
                if old_region.len().saturating_mul(new_region.len()) <= MAX_FALLBACK_CELLS {
                    for (old_offset, new_offset) in exact_lcs(old_region, new_region) {
                        matches.push((old_start + old_offset, new_start + new_offset));
                    }
                }
            }
        }
    }
    matches.sort_unstable();
    matches
}

struct Run {
    old_start: usize,
    new_start: usize,
    length: usize,
    rarity: usize,
}

fn longest_rare_run(old: &[u32], new: &[u32]) -> Option<Run> {
    let mut occurrences: HashMap<u32, Vec<usize>> = HashMap::new();
    for (index, token) in old.iter().enumerate() {
        occurrences.entry(*token).or_default().push(index);
    }
    let mut best: Option<Run> = None;
    let mut new_index = 0;
    while new_index < new.len() {
        let mut next_new_index = new_index + 1;
        if let Some(positions) = occurrences.get(&new[new_index])
            && positions.len() <= MAX_CHAIN_LENGTH
            && best
                .as_ref()
                .is_none_or(|best| positions.len() <= best.rarity)
        {
            for &old_index in positions {
                let mut start_old = old_index;
                let mut start_new = new_index;
                while start_old > 0 && start_new > 0 && old[start_old - 1] == new[start_new - 1] {
                    start_old -= 1;
                    start_new -= 1;
                }
                let mut end_old = old_index + 1;
                let mut end_new = new_index + 1;
                let mut rarity = positions.len();
                while end_old < old.len() && end_new < new.len() && old[end_old] == new[end_new] {
                    if let Some(other) = occurrences.get(&old[end_old]) {
                        rarity = rarity.min(other.len());
                    }
                    end_old += 1;
                    end_new += 1;
                }
                next_new_index = next_new_index.max(end_new);
                let length = end_old - start_old;
                let better = match &best {
                    None => true,
                    Some(best) => {
                        rarity < best.rarity || (rarity == best.rarity && length > best.length)
                    }
                };
                if better {
                    best = Some(Run {
                        old_start: start_old,
                        new_start: start_new,
                        length,
                        rarity,
                    });
                }
            }
        }
        new_index = next_new_index;
    }
    best
}

fn exact_lcs(old: &[u32], new: &[u32]) -> Vec<(usize, usize)> {
    let columns = new.len() + 1;
    let mut lengths = vec![0u32; (old.len() + 1) * columns];
    for old_index in (0..old.len()).rev() {
        for new_index in (0..new.len()).rev() {
            let cell = old_index * columns + new_index;
            lengths[cell] = if old[old_index] == new[new_index] {
                lengths[cell + columns + 1] + 1
            } else {
                lengths[cell + columns].max(lengths[cell + 1])
            };
        }
    }
    let mut result = Vec::new();
    let (mut old_index, mut new_index) = (0, 0);
    while old_index < old.len() && new_index < new.len() {
        let cell = old_index * columns + new_index;
        if old[old_index] == new[new_index] {
            result.push((old_index, new_index));
            old_index += 1;
            new_index += 1;
        } else if lengths[cell + columns] >= lengths[cell + 1] {
            old_index += 1;
        } else {
            new_index += 1;
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn ids(text: &str) -> Vec<u32> {
        text.bytes().map(u32::from).collect()
    }

    fn common(old: &str, new: &str) -> String {
        let old_ids = ids(old);
        diff(&old_ids, &ids(new))
            .into_iter()
            .map(|(old_index, _)| char::from(old.as_bytes()[old_index]))
            .collect()
    }

    #[test]
    fn identical_and_disjoint() {
        assert_eq!(common("abc", "abc"), "abc");
        assert_eq!(common("abc", "xyz"), "");
        assert_eq!(common("", "xyz"), "");
    }

    #[test]
    fn insertion_and_deletion() {
        assert_eq!(common("abcdef", "abXcdYef"), "abcdef");
        assert_eq!(common("abXcdYef", "abcdef"), "abcdef");
    }

    #[test]
    fn matches_are_monotonic() {
        let old = ids("the quick brown fox jumps over the lazy dog");
        let new = ids("a quick red fox leaped over lazy dogs");
        let matches = diff(&old, &new);
        for window in matches.windows(2) {
            assert!(window[0].0 < window[1].0 && window[0].1 < window[1].1);
        }
        for (old_index, new_index) in matches {
            assert_eq!(old[old_index], new[new_index]);
        }
    }

    #[test]
    fn anchors_on_rare_tokens() {
        // `A` and `B` are unique; the generic `(s)` groups must stay with their own callee
        // instead of drifting onto the inserted `C(s)`.
        let old = "A(s),B(s),";
        let new = "A(s),C(s),B(s),";
        let old_ids = ids(old);
        let new_ids = ids(new);
        let matches = diff(&old_ids, &new_ids);
        let b_old = old.find('B').unwrap_or_default();
        let b_new = new.find('B').unwrap_or_default();
        for offset in 0..5 {
            assert!(matches.contains(&(b_old + offset, b_new + offset)));
        }
        for offset in 0..5 {
            assert!(matches.contains(&(offset, offset)));
        }
    }
}
