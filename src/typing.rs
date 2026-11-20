//! wtype argv splitting, ported from the sibling `host-gui` skill (the
//! Python agent-display imports it from there at runtime).
//!
//! wtype 0.4 puts the Nth distinct character of an invocation on evdev
//! keycode N. Chromium keys only ASCII letters/digits by keysym; anything
//! else keeps the physical key's behaviour: on 14 (Backspace) it deletes the
//! previous character, on 29 (Ctrl) it is dropped, and on 28 (Enter) a
//! neighbour went missing. 1 and 15 (Escape, Tab) are avoided too. So other
//! characters may only use printable keycodes.

use std::collections::HashSet;

/// wtype 0.5 swaps keymaps every 48 distinct characters; stay below that.
pub const MAX_CODE: usize = 47;

pub fn printable_code(code: usize) -> bool {
    (2..14).contains(&code) || (16..28).contains(&code) || (30..42).contains(&code) || (43..54).contains(&code)
}

pub fn any_code(ch: char) -> bool {
    ch.is_ascii_alphanumeric()
}

/// Split text into (padded, run) pairs, each one wtype invocation.
pub fn type_runs(text: &str) -> Vec<(bool, String)> {
    let mut runs: Vec<(bool, String, HashSet<char>)> = Vec::new();
    for ch in text.chars() {
        if let Some((padded, chars, seen)) = runs.last_mut() {
            let code = seen.len() + 1 + *padded as usize;
            if seen.contains(&ch) || (code <= MAX_CODE && (any_code(ch) || printable_code(code))) {
                chars.push(ch);
                seen.insert(ch);
                continue;
            }
        }
        runs.push((!any_code(ch), ch.to_string(), HashSet::from([ch])));
    }
    runs.into_iter().map(|(p, s, _)| (p, s)).collect()
}

/// One wtype argv per run; together they type text exactly. The pause is
/// AFTER wtype installs its virtual keyboard/keymap, not before launch.
pub fn type_args(text: &str) -> Vec<Vec<String>> {
    type_runs(text)
        .into_iter()
        .map(|(padded, run)| {
            let mut argv: Vec<String> = vec!["wtype".into(), "-s".into(), "400".into()];
            if padded {
                // Occupies keycode 1 when a run starts with a character that must avoid it.
                argv.extend(["-k".into(), "Shift_L".into()]);
            }
            argv.extend(["--".into(), run]);
            argv
        })
        .collect()
}

pub fn key_args(key: &str, mods: &[String]) -> Vec<String> {
    let mut argv: Vec<String> = vec!["wtype".into(), "-s".into(), "400".into()];
    for m in mods {
        argv.extend(["-M".into(), m.clone()]);
    }
    argv.extend(["-k".into(), key.into()]);
    for m in mods.iter().rev() {
        argv.extend(["-m".into(), m.clone()]);
    }
    argv
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn literal_text_and_keymap_delay() {
        assert_eq!(type_args("x@ads_daily 日本語"), vec![v(&["wtype", "-s", "400", "--", "x@ads_daily 日本語"])]);
        assert_eq!(type_args("-s 1; $(x) @日"), vec![v(&["wtype", "-s", "400", "-k", "Shift_L", "--", "-s 1; $(x) @日"])]);
        assert!(type_args("").is_empty());
    }

    #[test]
    fn reported_drops_are_split_before_keycode_14() {
        assert_eq!(
            type_args("agent input test (not sent) 日本語"),
            vec![v(&["wtype", "-s", "400", "--", "agent input test (not sent) "]), v(&["wtype", "-s", "400", "-k", "Shift_L", "--", "日本語"])]
        );
        assert_eq!(
            type_args("x@ads_daily 日本語 -é"),
            vec![v(&["wtype", "-s", "400", "--", "x@ads_daily 日本語 "]), v(&["wtype", "-s", "400", "-k", "Shift_L", "--", "-é"])]
        );
    }

    #[test]
    fn runs_keep_text_on_printable_keycodes() {
        let pool: Vec<char> = "abcXYZ019 -_.,;:!?@#()[]{}<>/\\'\"\n\téñüÅŒß—«»½≥→✓日本語のテキスト漢字中文输入カタ🙂".chars().collect();
        let mut seed = 7u64;
        let mut rnd = |n: usize| {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (seed >> 33) as usize % n
        };
        let mut texts: Vec<String> = (0..300).map(|_| {
            let len = 1 + rnd(299);
            (0..len).map(|_| pool[rnd(pool.len())]).collect()
        }).collect();
        texts.push("a".repeat(3000));
        texts.push((32u8..127).map(|b| b as char).collect());
        for text in texts {
            let runs = type_runs(&text);
            assert_eq!(runs.iter().map(|(_, r)| r.as_str()).collect::<String>(), text);
            for (padded, run) in runs {
                // Model wtype 0.4: Nth distinct keysym of the call -> keycode N.
                let mut codes: Vec<(char, usize)> = Vec::new();
                for ch in run.chars() {
                    if !codes.iter().any(|(c, _)| *c == ch) {
                        codes.push((ch, codes.len() + 1 + padded as usize));
                    }
                }
                for (ch, code) in codes {
                    assert!(code <= MAX_CODE);
                    if !any_code(ch) {
                        assert!(printable_code(code), "{run:?} {ch:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn modifiers_released_in_reverse_order() {
        assert_eq!(
            key_args("a", &v(&["ctrl", "shift"])),
            v(&["wtype", "-s", "400", "-M", "ctrl", "-M", "shift", "-k", "a", "-m", "shift", "-m", "ctrl"])
        );
    }
}
