//! Behavioral tests for the `match()` / `search()` function extensions across the
//! `regex` feature boundary — the realistic cases a consumer hits depending on whether
//! they enable the feature.
//!
//! Exhaustive RFC conformance lives in `compliance_test_suite.rs` (which requires the
//! feature). These pin the two things that suite can't, by design:
//! * with `regex` on — a *valid* pattern selects, and an *invalid* pattern yields
//!   `false` rather than an error (RFC 9535 §2.4.6), for both literal and
//!   document-derived patterns;
//! * with `regex` off — any use of `match()`/`search()` is rejected at parse time,
//!   regardless of whether the pattern itself is valid.

#[cfg(feature = "regex")]
mod with_regex {
    use jsonpath_rfc9535::JsonPath;
    use serde_json::json;

    #[test]
    fn valid_pattern_selects_only_matching_nodes() {
        let document = json!({ "items": [{ "id": "abc" }, { "id": "xyz" }] });
        let nodes = JsonPath::parse(r#"$.items[?match(@.id, "a.*")]"#)
            .expect("a valid query")
            .query_values(&document);
        assert_eq!(
            nodes,
            [&json!({ "id": "abc" })],
            "match() should select only the node whose value matches the pattern"
        );
    }

    #[test]
    fn search_finds_a_substring() {
        let document = json!(["hello world", "goodbye"]);
        let nodes = JsonPath::parse(r#"$[?search(@, "world")]"#)
            .expect("a valid query")
            .query_values(&document);
        assert_eq!(
            nodes,
            [&json!("hello world")],
            "search() should select the element containing the substring"
        );
    }

    #[test]
    fn invalid_literal_pattern_yields_false_not_an_error() {
        // RFC 9535 §2.4.6: a pattern that is not a valid I-Regexp makes the function
        // return `false`. The query still compiles — `(` is an unbalanced group — it
        // selects nothing.
        let document = json!([{ "a": "anything" }]);
        let query = JsonPath::parse(r#"$[?match(@.a, "(")]"#)
            .expect("an invalid pattern must not fail compilation");
        assert!(
            query.query_values(&document).is_empty(),
            "an invalid literal pattern should match nothing, not error"
        );
    }

    #[test]
    fn invalid_document_pattern_yields_false_not_an_error() {
        // The same rule for a pattern taken from the document, which is compiled at
        // evaluation time rather than at parse time.
        let document = json!({ "pattern": "(", "items": ["x"] });
        let nodes = JsonPath::parse(r"$.items[?search(@, $.pattern)]")
            .expect("a valid query")
            .query_values(&document);
        assert!(
            nodes.is_empty(),
            "an invalid document-derived pattern should match nothing, not error"
        );
    }
}

#[cfg(not(feature = "regex"))]
mod without_regex {
    use jsonpath_rfc9535::JsonPath;

    #[test]
    fn match_is_rejected_without_the_feature() {
        assert!(
            JsonPath::parse(r#"$[?match(@.a, "a.*")]"#).is_err(),
            "match() must be rejected at parse time when the `regex` feature is off"
        );
    }

    #[test]
    fn search_is_rejected_without_the_feature() {
        assert!(
            JsonPath::parse(r#"$[?search(@, "world")]"#).is_err(),
            "search() must be rejected at parse time when the `regex` feature is off"
        );
    }

    #[test]
    fn rejection_does_not_depend_on_pattern_validity() {
        // The pattern's validity is irrelevant without the feature: the function itself
        // is unavailable, so the query is rejected at parse time either way.
        assert!(
            JsonPath::parse(r#"$[?match(@.a, "(")]"#).is_err(),
            "an invalid pattern must still be rejected (not yield a pattern error)"
        );
    }
}
