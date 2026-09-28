//! Anchor vocabulary: which `call:`/`macro:`/`member:` tokens carry domain
//! meaning and which are standard-library plumbing.
//!
//! Window anchors exist to answer "does this repetition DO something specific?".
//! `iter`, `collect`, `unwrap`, `to_string` and friends answer "no": every Rust
//! or TypeScript file on earth calls them, so windows anchored only on them are
//! language boilerplate, not a shared-tool lead. Filtering them out of the
//! anchor count, the cluster label and the family signature keeps the ranking
//! on repetition a shared tool could actually absorb.

/// Standard-library / built-in call, macro and member names, plus the
/// primitive and container type names that reach the normalizer as `member:`
/// tokens through paths like `parse::<usize>`. Sorted so the lookup is a
/// binary search; lowercased to match the normalizer's tokens.
const GENERIC_ANCHORS: &[&str] = &[
    "abs",
    "add",
    "all",
    "and_then",
    "any",
    "append",
    "args",
    "as_bytes",
    "as_deref",
    "as_mut",
    "as_path",
    "as_ref",
    "as_slice",
    "as_str",
    "assert",
    "assert_eq",
    "assert_ne",
    "await",
    "binary_search",
    "bool",
    "borrow",
    "borrow_mut",
    "boxed",
    "btreemap",
    "btreeset",
    "catch",
    "ceil",
    "char",
    "char_indices",
    "chars",
    "checked_add",
    "checked_sub",
    "chunks",
    "clear",
    "clone",
    "cloned",
    "cmp",
    "collect",
    "concat",
    "contains",
    "contains_key",
    "copied",
    "count",
    "dbg",
    "debug",
    "dedup",
    "default",
    "deref",
    "display",
    "drain",
    "drop",
    "ends_with",
    "entries",
    "entry",
    "enumerate",
    "eq",
    "err",
    "error",
    "expect",
    "extend",
    "f32",
    "f64",
    "filter",
    "filter_map",
    "find",
    "first",
    "first_mut",
    "flat_map",
    "flatten",
    "floor",
    "fmt",
    "fold",
    "for_each",
    "foreach",
    "format",
    "format_args",
    "from",
    "from_iter",
    "from_str",
    "get",
    "get_mut",
    "hash",
    "hashmap",
    "hashset",
    "i128",
    "i16",
    "i32",
    "i64",
    "i8",
    "includes",
    "indexof",
    "info",
    "insert",
    "into",
    "into_iter",
    "is_empty",
    "is_err",
    "is_none",
    "is_ok",
    "is_some",
    "isize",
    "iter",
    "iter_mut",
    "join",
    "keys",
    "keys_mut",
    "last",
    "last_mut",
    "len",
    "lines",
    "lock",
    "log",
    "map",
    "map_err",
    "map_or",
    "matches",
    "max",
    "max_by",
    "max_by_key",
    "min",
    "min_by",
    "min_by_key",
    "ne",
    "new",
    "next",
    "none",
    "nth",
    "ok",
    "ok_or",
    "ok_or_else",
    "option",
    "or_default",
    "or_else",
    "or_insert",
    "or_insert_with",
    "panic",
    "parse",
    "partial_cmp",
    "peek",
    "pop",
    "position",
    "print",
    "println",
    "push",
    "push_str",
    "range",
    "read",
    "read_to_string",
    "recv",
    "reduce",
    "remove",
    "repeat",
    "replace",
    "resize",
    "result",
    "retain",
    "rev",
    "reverse",
    "round",
    "rsplit",
    "rsplitn",
    "saturating_add",
    "saturating_sub",
    "send",
    "shift",
    "skip",
    "some",
    "sort",
    "sort_by",
    "sort_by_key",
    "sort_unstable",
    "sort_unstable_by",
    "splice",
    "split",
    "split_once",
    "split_whitespace",
    "splitn",
    "starts_with",
    "str",
    "string",
    "stringify",
    "strip_prefix",
    "strip_suffix",
    "sum",
    "swap",
    "take",
    "then",
    "then_with",
    "to_lowercase",
    "to_owned",
    "to_path_buf",
    "to_str",
    "to_string",
    "to_uppercase",
    "to_vec",
    "tostring",
    "trace",
    "trim",
    "trim_end",
    "trim_start",
    "truncate",
    "try_from",
    "try_into",
    "u128",
    "u16",
    "u32",
    "u64",
    "u8",
    "unshift",
    "unwrap",
    "unwrap_or",
    "unwrap_or_default",
    "unwrap_or_else",
    "usize",
    "values",
    "values_mut",
    "vec",
    "vec_deque",
    "warn",
    "windows",
    "with_capacity",
    "write",
    "writeln",
    "zip",
];

/// Whether an anchor name is standard-library plumbing rather than domain code.
#[must_use]
pub(crate) fn is_generic_anchor(name: &str) -> bool {
    GENERIC_ANCHORS.binary_search(&name).is_ok()
}

/// The domain-meaningful name inside a normalized anchor token, or `None` when
/// the token is not an anchor or names standard-library plumbing.
#[must_use]
pub(crate) fn domain_anchor(token: &str) -> Option<&str> {
    let name = token
        .strip_prefix("call:")
        .or_else(|| token.strip_prefix("macro:"))
        .or_else(|| token.strip_prefix("member:"))?;
    (!is_generic_anchor(name)).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vocabulary_is_sorted_and_deduplicated() {
        let mut sorted = GENERIC_ANCHORS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted, GENERIC_ANCHORS.to_vec());
    }

    #[test]
    fn stdlib_names_are_never_domain_anchors() {
        for token in [
            "call:collect",
            "member:display",
            "call:read_to_string",
            "macro:vec",
        ] {
            assert_eq!(
                domain_anchor(token),
                None,
                "{token} must not anchor a cluster"
            );
        }
    }

    #[test]
    fn domain_names_survive() {
        assert_eq!(domain_anchor("call:issue_token"), Some("issue_token"));
        assert_eq!(domain_anchor("member:checksum"), Some("checksum"));
        assert_eq!(domain_anchor("id"), None);
        assert_eq!(domain_anchor("kw:if"), None);
    }
}
