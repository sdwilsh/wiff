//! Turning a parsed base ruleset into a concrete base commit.
//!
//! The grammar in [`base_ruleset`](crate::base_ruleset) says what a ruleset
//! means; this module runs it against a repository. The rule-walking is the same
//! for every scm (gate each rule by the active scm, try the rules left to right,
//! stop at the first that resolves), so it lives here as a driver over a
//! [`RevisionResolver`] that each scm implements with its own primitives.

use async_trait::async_trait;

use crate::base_ruleset::{Reference, RuleOp, Ruleset};
use crate::error::Result;
use crate::identity::ScmType;
use crate::record::RevisionId;

/// The per-scm primitives the base grammar resolves through. Each primitive
/// answers for one repository: it returns `Ok(None)` when the query names
/// nothing there (an unknown ref, a missing upstream, no common ancestor), which
/// the driver reads as "this rule does not apply, try the next." An `Err` is a
/// genuine failure of the scm itself (it could not be run, it returned garbage)
/// and aborts the whole resolution rather than falling through.
#[async_trait]
pub trait RevisionResolver {
    /// The scm this resolver speaks for, used to gate rules written for a
    /// different one.
    fn scm(&self) -> ScmType;

    /// Resolve a literal ref, written `name(...)` in the grammar, to a commit.
    async fn resolve_ref(&self, name: &str) -> Result<Option<RevisionId>>;

    /// Resolve the repository's default branch.
    async fn trunk(&self) -> Result<Option<RevisionId>>;

    /// Resolve the current branch's configured tracking tip.
    async fn upstream(&self) -> Result<Option<RevisionId>>;

    /// Resolve the first parent of `rev`.
    async fn parent(&self, rev: &RevisionId) -> Result<Option<RevisionId>>;

    /// Resolve the common ancestor of `rev` and the tip under review.
    async fn merge_base(&self, rev: &RevisionId, tip: &RevisionId) -> Result<Option<RevisionId>>;

    /// Resolve an expression handed verbatim to the scm's own resolver.
    async fn native(&self, expr: &str) -> Result<Option<RevisionId>>;

    /// Resolve the empty tree, the base of a review that reaches back to the
    /// root.
    async fn empty(&self) -> Result<Option<RevisionId>>;
}

/// Resolve `ruleset` to a base commit against `resolver`, given the `tip` the
/// review runs up to. Rules are tried left to right; the first that resolves
/// wins. Returns `Ok(None)` when every rule falls through.
pub async fn resolve_base(
    ruleset: &Ruleset,
    tip: &RevisionId,
    resolver: &dyn RevisionResolver,
) -> Result<Option<RevisionId>> {
    for rule in &ruleset.rules {
        // A rule gated to another scm never applies here.
        if rule.scm.is_some_and(|scm| scm != resolver.scm()) {
            continue;
        }
        if let Some(base) = resolve_op(&rule.op, tip, resolver).await? {
            return Ok(Some(base));
        }
    }
    Ok(None)
}

/// Resolve one rule's operator to a base commit, or `None` when it does not
/// apply to this repository.
async fn resolve_op(
    op: &RuleOp,
    tip: &RevisionId,
    resolver: &dyn RevisionResolver,
) -> Result<Option<RevisionId>> {
    match op {
        RuleOp::Ref(reference) => resolve_reference(reference, tip, resolver).await,
        RuleOp::Parent(reference) => match resolve_reference(reference, tip, resolver).await? {
            Some(rev) => resolver.parent(&rev).await,
            None => Ok(None),
        },
        RuleOp::MergeBase(reference) => match resolve_reference(reference, tip, resolver).await? {
            Some(rev) => resolver.merge_base(&rev, tip).await,
            None => Ok(None),
        },
        RuleOp::Empty => resolver.empty().await,
        // The function name is the scm selector, so a native op for another scm
        // falls through just as a mismatched gate does.
        RuleOp::Native { scm, expr } => {
            if *scm != resolver.scm() {
                Ok(None)
            } else {
                resolver.native(expr).await
            }
        }
    }
}

/// Resolve a reference to a commit. The tip resolves to itself; the others
/// delegate to the scm.
async fn resolve_reference(
    reference: &Reference,
    tip: &RevisionId,
    resolver: &dyn RevisionResolver,
) -> Result<Option<RevisionId>> {
    match reference {
        Reference::Tip => Ok(Some(tip.clone())),
        Reference::Literal(name) => resolver.resolve_ref(name).await,
        Reference::Trunk => resolver.trunk().await,
        Reference::Upstream => resolver.upstream().await,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::base_ruleset::parse_ruleset;

    /// A resolver backed by fixed answers, so the driver's rule-walking can be
    /// exercised without a real repository. Every primitive looks its query up
    /// in a map and returns `None` when absent, mirroring a real scm's
    /// fall-through. An entry mapped to `Err` stands for a genuine scm failure.
    #[derive(Default)]
    struct FakeResolver {
        scm: ScmTypeForTest,
        refs: HashMap<String, RevisionId>,
        trunk: Option<RevisionId>,
        upstream: Option<RevisionId>,
        parents: HashMap<String, RevisionId>,
        merge_bases: HashMap<(String, String), RevisionId>,
        native: HashMap<String, RevisionId>,
        empty: Option<RevisionId>,
        fail_ref: Option<String>,
    }

    /// The scm a [`FakeResolver`] claims, defaulting to git.
    struct ScmTypeForTest(ScmType);

    impl Default for ScmTypeForTest {
        fn default() -> Self {
            ScmTypeForTest(ScmType::Git)
        }
    }

    fn rev(id: &str) -> RevisionId {
        RevisionId(id.to_string())
    }

    #[async_trait]
    impl RevisionResolver for FakeResolver {
        fn scm(&self) -> ScmType {
            self.scm.0
        }

        async fn resolve_ref(&self, name: &str) -> Result<Option<RevisionId>> {
            if self.fail_ref.as_deref() == Some(name) {
                return Err(crate::error::Error::Source(format!(
                    "ref {name} could not be resolved"
                )));
            }
            Ok(self.refs.get(name).cloned())
        }

        async fn trunk(&self) -> Result<Option<RevisionId>> {
            Ok(self.trunk.clone())
        }

        async fn upstream(&self) -> Result<Option<RevisionId>> {
            Ok(self.upstream.clone())
        }

        async fn parent(&self, rev: &RevisionId) -> Result<Option<RevisionId>> {
            Ok(self.parents.get(rev.as_str()).cloned())
        }

        async fn merge_base(
            &self,
            rev: &RevisionId,
            tip: &RevisionId,
        ) -> Result<Option<RevisionId>> {
            Ok(self
                .merge_bases
                .get(&(rev.0.clone(), tip.0.clone()))
                .cloned())
        }

        async fn native(&self, expr: &str) -> Result<Option<RevisionId>> {
            Ok(self.native.get(expr).cloned())
        }

        async fn empty(&self) -> Result<Option<RevisionId>> {
            Ok(self.empty.clone())
        }
    }

    #[tokio::test]
    async fn the_first_resolving_rule_wins_and_later_rules_are_not_tried() {
        let resolver = FakeResolver {
            upstream: Some(rev("upstream-tip")),
            merge_bases: HashMap::from([(
                ("upstream-tip".to_string(), "tip".to_string()),
                rev("ancestor"),
            )]),
            ..Default::default()
        };
        let ruleset = parse_ruleset("merge-base(upstream), merge-base(trunk)").expect("parse");
        let base = resolve_base(&ruleset, &rev("tip"), &resolver)
            .await
            .expect("resolve");
        wince::assert_eq!(base, Some(rev("ancestor")));
    }

    #[tokio::test]
    async fn a_rule_whose_reference_is_absent_falls_through_to_the_next() {
        let resolver = FakeResolver {
            trunk: Some(rev("trunk-tip")),
            merge_bases: HashMap::from([(
                ("trunk-tip".to_string(), "tip".to_string()),
                rev("fork-point"),
            )]),
            ..Default::default()
        };
        // upstream is unset, so the first rule falls through to the trunk rule.
        let ruleset = parse_ruleset("merge-base(upstream), merge-base(trunk)").expect("parse");
        let base = resolve_base(&ruleset, &rev("tip"), &resolver)
            .await
            .expect("resolve");
        wince::assert_eq!(base, Some(rev("fork-point")));
    }

    #[tokio::test]
    async fn every_rule_falling_through_resolves_to_none() {
        let resolver = FakeResolver::default();
        let ruleset = parse_ruleset("merge-base(upstream), merge-base(trunk)").expect("parse");
        let base = resolve_base(&ruleset, &rev("tip"), &resolver)
            .await
            .expect("resolve");
        wince::assert_eq!(base, None);
    }

    #[tokio::test]
    async fn parent_of_the_tip_resolves_through_the_at_symbol() {
        let resolver = FakeResolver {
            parents: HashMap::from([("tip".to_string(), rev("tip-parent"))]),
            ..Default::default()
        };
        let ruleset = parse_ruleset("parent(@)").expect("parse");
        let base = resolve_base(&ruleset, &rev("tip"), &resolver)
            .await
            .expect("resolve");
        wince::assert_eq!(base, Some(rev("tip-parent")));
    }

    #[tokio::test]
    async fn a_named_literal_ref_resolves_directly() {
        let resolver = FakeResolver {
            refs: HashMap::from([("origin/main".to_string(), rev("origin-main-sha"))]),
            ..Default::default()
        };
        let ruleset = parse_ruleset("ref(name(origin/main))").expect("parse");
        let base = resolve_base(&ruleset, &rev("tip"), &resolver)
            .await
            .expect("resolve");
        wince::assert_eq!(base, Some(rev("origin-main-sha")));
    }

    #[tokio::test]
    async fn empty_resolves_to_the_empty_tree() {
        let resolver = FakeResolver {
            empty: Some(rev("empty-tree")),
            ..Default::default()
        };
        let ruleset = parse_ruleset("empty").expect("parse");
        let base = resolve_base(&ruleset, &rev("tip"), &resolver)
            .await
            .expect("resolve");
        wince::assert_eq!(base, Some(rev("empty-tree")));
    }

    #[tokio::test]
    async fn a_native_op_for_another_scm_falls_through_to_the_next_rule() {
        let resolver = FakeResolver {
            refs: HashMap::from([("fallback".to_string(), rev("fallback-sha"))]),
            ..Default::default()
        };
        // Under a git resolver the jj-native op is skipped without a lookup, so
        // resolution falls through to the following ref rule.
        let ruleset = parse_ruleset("jj(main@origin), ref(name(fallback))").expect("parse");
        let base = resolve_base(&ruleset, &rev("tip"), &resolver)
            .await
            .expect("resolve");
        wince::assert_eq!(base, Some(rev("fallback-sha")));
    }

    #[tokio::test]
    async fn a_native_expression_for_the_active_scm_resolves() {
        let resolver = FakeResolver {
            native: HashMap::from([("HEAD~2".to_string(), rev("two-back"))]),
            ..Default::default()
        };
        let ruleset = parse_ruleset("git(HEAD~2)").expect("parse");
        let base = resolve_base(&ruleset, &rev("tip"), &resolver)
            .await
            .expect("resolve");
        wince::assert_eq!(base, Some(rev("two-back")));
    }

    #[tokio::test]
    async fn an_scm_failure_aborts_rather_than_falling_through() {
        let resolver = FakeResolver {
            fail_ref: Some("broken".to_string()),
            refs: HashMap::from([("ok".to_string(), rev("ok-sha"))]),
            ..Default::default()
        };
        // The first rule hits the failing ref; the driver must report that
        // error rather than moving on to the resolvable second rule.
        let ruleset = parse_ruleset("ref(name(broken)), ref(name(ok))").expect("parse");
        let error = resolve_base(&ruleset, &rev("tip"), &resolver)
            .await
            .expect_err("resolve fails");
        wince::assert_eq!(
            error.to_string(),
            "could not capture diff: ref broken could not be resolved".to_string()
        );
    }
}
