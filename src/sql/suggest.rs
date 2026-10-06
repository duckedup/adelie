//! "Did you mean" for bind errors (SPEC §11): the few candidate names closest to a typo.

/// Up to three `candidates` near `name`: case-insensitive exact matches first, then those
/// within Levenshtein distance max(2, len/3), nearest first, ties in input order.
pub(crate) fn closest<'a>(
    name: &str,
    candidates: impl IntoIterator<Item = &'a str>,
) -> Vec<&'a str> {
    let limit = (name.chars().count() / 3).max(2);
    let lower = name.to_lowercase();
    let mut scored: Vec<(usize, &'a str)> = Vec::new();
    for c in candidates {
        let c_lower = c.to_lowercase();
        if c_lower == lower {
            scored.push((0, c));
            continue;
        }
        let d = distance(&lower, &c_lower);
        if d <= limit {
            scored.push((d, c));
        }
    }
    scored.sort_by_key(|(d, _)| *d);
    scored.into_iter().take(3).map(|(_, c)| c).collect()
}

/// `; did you mean "a", "b"?`, or empty when there is nothing to suggest.
pub(crate) fn hint(name: &str, candidates: impl IntoIterator<Item = String>) -> String {
    let all: Vec<String> = candidates.into_iter().collect();
    let found = closest(name, all.iter().map(String::as_str));
    if found.is_empty() {
        return String::new();
    }
    let list: Vec<String> = found.iter().map(|c| format!("\"{c}\"")).collect();
    format!("; did you mean {}?", list.join(", "))
}

fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1];
        for (j, cb) in b.iter().enumerate() {
            let sub = prev[j] + usize::from(ca != *cb);
            cur.push(sub.min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typo_finds_the_name() {
        assert_eq!(closest("naem", ["id", "name", "age"]), vec!["name"]);
    }

    #[test]
    fn case_insensitive_match_ranks_first() {
        assert_eq!(closest("NAME", ["nam", "name"]), vec!["name", "nam"]);
    }

    #[test]
    fn far_names_are_not_suggested() {
        assert!(closest("zzzzzz", ["id", "name"]).is_empty());
    }

    #[test]
    fn at_most_three() {
        assert_eq!(closest("a", ["b", "c", "d", "e"]).len(), 3);
    }

    #[test]
    fn hint_formats_or_is_empty() {
        let c = || ["name".to_string()];
        assert_eq!(hint("naem", c()), "; did you mean \"name\"?");
        assert_eq!(hint("zzzzzz", c()), "");
    }
}
