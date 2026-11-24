//! The press-and-hold state machine. Knows nothing about Wayland.

use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Meaning {
    Modifier,
    Escape,
    Left,
    Right,
    Accept,
    Other,
}

#[derive(Debug)]
pub struct Press {
    pub code: u32,
    /// Letter typed, if any and no shortcut modifier is held.
    pub letter: Option<char>,
    /// 1..=9 for number keys, whatever the shift state.
    pub digit: Option<usize>,
    pub meaning: Meaning,
    /// Variants of `letter`; empty disables holding.
    pub variants: Vec<char>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Key(u32, bool),
    Show(Vec<char>, Option<usize>),
    Hide,
    Replace { old: char, new: char },
}

#[derive(Debug)]
struct Hold {
    code: u32,
    letter: char,
    variants: Vec<char>,
}

#[derive(Debug)]
struct Menu {
    letter: char,
    variants: Vec<char>,
    selected: Option<usize>,
}

pub struct Engine {
    delay: Duration,
    hold: Option<(Hold, Instant)>,
    menu: Option<Menu>,
    /// Keys the application currently sees as pressed.
    down: Vec<u32>,
}

impl Engine {
    pub fn new(delay: Duration) -> Self {
        Self { delay, hold: None, menu: None, down: vec![] }
    }

    pub fn deadline(&self) -> Option<Instant> {
        self.hold.as_ref().map(|h| h.1)
    }

    pub fn press(&mut self, p: Press, now: Instant) -> Vec<Action> {
        let mut out = vec![];
        if p.meaning == Meaning::Modifier {
            self.down.push(p.code);
            return vec![Action::Key(p.code, true)];
        }
        self.hold = None;
        if let Some(mut menu) = self.menu.take() {
            let n = menu.variants.len();
            let pick = match (p.digit, p.meaning) {
                (Some(d), _) if d <= n => Some(d - 1),
                (_, Meaning::Accept) => menu.selected,
                _ => None,
            };
            if let Some(i) = pick {
                let new = menu.variants[i];
                return vec![Action::Hide, Action::Replace { old: menu.letter, new }];
            }
            match p.meaning {
                Meaning::Escape => return vec![Action::Hide],
                Meaning::Left | Meaning::Right => {
                    menu.selected = Some(match (menu.selected, p.meaning) {
                        (None, Meaning::Left) => n - 1,
                        (None, _) => 0,
                        (Some(i), Meaning::Left) => (i + n - 1) % n,
                        (Some(i), _) => (i + 1) % n,
                    });
                    out.push(Action::Show(menu.variants.clone(), menu.selected));
                    self.menu = Some(menu);
                    return out;
                }
                _ => out.push(Action::Hide),
            }
        }
        out.push(Action::Key(p.code, true));
        match p.letter {
            Some(letter) if !p.variants.is_empty() => {
                // Release at once, so that the application does not start key repeat.
                out.push(Action::Key(p.code, false));
                let hold = Hold { code: p.code, letter, variants: p.variants };
                self.hold = Some((hold, now + self.delay));
            }
            _ => self.down.push(p.code),
        }
        out
    }

    pub fn release(&mut self, code: u32) -> Vec<Action> {
        if self.hold.as_ref().is_some_and(|(h, _)| h.code == code) {
            self.hold = None;
        }
        match self.down.iter().position(|c| *c == code) {
            Some(i) => {
                self.down.remove(i);
                vec![Action::Key(code, false)]
            }
            None => vec![],
        }
    }

    pub fn tick(&mut self, now: Instant) -> Vec<Action> {
        match self.hold.take() {
            Some((h, at)) if now >= at => {
                let show = Action::Show(h.variants.clone(), None);
                self.menu = Some(Menu { letter: h.letter, variants: h.variants, selected: None });
                vec![show]
            }
            other => {
                self.hold = other;
                vec![]
            }
        }
    }

    /// The text field went away: let go of everything.
    pub fn reset(&mut self) -> Vec<Action> {
        self.hold = None;
        let mut out: Vec<_> = self.down.drain(..).map(|c| Action::Key(c, false)).collect();
        if self.menu.take().is_some() {
            out.push(Action::Hide);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use Action::*;

    const C: u32 = 46;

    fn press(e: &mut Engine, code: u32, t: Instant) -> Vec<Action> {
        let (letter, digit, meaning, variants) = match code {
            C => (Some('c'), None, Meaning::Other, vec!['č', 'ć']),
            2..=10 => (None, Some(code as usize - 1), Meaning::Other, vec![]),
            1 => (None, None, Meaning::Escape, vec![]),
            106 => (None, None, Meaning::Right, vec![]),
            28 => (None, None, Meaning::Accept, vec![]),
            42 => (None, None, Meaning::Modifier, vec![]),
            _ => (Some('x'), None, Meaning::Other, vec![]),
        };
        e.press(Press { code, letter, digit, meaning, variants }, t)
    }

    fn open() -> (Engine, Instant) {
        let t = Instant::now();
        let mut e = Engine::new(Duration::from_millis(400));
        assert_eq!(press(&mut e, C, t), [Key(C, true), Key(C, false)]);
        assert!(e.tick(t + Duration::from_millis(399)).is_empty());
        let t = t + Duration::from_millis(400);
        assert_eq!(e.tick(t), [Show(vec!['č', 'ć'], None)]);
        assert!(e.release(C).is_empty());
        (e, t)
    }

    #[test]
    fn number_picks_and_is_swallowed() {
        let (mut e, t) = open();
        assert_eq!(press(&mut e, 3, t), [Hide, Replace { old: 'c', new: 'ć' }]);
        assert!(e.release(3).is_empty());
        assert_eq!(press(&mut e, 3, t), [Key(3, true)]);
    }

    #[test]
    fn number_out_of_range_is_typed() {
        let (mut e, t) = open();
        assert_eq!(press(&mut e, 4, t), [Hide, Key(4, true)]);
        assert_eq!(e.release(4), [Key(4, false)]);
    }

    #[test]
    fn arrows_and_accept() {
        let (mut e, t) = open();
        assert_eq!(press(&mut e, 106, t), [Show(vec!['č', 'ć'], Some(0))]);
        assert_eq!(press(&mut e, 28, t), [Hide, Replace { old: 'c', new: 'č' }]);
    }

    #[test]
    fn accept_without_selection_is_typed() {
        let (mut e, t) = open();
        assert_eq!(press(&mut e, 28, t), [Hide, Key(28, true)]);
    }

    #[test]
    fn escape_closes_and_modifiers_do_not() {
        let (mut e, t) = open();
        assert_eq!(press(&mut e, 42, t), [Key(42, true)]);
        assert_eq!(press(&mut e, 1, t), [Hide]);
    }

    #[test]
    fn quick_tap_or_rollover_never_opens() {
        let t = Instant::now();
        let mut e = Engine::new(Duration::from_millis(400));
        press(&mut e, C, t);
        e.release(C);
        assert!(e.deadline().is_none());
        press(&mut e, C, t);
        press(&mut e, 45, t);
        assert!(e.deadline().is_none());
    }

    #[test]
    fn reset_releases_held_keys() {
        let (mut e, t) = open();
        press(&mut e, 42, t);
        assert_eq!(e.reset(), [Key(42, false), Hide]);
    }
}
