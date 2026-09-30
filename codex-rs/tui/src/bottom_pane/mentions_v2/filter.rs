use codex_file_search::FileMatch;
use codex_file_search::MatchType;
use codex_utils_fuzzy_match::fuzzy_match;

use super::candidate::Candidate;
use super::candidate::MentionType;
use super::candidate::SearchResult;
use super::candidate::Selection;
use super::search_mode::SearchMode;

pub(super) fn filtered_candidates(
    candidates: &[Candidate],
    file_matches: &[FileMatch],
    query: &str,
    search_mode: SearchMode,
    show_file_matches: bool,
) -> Vec<SearchResult> {
    let filter = query.trim();
    let mut out = Vec::new();
    let peer_filter = codex_claude_peer::names::normalize_name(filter.trim_matches('"'));
    let mut peer_count = 0;

    for candidate in candidates {
        if !search_mode.accepts(candidate.mention_type) {
            continue;
        }
        if candidate.mention_type == MentionType::Peer {
            if !peer_filter.is_empty()
                && peer_count < 20
                && codex_claude_peer::names::normalize_name(&candidate.display_name)
                    .starts_with(&peer_filter)
            {
                out.push(candidate.to_result(None, 0));
                peer_count += 1;
            }
            continue;
        }
        if filter.is_empty() {
            out.push(candidate.to_result(/*match_indices*/ None, /*score*/ 0));
            continue;
        }

        if let Some((indices, score)) = best_tool_match(candidate, filter) {
            out.push(candidate.to_result(indices, score));
        }
    }

    if show_file_matches {
        out.extend(
            file_matches
                .iter()
                .map(file_match_to_row)
                .filter(|candidate| search_mode.accepts(candidate.mention_type)),
        );
    }

    sort_rows(&mut out, filter);
    out
}

fn best_tool_match(candidate: &Candidate, filter: &str) -> Option<(Option<Vec<usize>>, i32)> {
    if let Some((indices, score)) = fuzzy_match(&candidate.display_name, filter) {
        return Some((Some(indices), score));
    }

    candidate
        .search_terms
        .iter()
        .filter(|term| *term != &candidate.display_name)
        .filter_map(|term| fuzzy_match(term, filter).map(|(_indices, score)| score))
        .min()
        .map(|score| (None, score))
}

fn sort_rows(rows: &mut [SearchResult], filter: &str) {
    let type_order = |mention_type: MentionType| match mention_type {
        MentionType::Peer => 0,
        MentionType::Plugin => 1,
        MentionType::Skill => 2,
        MentionType::Task => 3,
        MentionType::File | MentionType::Directory => 4,
    };

    rows.sort_by(|a, b| {
        if a.mention_type == b.mention_type
            && matches!(a.mention_type, MentionType::Task | MentionType::Peer)
        {
            return std::cmp::Ordering::Equal;
        }
        type_order(a.mention_type)
            .cmp(&type_order(b.mention_type))
            .then_with(|| compare_within_rank(a, b, filter))
            .then_with(|| a.display_name.cmp(&b.display_name))
    });
}

fn compare_within_rank(a: &SearchResult, b: &SearchResult, filter: &str) -> std::cmp::Ordering {
    if a.mention_type.is_filesystem() && b.mention_type.is_filesystem() {
        return b.score.cmp(&a.score);
    }
    if filter.is_empty() {
        return a.display_name.cmp(&b.display_name);
    }

    a.match_indices
        .is_none()
        .cmp(&b.match_indices.is_none())
        .then_with(|| a.score.cmp(&b.score))
}

fn file_match_to_row(file_match: &FileMatch) -> SearchResult {
    let mention_type = match file_match.match_type {
        MatchType::File => MentionType::File,
        MatchType::Directory => MentionType::Directory,
    };
    SearchResult {
        display_name: file_match.path.to_string_lossy().to_string(),
        description: None,
        mention_type,
        selection: Selection::File(file_match.path.clone()),
        match_indices: file_match
            .indices
            .as_ref()
            .map(|indices| indices.iter().map(|idx| *idx as usize).collect()),
        score: file_match.score as i32,
    }
}

#[cfg(test)]
mod peer_tests {
    use super::super::search_catalog::build_peer_catalog;
    use super::*;
    use crate::peer_mentions::PeerMention;

    fn peer(name: &str, id: usize) -> PeerMention {
        PeerMention {
            name: name.into(),
            reference: format!("{id:06x}"),
            address: format!("uds:/tmp/cc-socks/{id}.sock"),
            session_id: format!("00000000-0000-4000-8000-{id:012x}"),
            status: "idle".into(),
        }
    }

    #[test]
    fn peer_mentions_are_prefix_filtered_and_bind_the_address() {
        let peers = [peer("claudex 업데이트", 1), peer("CSM_TEST", 2)];
        let catalog = build_peer_catalog(&peers);
        assert!(filtered_candidates(&catalog, &[], "", SearchMode::Results, false).is_empty());
        for query in ["cl", "ＣＬ", "\"claudex 업"] {
            let matches = filtered_candidates(&catalog, &[], query, SearchMode::Results, false);
            assert_eq!(matches.len(), 1);
            assert_eq!(matches[0].display_name, "claudex 업데이트");
            assert_eq!(
                matches[0].selection,
                Selection::Tool {
                    insert_text: "@\"claudex 업데이트\"".into(),
                    path: Some("claude-peer://00000000-0000-4000-8000-000000000001".into()),
                }
            );
        }
        assert!(
            filtered_candidates(&catalog, &[], "update", SearchMode::Results, false).is_empty()
        );
        assert!(
            filtered_candidates(&catalog, &[], "cl", SearchMode::FilesystemOnly, false).is_empty()
        );
    }

    #[test]
    fn peer_mentions_preserve_duplicates_as_distinct_targets_and_cap_results() {
        let peers = (0..30).map(|id| peer("claudex", id)).collect::<Vec<_>>();
        let catalog = build_peer_catalog(&peers);
        let matches = filtered_candidates(&catalog, &[], "CL", SearchMode::Results, false);
        assert_eq!(matches.len(), 20);
        assert_ne!(matches[0].selection, matches[1].selection);
        assert!(
            matches[0]
                .description
                .as_deref()
                .unwrap()
                .contains("000000")
        );
        assert!(
            matches[19]
                .description
                .as_deref()
                .unwrap()
                .contains("000013")
        );
    }
}
