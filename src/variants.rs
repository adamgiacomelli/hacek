//! Works out which letters have variants, by looking at what keyboard layouts can type.

use std::collections::BTreeMap;
use unicode_normalization::UnicodeNormalization;
use xkbcommon::xkb;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Variants(BTreeMap<char, Vec<char>>);

impl Variants {
    /// Variants of `c`, in the case `c` was typed in.
    pub fn get(&self, c: char) -> Vec<char> {
        let Some(lower) = single(c.to_lowercase()) else {
            return vec![];
        };
        let list = self.0.get(&lower).cloned().unwrap_or_default();
        if c == lower {
            return list;
        }
        list.into_iter()
            .filter_map(|v| match v {
                'ß' => Some('ẞ'),
                v => single(v.to_uppercase()),
            })
            .collect()
    }

    pub fn add(&mut self, base: char, variant: char) {
        let list = self.0.entry(base).or_default();
        if base != variant && !list.contains(&variant) && list.len() < 9 {
            list.push(variant);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&char, &Vec<char>)> {
        self.0.iter()
    }

    /// Add every accented letter that any layout group in `keymap` can type.
    ///
    /// Many layouts carry a pile of foreign letters behind AltGr (Slovenian has ß and ł).
    /// Those only count when the layout has no accented letters on its plain keys, as in
    /// Polish or US-intl, or when `altgr` asks for them.
    pub fn add_keymap(&mut self, keymap: &xkb::Keymap, altgr: bool) {
        for group in 0..keymap.num_layouts() {
            let plain = letters(keymap, group, 0..2);
            let found = if altgr || plain.is_empty() {
                letters(keymap, group, 0..8)
            } else {
                plain
            };
            for (base, variant) in found {
                self.add(base, variant);
            }
        }
    }
}

fn letters(keymap: &xkb::Keymap, group: u32, levels: std::ops::Range<u32>) -> Vec<(char, char)> {
    let mut out = vec![];
    for code in keymap.min_keycode().raw()..=keymap.max_keycode().raw() {
        let key = xkb::Keycode::new(code);
        // Keys with fewer groups than the keymap would wrap around; they are not letters.
        if group >= keymap.num_layouts_for_key(key) {
            continue;
        }
        for level in levels.clone() {
            if level >= keymap.num_levels_for_key(key, group) {
                break;
            }
            for sym in keymap.key_get_syms_by_level(key, group, level) {
                let Some(c) = char::from_u32(xkb::keysym_to_utf32(*sym)) else {
                    continue;
                };
                if let Some(pair) = single(c.to_lowercase()).and_then(|c| Some((base_of(c)?, c))) {
                    out.push(pair);
                }
            }
        }
    }
    // Keymap order follows the physical keyboard; code point order is more predictable,
    // with common caron letters first.
    out.sort_by_key(|&(b, v)| (b, !matches!(v, 'č' | 'š' | 'ž'), v));
    out.dedup();
    out
}

/// The plain letter `c` is a variant of, if it is one.
fn base_of(c: char) -> Option<char> {
    let base = match c {
        'đ' | 'ð' => 'd',
        'ł' => 'l',
        'ø' | 'œ' => 'o',
        'æ' => 'a',
        'ħ' => 'h',
        'ı' => 'i',
        'ŧ' | 'þ' => 't',
        'ß' => 's',
        'ŋ' => 'n',
        _ => c.nfd().next()?,
    };
    (base != c && base.is_alphabetic()).then_some(base)
}

fn single(mut it: impl Iterator<Item = char>) -> Option<char> {
    let c = it.next()?;
    it.next().is_none().then_some(c)
}

/// Compile a layout given as `si` or `us(intl)`.
pub fn layout_keymap(spec: &str) -> Option<xkb::Keymap> {
    let (layout, variant) = match spec.split_once('(') {
        Some((l, v)) => (l, v.trim_end_matches(')')),
        None => (spec, ""),
    };
    let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    xkb::Keymap::new_from_names(&ctx, "", "", layout, variant, None, xkb::KEYMAP_COMPILE_NO_FLAGS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn of(spec: &str, altgr: bool) -> Variants {
        let mut v = Variants::default();
        v.add_keymap(&layout_keymap(spec).unwrap(), altgr);
        v
    }

    #[test]
    fn slovenian() {
        let v = of("si", false);
        let all: Vec<_> = v.iter().map(|(b, l)| (*b, l.iter().collect::<String>())).collect();
        let want = [('c', "čć"), ('d', "đ"), ('s', "š"), ('z', "ž")];
        assert_eq!(all, want.map(|(b, l)| (b, l.to_string())));
        assert_eq!(v.get('C'), ['Č', 'Ć']);
        assert!(v.get('x').is_empty());
    }

    #[test]
    fn altgr_is_opt_in_when_the_layout_has_plain_accents() {
        assert!(of("si", false).get('l').is_empty());
        assert!(of("si", true).get('l').contains(&'ł'));
    }

    #[test]
    fn altgr_only_layouts_work() {
        assert!(of("pl", false).get('l').contains(&'ł'));
    }

    #[test]
    fn us_has_none_and_groups_combine() {
        assert!(of("us", false).is_empty());
        let ctx = xkb::Context::new(0);
        let k = xkb::Keymap::new_from_names(&ctx, "", "", "us,si,de", "", None, 0).unwrap();
        let mut v = Variants::default();
        v.add_keymap(&k, false);
        assert_eq!(v.get('s'), ['š', 'ß']);
        assert_eq!(v.get('S'), ['Š', 'ẞ']);
        assert_eq!(v.get('u'), ['ü']);
    }
}
