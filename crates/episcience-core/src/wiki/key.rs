use uuid::Uuid;

/// Stable identity of a wiki page: the theme's clustering provenance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WikiKey {
    pub run_id: Uuid,
    pub cluster_id: i64,
    pub split_part: Option<i64>,
}

impl WikiKey {
    /// From `claim_themes.properties`. `None` unless `cluster_run_id` is a uuid
    /// string and `cluster_id` a non-negative integer. `split_part` is optional
    /// (absent or `null` = not split), but when present it must also be a
    /// non-negative integer: a malformed one fails closed rather than collapsing
    /// a split child onto its unsplit parent's key. Non-negative ids guarantee
    /// every accepted key round-trips through [`Self::parse_slug`].
    pub fn from_properties(p: &serde_json::Value) -> Option<WikiKey> {
        let run_id = Uuid::parse_str(p.get("cluster_run_id")?.as_str()?).ok()?;
        let cluster_id = p.get("cluster_id")?.as_i64().filter(|n| *n >= 0)?;
        let split_part = match p.get("split_part") {
            None | Some(serde_json::Value::Null) => None,
            Some(v) => Some(v.as_i64().filter(|n| *n >= 0)?),
        };
        Some(WikiKey {
            run_id,
            cluster_id,
            split_part,
        })
    }

    /// `r{run_id as 32 lowercase hex}-c{cluster_id}` plus `-s{split_part}` when
    /// present. The FULL run id is kept: a truncated prefix of a UUIDv7 run id
    /// is a timestamp and would merge the histories of different runs.
    pub fn as_slug(&self) -> String {
        let run = self.run_id.simple();
        match self.split_part {
            Some(s) => format!("r{run}-c{}-s{s}", self.cluster_id),
            None => format!("r{run}-c{}", self.cluster_id),
        }
    }

    /// Inverse of [`Self::as_slug`]; `None` for anything else (URL input).
    pub fn parse_slug(s: &str) -> Option<WikiKey> {
        let rest = s.strip_prefix('r')?;
        let mut parts = rest.split('-');
        let run = parts.next()?;
        if run.len() != 32
            || !run
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return None;
        }
        let digits = |t: &str| -> Option<i64> {
            if t.is_empty() || !t.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            t.parse().ok()
        };
        let cluster_id = digits(parts.next()?.strip_prefix('c')?)?;
        let split_part = match parts.next() {
            None => None,
            Some(p) => Some(digits(p.strip_prefix('s')?)?),
        };
        if parts.next().is_some() {
            return None;
        }
        Some(WikiKey {
            run_id: Uuid::parse_str(run).ok()?,
            cluster_id,
            split_part,
        })
    }
}

/// The synthesis query a wiki article is generated from (also its title source).
/// Label and description are separated by a newline, not `". "`, because
/// labels contain periods ("U.S.", "e.g.").
pub fn article_query(label: &str, description: &str) -> String {
    if description.trim().is_empty() {
        label.to_string()
    } else {
        format!("{label}\n\n{description}")
    }
}

/// The article title: the label line of [`article_query`].
pub fn title_from_query(q: &str) -> &str {
    q.split_once('\n').map(|(t, _)| t).unwrap_or(q)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wiki_key_from_properties_with_and_without_split() {
        let p = serde_json::json!({"source":"cluster_run","cluster_id":197,"split_part":1,
            "cluster_run_id":"16138781-156b-4e12-9b1d-27f6ae8f9e8b"});
        let k = WikiKey::from_properties(&p).unwrap();
        assert_eq!(k.as_slug(), "r16138781156b4e129b1d27f6ae8f9e8b-c197-s1");
        let p2 = serde_json::json!({"cluster_id":135,"cluster_run_id":"16138781-156b-4e12-9b1d-27f6ae8f9e8b"});
        assert_eq!(
            WikiKey::from_properties(&p2).unwrap().as_slug(),
            "r16138781156b4e129b1d27f6ae8f9e8b-c135"
        );
    }

    #[test]
    fn wiki_key_ignores_theme_uuid() {
        // Two projections of the same cluster (different theme ids, same properties) share a key.
        let p = serde_json::json!({"cluster_id":75,"cluster_run_id":"16138781-156b-4e12-9b1d-27f6ae8f9e8b","split_of":"38e3f1c7-116b-4e67-966c-23fe732d7d27"});
        assert_eq!(
            WikiKey::from_properties(&p).unwrap().as_slug(),
            "r16138781156b4e129b1d27f6ae8f9e8b-c75"
        );
    }

    #[test]
    fn uuidv7_runs_minted_close_together_get_distinct_keys() {
        // Same 48-bit timestamp prefix, different random tail.
        let a = serde_json::json!({"cluster_id":1,"cluster_run_id":"0192a1b2-c3d4-7000-8000-000000000001"});
        let b = serde_json::json!({"cluster_id":1,"cluster_run_id":"0192a1b2-c3d4-7000-8000-000000000002"});
        assert_ne!(
            WikiKey::from_properties(&a).unwrap().as_slug(),
            WikiKey::from_properties(&b).unwrap().as_slug()
        );
    }

    #[test]
    fn wiki_key_refuses_incomplete_properties() {
        assert!(WikiKey::from_properties(&serde_json::json!({"cluster_id":1})).is_none());
        assert!(WikiKey::from_properties(
            &serde_json::json!({"cluster_run_id":"not-a-uuid","cluster_id":1})
        )
        .is_none());
        assert!(WikiKey::from_properties(
            &serde_json::json!({"cluster_run_id":"16138781-156b-4e12-9b1d-27f6ae8f9e8b","cluster_id":"7"})
        )
        .is_none());
        assert!(WikiKey::from_properties(&serde_json::json!(null)).is_none());
    }

    #[test]
    fn wiki_key_refuses_a_malformed_or_negative_split_part_or_cluster_id() {
        // A present-but-malformed split_part must fail closed like cluster_id does,
        // never fall back to the unsplit parent's key (which would merge histories).
        let run = "16138781-156b-4e12-9b1d-27f6ae8f9e8b";
        for bad in [
            serde_json::json!({"cluster_run_id":run,"cluster_id":1,"split_part":"1"}),
            serde_json::json!({"cluster_run_id":run,"cluster_id":1,"split_part":1.0}),
            serde_json::json!({"cluster_run_id":run,"cluster_id":1,"split_part":1.5}),
            serde_json::json!({"cluster_run_id":run,"cluster_id":1,"split_part":{"n":1}}),
            serde_json::json!({"cluster_run_id":run,"cluster_id":1,"split_part":-1}),
            serde_json::json!({"cluster_run_id":run,"cluster_id":-1}),
            serde_json::json!({"cluster_run_id":run,"cluster_id":1.0}),
        ] {
            assert!(WikiKey::from_properties(&bad).is_none(), "{bad}");
        }
        // An explicit null split_part means "not split", same as the field being absent.
        let null = serde_json::json!({"cluster_run_id":run,"cluster_id":1,"split_part":null});
        let absent = serde_json::json!({"cluster_run_id":run,"cluster_id":1});
        assert_eq!(
            WikiKey::from_properties(&null).unwrap().as_slug(),
            WikiKey::from_properties(&absent).unwrap().as_slug()
        );
    }

    #[test]
    fn every_accepted_key_round_trips_through_its_slug() {
        let run = "16138781-156b-4e12-9b1d-27f6ae8f9e8b";
        for p in [
            serde_json::json!({"cluster_run_id":run,"cluster_id":0}),
            serde_json::json!({"cluster_run_id":run,"cluster_id":0,"split_part":0}),
            serde_json::json!({"cluster_run_id":run,"cluster_id":7,"split_part":0}),
            serde_json::json!({"cluster_run_id":run,"cluster_id":i64::MAX}),
            serde_json::json!({"cluster_run_id":run,"cluster_id":10,"split_part":i64::MAX}),
            serde_json::json!({"cluster_run_id":"0192a1b2-c3d4-7000-8000-000000000001","cluster_id":197}),
        ] {
            let k = WikiKey::from_properties(&p).unwrap_or_else(|| panic!("refused {p}"));
            assert_eq!(WikiKey::parse_slug(&k.as_slug()), Some(k), "{p}");
        }
    }

    #[test]
    fn slug_parse_round_trips_and_rejects_garbage() {
        let p = serde_json::json!({"cluster_id":197,"split_part":1,"cluster_run_id":"16138781-156b-4e12-9b1d-27f6ae8f9e8b"});
        let k = WikiKey::from_properties(&p).unwrap();
        assert_eq!(WikiKey::parse_slug(&k.as_slug()), Some(k));
        let run = "16138781156b4e129b1d27f6ae8f9e8b";
        for bad in [
            String::new(),
            format!("r{}-c1", &run[..31]),
            format!("r{run}-c"),
            format!("r{run}-c1-s"),
            format!("x{run}-c1"),
            format!("r{run}-c1-s1-x"),
            format!("r{}g-c1", &run[..31]),
            format!("r{}-c1", run.to_uppercase()),
            format!("r{run}-c-1"),
        ] {
            assert!(WikiKey::parse_slug(&bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn article_query_and_title() {
        assert_eq!(
            article_query("Friction", "How friction works."),
            "Friction\n\nHow friction works."
        );
        assert_eq!(article_query("Friction", "  "), "Friction");
        assert_eq!(
            title_from_query("Friction\n\nHow friction works."),
            "Friction"
        );
        assert_eq!(
            title_from_query("U.S. building codes. Dimensions"),
            "U.S. building codes. Dimensions"
        );
        assert_eq!(
            title_from_query(&article_query("U.S. codes", "Scope.")),
            "U.S. codes"
        );
    }
}
