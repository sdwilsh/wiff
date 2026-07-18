//! Parsing the base-ruleset grammar a session stores to find the base of its
//! reviewed range.
//!
//! A ruleset is a comma-separated list of rules tried left to right until one
//! resolves. This module owns only the grammar: it turns the stored text into an
//! ordered [`Ruleset`] of [`Rule`]s. Resolving a rule to a concrete revision
//! needs the repository and belongs to each scm's source adapter, which this
//! module deliberately leaves out.

use serde::{Deserialize, Serialize};

use crate::identity::ScmType;
use crate::record::RevisionId;

/// A parsed base ruleset: the ordered rules tried left to right until one
/// resolves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ruleset {
    /// The rules, tried in order. A ruleset from [`parse_ruleset`] holds at
    /// least one, though the field itself does not enforce that.
    pub rules: Vec<Rule>,
}

impl Ruleset {
    /// A ruleset of a single unguarded rule.
    fn single(op: RuleOp) -> Self {
        Self {
            rules: vec![Rule { scm: None, op }],
        }
    }

    /// The ruleset that pins a review at `revision`, resolving to the same commit
    /// every refresh.
    pub fn pinned(revision: &RevisionId) -> Self {
        Self::single(RuleOp::Ref(Reference::Literal(revision.to_string())))
    }

    /// The ruleset that reviews the whole history back to the root.
    pub fn empty() -> Self {
        Self::single(RuleOp::Empty)
    }
}

impl std::fmt::Display for Ruleset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (index, rule) in self.rules.iter().enumerate() {
            if index > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{rule}")?;
        }
        Ok(())
    }
}

impl std::fmt::Display for Rule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(scm) = self.scm {
            write!(f, "{scm}:")?;
        }
        write!(f, "{}", self.op)
    }
}

impl std::fmt::Display for RuleOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RuleOp::Ref(reference) => write!(f, "ref({reference})"),
            RuleOp::Parent(reference) => write!(f, "parent({reference})"),
            RuleOp::MergeBase(reference) => write!(f, "merge-base({reference})"),
            RuleOp::Empty => f.write_str("empty"),
            RuleOp::Native { scm, expr } => write!(f, "{scm}({expr})"),
        }
    }
}

impl std::fmt::Display for Reference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Reference::Literal(name) => write!(f, "name({name})"),
            Reference::Trunk => f.write_str("trunk"),
            Reference::Upstream => f.write_str("upstream"),
            Reference::Tip => f.write_str("@"),
        }
    }
}

/// The base-ruleset grammar text, as the user or config wrote it, parsed with
/// [`parse_ruleset`] when a review's base is resolved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BaseRuleset(pub String);

impl BaseRuleset {
    /// Build a ruleset from arbitrary grammar text.
    pub fn new(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    /// The ruleset that pins a review at `revision`.
    pub fn pinned(revision: &RevisionId) -> Self {
        Self(Ruleset::pinned(revision).to_string())
    }

    /// The ruleset that reviews the whole history back to the root.
    pub fn empty() -> Self {
        Self(Ruleset::empty().to_string())
    }

    /// Returns the ruleset grammar as written.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for BaseRuleset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// One rule of a base ruleset, tried in turn until one resolves a base commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    /// The scm this rule is gated to; `None` applies to any scm.
    pub scm: Option<ScmType>,
    /// What the rule computes.
    pub op: RuleOp,
}

/// How a rule resolves its base commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleOp {
    /// Use the referenced commit directly as the base.
    Ref(Reference),
    /// Use the first parent of the referenced commit.
    Parent(Reference),
    /// Use the common ancestor of the referenced commit and the tip.
    MergeBase(Reference),
    /// Use the empty tree, reviewing the whole history to the root.
    Empty,
    /// Pass an expression verbatim to a named scm's own resolver. The function
    /// name is itself the scm selector: `git(<expr>)` is considered only when the
    /// session's scm is git.
    Native {
        /// The scm whose resolver the expression is handed to.
        scm: ScmType,
        /// The expression, with only its surrounding whitespace stripped.
        expr: String,
    },
}

/// The reference an operator resolves: a literal ref named with `name(...)`, or
/// one of the bare computed symbols.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reference {
    /// A ref named literally, written `name(<ref>)`, kept apart from the bare
    /// symbols so a branch called `trunk` never collides with the `trunk` symbol.
    Literal(String),
    /// The repository's default branch.
    Trunk,
    /// This branch's configured tracking tip.
    Upstream,
    /// The tip under review, written `@`.
    Tip,
}

/// A base ruleset that could not be parsed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid base ruleset at position {position}: {message}")]
pub struct ParseError {
    /// The human-readable reason parsing failed.
    pub message: String,
    /// The character offset into the input at which the error was detected: the
    /// start of an unrecognized token, or where a structural element ran out.
    pub position: usize,
}

/// Parse `input` into a [`Ruleset`].
pub fn parse_ruleset(input: &str) -> Result<Ruleset, ParseError> {
    let mut parser = Parser {
        chars: input.chars().collect(),
        pos: 0,
    };
    let rules = parser.ruleset()?;
    Ok(Ruleset { rules })
}

/// The scm tokens the grammar accepts, as a human list ("git, jj, hg, or sl"),
/// built from [`ScmType::ALL`] so the accepted set is named in one place.
fn accepted_scms() -> String {
    let tokens: Vec<String> = ScmType::ALL.iter().map(ScmType::to_string).collect();
    match tokens.split_last() {
        Some((last, rest)) => format!("{}, or {last}", rest.join(", ")),
        None => String::new(),
    }
}

/// A cursor over the input's characters. Character offsets, not byte offsets,
/// keep the reported position meaningful for multi-byte input.
struct Parser {
    chars: Vec<char>,
    pos: usize,
}

impl Parser {
    fn ruleset(&mut self) -> Result<Vec<Rule>, ParseError> {
        let mut rules = Vec::new();
        self.skip_ws();
        if self.at_end() {
            return Err(self.err("a base ruleset must contain at least one rule"));
        }
        loop {
            rules.push(self.rule()?);
            self.skip_ws();
            if self.at_end() {
                return Ok(rules);
            }
            if self.peek() != Some(',') {
                return Err(self.err("expected ',' between rules"));
            }
            self.bump();
            self.skip_ws();
            if self.at_end() {
                return Err(self.err("a base ruleset must not end with a comma"));
            }
        }
    }

    fn rule(&mut self) -> Result<Rule, ParseError> {
        self.skip_ws();
        let word_start = self.pos;
        let word = self.word()?;
        self.skip_ws();
        // A colon after the first word makes it an scm gate; the operator
        // follows. Otherwise the word is the operator itself.
        if self.peek() == Some(':') {
            let scm = ScmType::from_token(&word).ok_or_else(|| {
                self.err_at(
                    word_start,
                    &format!("unknown scm prefix '{word}'; use {}", accepted_scms()),
                )
            })?;
            self.bump();
            self.skip_ws();
            let op_start = self.pos;
            let op_word = self.word()?;
            let op = self.operator(&op_word, op_start)?;
            // A native operator names its own scm, so a gate in front of it is at
            // best redundant and at worst contradictory (hg:git(...)); reject it.
            if matches!(op, RuleOp::Native { .. }) {
                return Err(self.err_at(
                    word_start,
                    "an scm-native operator already names its scm; drop the redundant prefix",
                ));
            }
            Ok(Rule { scm: Some(scm), op })
        } else {
            let op = self.operator(&word, word_start)?;
            Ok(Rule { scm: None, op })
        }
    }

    fn operator(&mut self, name: &str, start: usize) -> Result<RuleOp, ParseError> {
        match name {
            "empty" => Ok(RuleOp::Empty),
            "ref" => Ok(RuleOp::Ref(self.paren_reference()?)),
            "parent" => Ok(RuleOp::Parent(self.paren_reference()?)),
            "merge-base" => Ok(RuleOp::MergeBase(self.paren_reference()?)),
            _ => match ScmType::from_token(name) {
                Some(scm) => Ok(RuleOp::Native {
                    scm,
                    expr: self.verbatim_body()?,
                }),
                None => Err(self.err_at(
                    start,
                    &format!(
                        "unknown base operator '{name}'; use ref, parent, merge-base, empty, or \
                         an scm-native expression ({})",
                        accepted_scms()
                    ),
                )),
            },
        }
    }

    /// Parse `(` reference `)`.
    fn paren_reference(&mut self) -> Result<Reference, ParseError> {
        self.expect('(')?;
        let reference = self.reference()?;
        self.expect(')')?;
        Ok(reference)
    }

    fn reference(&mut self) -> Result<Reference, ParseError> {
        self.skip_ws();
        match self.peek() {
            Some('@') => {
                self.bump();
                Ok(Reference::Tip)
            }
            Some(_) => {
                let start = self.pos;
                let word = self.word()?;
                match word.as_str() {
                    "trunk" => Ok(Reference::Trunk),
                    "upstream" => Ok(Reference::Upstream),
                    "name" => Ok(Reference::Literal(self.verbatim_body()?)),
                    other => Err(self.err_at(
                        start,
                        &format!(
                            "unknown reference '{other}'; use trunk, upstream, @, or name(<ref>)"
                        ),
                    )),
                }
            }
            None => Err(self.err("expected a reference")),
        }
    }

    /// Read the balanced-parenthesis body shared by `name(...)` and the scm-native
    /// operators. Parentheses are matched purely by depth, with no interpretation
    /// of the body's own quoting; only the surrounding whitespace is stripped, and
    /// the interior is kept as written. A body with unbalanced parentheses
    /// therefore cannot be expressed.
    fn verbatim_body(&mut self) -> Result<String, ParseError> {
        self.expect('(')?;
        let start = self.pos;
        let mut depth = 1usize;
        while let Some(c) = self.peek() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        let body: String = self.chars[start..self.pos].iter().collect();
                        self.bump();
                        let body = body.trim().to_string();
                        if body.is_empty() {
                            return Err(
                                self.err_at(start, "expected a value inside the parentheses")
                            );
                        }
                        return Ok(body);
                    }
                }
                _ => {}
            }
            self.bump();
        }
        Err(self.err("unterminated parentheses; missing ')'"))
    }

    /// Read a run of identifier characters (`ref`, `merge-base`, `upstream`, an
    /// scm name).
    fn word(&mut self) -> Result<String, ParseError> {
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                self.bump();
            } else {
                break;
            }
        }
        if self.pos == start {
            return Err(self.err("expected an operator or scm name"));
        }
        Ok(self.chars[start..self.pos].iter().collect())
    }

    fn expect(&mut self, want: char) -> Result<(), ParseError> {
        self.skip_ws();
        if self.peek() == Some(want) {
            self.bump();
            Ok(())
        } else {
            Err(self.err(&format!("expected '{want}'")))
        }
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(c) if c.is_whitespace()) {
            self.bump();
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn bump(&mut self) {
        self.pos += 1;
    }

    fn at_end(&self) -> bool {
        self.pos >= self.chars.len()
    }

    /// An error at the current position, where parsing stopped.
    fn err(&self, message: &str) -> ParseError {
        self.err_at(self.pos, message)
    }

    /// An error at `position`, for pointing back at the start of a token already
    /// consumed rather than at where its parse gave up.
    fn err_at(&self, position: usize, message: &str) -> ParseError {
        ParseError {
            message: message.to_string(),
            position,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_built_in_default_ruleset_parses_to_two_merge_base_rules() {
        wince::assert_eq!(
            parse_ruleset("merge-base(upstream), merge-base(trunk)"),
            Ok(Ruleset {
                rules: vec![
                    Rule {
                        scm: None,
                        op: RuleOp::MergeBase(Reference::Upstream),
                    },
                    Rule {
                        scm: None,
                        op: RuleOp::MergeBase(Reference::Trunk),
                    },
                ],
            })
        );
    }

    #[test]
    fn every_operator_and_reference_parses() {
        wince::assert_eq!(
            parse_ruleset(
                "git:ref(name(origin/main)), parent(@), merge-base(trunk), empty, jj(parents(@))"
            ),
            Ok(Ruleset {
                rules: vec![
                    Rule {
                        scm: Some(ScmType::Git),
                        op: RuleOp::Ref(Reference::Literal("origin/main".to_string())),
                    },
                    Rule {
                        scm: None,
                        op: RuleOp::Parent(Reference::Tip),
                    },
                    Rule {
                        scm: None,
                        op: RuleOp::MergeBase(Reference::Trunk),
                    },
                    Rule {
                        scm: None,
                        op: RuleOp::Empty,
                    },
                    Rule {
                        scm: None,
                        op: RuleOp::Native {
                            scm: ScmType::Jujutsu,
                            expr: "parents(@)".to_string(),
                        },
                    },
                ],
            })
        );
    }

    #[test]
    fn a_pinned_base_is_a_ref_of_a_named_sha() {
        wince::assert_eq!(
            parse_ruleset("ref(name(9c1b453))"),
            Ok(Ruleset {
                rules: vec![Rule {
                    scm: None,
                    op: RuleOp::Ref(Reference::Literal("9c1b453".to_string())),
                }],
            })
        );
    }

    #[test]
    fn a_native_expression_and_a_named_ref_keep_their_nested_parens() {
        // Both the scm-native body and a name(...) literal capture verbatim by
        // paren depth, so an interior '(' or ')' pair survives intact.
        wince::assert_eq!(
            parse_ruleset("git(rev-list(a, b)), ref(name(refs/heads/(wip)))"),
            Ok(Ruleset {
                rules: vec![
                    Rule {
                        scm: None,
                        op: RuleOp::Native {
                            scm: ScmType::Git,
                            expr: "rev-list(a, b)".to_string(),
                        },
                    },
                    Rule {
                        scm: None,
                        op: RuleOp::Ref(Reference::Literal("refs/heads/(wip)".to_string())),
                    },
                ],
            })
        );
    }

    #[test]
    fn an_apostrophe_in_a_native_expression_is_passed_through_untouched() {
        // The body is verbatim: a lone quote is an ordinary character, not the
        // start of an opaque span, so the first unmatched ')' closes the body.
        wince::assert_eq!(
            parse_ruleset("git(author('wez))"),
            Ok(Ruleset {
                rules: vec![Rule {
                    scm: None,
                    op: RuleOp::Native {
                        scm: ScmType::Git,
                        expr: "author('wez)".to_string(),
                    },
                }],
            })
        );
    }

    #[test]
    fn surrounding_and_interior_whitespace_is_tolerated() {
        wince::assert_eq!(
            parse_ruleset("  ref( @ ) ,  empty  "),
            Ok(Ruleset {
                rules: vec![
                    Rule {
                        scm: None,
                        op: RuleOp::Ref(Reference::Tip),
                    },
                    Rule {
                        scm: None,
                        op: RuleOp::Empty,
                    },
                ],
            })
        );
    }

    #[test]
    fn whitespace_around_the_scm_gate_colon_is_tolerated() {
        wince::assert_eq!(
            parse_ruleset("git : ref(@)"),
            Ok(Ruleset {
                rules: vec![Rule {
                    scm: Some(ScmType::Git),
                    op: RuleOp::Ref(Reference::Tip),
                }],
            })
        );
    }

    #[test]
    fn an_empty_ruleset_is_rejected() {
        wince::assert_eq!(
            parse_ruleset("   "),
            Err(ParseError {
                message: "a base ruleset must contain at least one rule".to_string(),
                position: 3,
            })
        );
    }

    #[test]
    fn a_trailing_comma_is_rejected() {
        wince::assert_eq!(
            parse_ruleset("empty,"),
            Err(ParseError {
                message: "a base ruleset must not end with a comma".to_string(),
                position: 6,
            })
        );
    }

    #[test]
    fn an_unknown_operator_is_rejected_at_its_start() {
        wince::assert_eq!(
            parse_ruleset("fork-point(trunk)"),
            Err(ParseError {
                message: "unknown base operator 'fork-point'; use ref, parent, merge-base, \
                          empty, or an scm-native expression (git, jj, hg, or sl)"
                    .to_string(),
                position: 0,
            })
        );
    }

    #[test]
    fn an_unknown_reference_symbol_is_rejected_at_its_start() {
        wince::assert_eq!(
            parse_ruleset("merge-base(main)"),
            Err(ParseError {
                message: "unknown reference 'main'; use trunk, upstream, @, or name(<ref>)"
                    .to_string(),
                position: 11,
            })
        );
    }

    #[test]
    fn an_unknown_scm_prefix_is_rejected_at_its_start() {
        wince::assert_eq!(
            parse_ruleset("foo:ref(@)"),
            Err(ParseError {
                message: "unknown scm prefix 'foo'; use git, jj, hg, or sl".to_string(),
                position: 0,
            })
        );
    }

    #[test]
    fn a_prefix_on_a_native_operator_is_rejected_at_its_start() {
        wince::assert_eq!(
            parse_ruleset("hg:git(x)"),
            Err(ParseError {
                message: "an scm-native operator already names its scm; drop the redundant prefix"
                    .to_string(),
                position: 0,
            })
        );
    }

    #[test]
    fn an_empty_named_ref_is_rejected() {
        wince::assert_eq!(
            parse_ruleset("ref(name())"),
            Err(ParseError {
                message: "expected a value inside the parentheses".to_string(),
                position: 9,
            })
        );
    }

    #[test]
    fn rendering_a_ruleset_is_the_inverse_of_parsing_it() {
        let text =
            "git:ref(name(origin/main)), parent(@), merge-base(trunk), empty, jj(parents(@))";
        let ruleset = parse_ruleset(text).expect("parse");
        wince::assert_eq!(ruleset.to_string(), text.to_string());
    }

    #[test]
    fn the_structured_constructors_render_to_parseable_grammar() {
        let revision = RevisionId("9c1b453".to_string());
        wince::assert_eq!(
            Ruleset::pinned(&revision).to_string(),
            "ref(name(9c1b453))".to_string()
        );
        wince::assert_eq!(Ruleset::empty().to_string(), "empty".to_string());
    }

    #[test]
    fn an_unterminated_native_expression_is_rejected() {
        wince::assert_eq!(
            parse_ruleset("git(parents(@)"),
            Err(ParseError {
                message: "unterminated parentheses; missing ')'".to_string(),
                position: 14,
            })
        );
    }
}
