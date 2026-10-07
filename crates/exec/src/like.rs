//! `LIKE` pattern matching: `%` matches any run of characters (including
//! none), `_` exactly one character; everything else matches itself,
//! case-sensitively. The match is anchored at both ends.

/// Iterative wildcard matching with single-star backtracking: O(n·m) in the
/// worst case, no recursion.
pub fn like(text: &str, pattern: &str) -> bool {
    let text: Vec<char> = text.chars().collect();
    let pattern: Vec<char> = pattern.chars().collect();
    let (mut t, mut p) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while t < text.len() {
        match pattern.get(p) {
            Some('%') => {
                star = Some((p, t));
                p += 1;
            }
            Some(&c) if c == '_' || Some(&c) == text.get(t) => {
                t += 1;
                p += 1;
            }
            _ => match star {
                Some((star_p, star_t)) => {
                    p = star_p + 1;
                    t = star_t + 1;
                    star = Some((star_p, star_t + 1));
                }
                None => return false,
            },
        }
    }
    pattern.iter().skip(p).all(|&c| c == '%')
}

#[cfg(test)]
mod tests {
    use super::like;

    #[test]
    fn wildcards() {
        let cases = [
            ("abc", "abc", true),
            ("abc", "ab", false),
            ("abc", "a%", true),
            ("abc", "%c", true),
            ("abc", "%b%", true),
            ("abc", "%%%", true),
            ("", "%", true),
            ("", "_", false),
            ("", "", true),
            ("abc", "a_c", true),
            ("abc", "a__c", false),
            ("abc", "___", true),
            ("aXbXc", "a%b%c", true),
            ("abcbc", "a%bc", true),
            ("abcbd", "a%bc", false),
            ("ABC", "abc", false),
            ("é世", "__", true),
            ("é世", "_", false),
            ("100%", "100%", true),
            ("mississippi", "m%iss%ppi", true),
            ("mississippi", "m%iss%pi%x", false),
        ];
        for (text, pattern, expected) in cases {
            assert_eq!(like(text, pattern), expected, "{text:?} LIKE {pattern:?}");
        }
    }
}
