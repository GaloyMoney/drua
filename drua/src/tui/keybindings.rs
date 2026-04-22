use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde::de::{self, Deserializer, SeqAccess, Visitor};
use serde::Deserialize;
use std::fmt;
use std::path::Path;

/// A single key combination: a key code plus zero or more modifiers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyCombo {
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
}

impl KeyCombo {
    fn new(code: KeyCode, modifiers: KeyModifiers) -> Self {
        Self { code, modifiers }
    }

    fn matches(&self, key: &KeyEvent) -> bool {
        if key.code != self.code {
            return false;
        }
        // BackTab is inherently Shift+Tab — crossterm always sets the SHIFT
        // modifier for it, so ignore SHIFT when matching BackTab.
        if self.code == KeyCode::BackTab {
            return key.modifiers & !KeyModifiers::SHIFT == self.modifiers;
        }
        key.modifiers == self.modifiers
    }

    /// Parse a string like `"Ctrl+C"`, `"Enter"`, `"j"` into a KeyCombo.
    fn parse(s: &str) -> Result<Self, String> {
        let parts: Vec<&str> = s.split('+').collect();
        if parts.is_empty() {
            return Err("empty key string".to_string());
        }

        let key_str = parts.last().unwrap().trim();
        let mut modifiers = KeyModifiers::empty();

        for &part in &parts[..parts.len() - 1] {
            match part.trim().to_lowercase().as_str() {
                "ctrl" | "control" => modifiers |= KeyModifiers::CONTROL,
                "shift" => modifiers |= KeyModifiers::SHIFT,
                "alt" => modifiers |= KeyModifiers::ALT,
                other => return Err(format!("unknown modifier: {other}")),
            }
        }

        let code = parse_key_code(key_str)?;
        Ok(Self { code, modifiers })
    }
}

fn parse_key_code(s: &str) -> Result<KeyCode, String> {
    match s.to_lowercase().as_str() {
        "enter" | "return" => Ok(KeyCode::Enter),
        "esc" | "escape" => Ok(KeyCode::Esc),
        "tab" => Ok(KeyCode::Tab),
        "backtab" => Ok(KeyCode::BackTab),
        "backspace" => Ok(KeyCode::Backspace),
        "delete" | "del" => Ok(KeyCode::Delete),
        "up" => Ok(KeyCode::Up),
        "down" => Ok(KeyCode::Down),
        "left" => Ok(KeyCode::Left),
        "right" => Ok(KeyCode::Right),
        "home" => Ok(KeyCode::Home),
        "end" => Ok(KeyCode::End),
        "pageup" => Ok(KeyCode::PageUp),
        "pagedown" => Ok(KeyCode::PageDown),
        "space" => Ok(KeyCode::Char(' ')),
        s if s.len() == 1 => Ok(KeyCode::Char(s.chars().next().unwrap())),
        other => Err(format!("unknown key: {other}")),
    }
}

/// One or more key combos that all trigger the same action.
#[derive(Clone, Debug)]
pub struct Binding(Vec<KeyCombo>);

impl Binding {
    fn single(code: KeyCode, modifiers: KeyModifiers) -> Self {
        Self(vec![KeyCombo::new(code, modifiers)])
    }

    fn keys(combos: Vec<KeyCombo>) -> Self {
        Self(combos)
    }

    pub fn matches(&self, key: &KeyEvent) -> bool {
        self.0.iter().any(|combo| combo.matches(key))
    }

    /// Format all key combos joined with `/`, e.g. "j/Down".
    pub fn hint_all(&self) -> String {
        self.0
            .iter()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .join("/")
    }
}

impl fmt::Display for KeyCombo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.code == KeyCode::BackTab {
            return write!(f, "S-Tab");
        }
        if self.modifiers.contains(KeyModifiers::CONTROL) {
            write!(f, "^")?;
        }
        if self.modifiers.contains(KeyModifiers::ALT) {
            write!(f, "Alt+")?;
        }
        if self.modifiers.contains(KeyModifiers::SHIFT) {
            write!(f, "S-")?;
        }
        match self.code {
            KeyCode::Char(' ') => write!(f, "Space"),
            KeyCode::Char(c) if self.modifiers.contains(KeyModifiers::CONTROL) => {
                write!(f, "{}", c.to_ascii_uppercase())
            }
            KeyCode::Char(c) => write!(f, "{c}"),
            KeyCode::Enter => write!(f, "Enter"),
            KeyCode::Esc => write!(f, "Esc"),
            KeyCode::Tab => write!(f, "Tab"),
            KeyCode::Backspace => write!(f, "\u{232b}"),
            KeyCode::Delete => write!(f, "Del"),
            KeyCode::Up => write!(f, "Up"),
            KeyCode::Down => write!(f, "Down"),
            KeyCode::Left => write!(f, "Left"),
            KeyCode::Right => write!(f, "Right"),
            KeyCode::PageUp => write!(f, "PgUp"),
            KeyCode::PageDown => write!(f, "PgDn"),
            KeyCode::Home => write!(f, "Home"),
            KeyCode::End => write!(f, "End"),
            _ => write!(f, "?"),
        }
    }
}

impl fmt::Display for Binding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(combo) = self.0.first() {
            write!(f, "{combo}")
        } else {
            Ok(())
        }
    }
}

impl<'de> Deserialize<'de> for Binding {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct BindingVisitor;

        impl<'de> Visitor<'de> for BindingVisitor {
            type Value = Binding;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a key string or array of key strings")
            }

            fn visit_str<E: de::Error>(self, v: &str) -> Result<Binding, E> {
                let combo = KeyCombo::parse(v).map_err(de::Error::custom)?;
                Ok(Binding(vec![combo]))
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Binding, A::Error> {
                let mut combos = Vec::new();
                while let Some(s) = seq.next_element::<String>()? {
                    combos.push(KeyCombo::parse(&s).map_err(de::Error::custom)?);
                }
                if combos.is_empty() {
                    return Err(de::Error::custom("empty key binding array"));
                }
                Ok(Binding(combos))
            }
        }

        deserializer.deserialize_any(BindingVisitor)
    }
}

// ── Section structs ──────────────────────────────────────────────

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct GlobalBindings {
    pub quit: Binding,
    pub suspend: Binding,
    pub focus_lead: Binding,
    pub refresh: Binding,
    pub focus_left: Binding,
    pub focus_right: Binding,
    pub toggle_threads: Binding,
    pub show_help: Binding,
}

impl Default for GlobalBindings {
    fn default() -> Self {
        Self {
            quit: Binding::single(KeyCode::Char('c'), KeyModifiers::CONTROL),
            suspend: Binding::single(KeyCode::Char('z'), KeyModifiers::CONTROL),
            focus_lead: Binding::single(KeyCode::Char('o'), KeyModifiers::CONTROL),
            refresh: Binding::single(KeyCode::Char('r'), KeyModifiers::CONTROL),
            focus_left: Binding::single(KeyCode::Char('h'), KeyModifiers::CONTROL),
            focus_right: Binding::single(KeyCode::Char('l'), KeyModifiers::CONTROL),
            toggle_threads: Binding::single(KeyCode::Char('t'), KeyModifiers::CONTROL),
            show_help: Binding::single(KeyCode::Char('?'), KeyModifiers::empty()),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct SidebarBindings {
    pub quit: Binding,
    pub navigate_down: Binding,
    pub navigate_up: Binding,
    pub select: Binding,
    pub new_workspace: Binding,
    pub toggle_focus: Binding,
}

impl Default for SidebarBindings {
    fn default() -> Self {
        Self {
            quit: Binding::single(KeyCode::Char('q'), KeyModifiers::empty()),
            navigate_down: Binding::keys(vec![
                KeyCombo::new(KeyCode::Char('j'), KeyModifiers::empty()),
                KeyCombo::new(KeyCode::Down, KeyModifiers::empty()),
            ]),
            navigate_up: Binding::keys(vec![
                KeyCombo::new(KeyCode::Char('k'), KeyModifiers::empty()),
                KeyCombo::new(KeyCode::Up, KeyModifiers::empty()),
            ]),
            select: Binding::single(KeyCode::Enter, KeyModifiers::empty()),
            new_workspace: Binding::single(KeyCode::Char('n'), KeyModifiers::empty()),
            toggle_focus: Binding::single(KeyCode::Tab, KeyModifiers::empty()),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct AgentsBindings {
    pub navigate_down: Binding,
    pub navigate_up: Binding,
    pub toggle_focus: Binding,
    pub back: Binding,
    pub open_chat: Binding,
}

impl Default for AgentsBindings {
    fn default() -> Self {
        Self {
            navigate_down: Binding::keys(vec![
                KeyCombo::new(KeyCode::Char('j'), KeyModifiers::empty()),
                KeyCombo::new(KeyCode::Down, KeyModifiers::empty()),
            ]),
            navigate_up: Binding::keys(vec![
                KeyCombo::new(KeyCode::Char('k'), KeyModifiers::empty()),
                KeyCombo::new(KeyCode::Up, KeyModifiers::empty()),
            ]),
            toggle_focus: Binding::single(KeyCode::Tab, KeyModifiers::empty()),
            back: Binding::single(KeyCode::Esc, KeyModifiers::empty()),
            open_chat: Binding::single(KeyCode::Enter, KeyModifiers::empty()),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct ChatBindings {
    pub back: Binding,
    pub toggle_focus: Binding,
    pub send: Binding,
    pub scroll_up: Binding,
    pub scroll_down: Binding,
}

impl Default for ChatBindings {
    fn default() -> Self {
        Self {
            back: Binding::single(KeyCode::Esc, KeyModifiers::empty()),
            toggle_focus: Binding::single(KeyCode::Tab, KeyModifiers::empty()),
            send: Binding::single(KeyCode::Enter, KeyModifiers::empty()),
            scroll_up: Binding::single(KeyCode::Up, KeyModifiers::empty()),
            scroll_down: Binding::single(KeyCode::Down, KeyModifiers::empty()),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct ThreadsBindings {
    pub navigate_left: Binding,
    pub navigate_right: Binding,
    pub navigate_up: Binding,
    pub navigate_down: Binding,
    pub jump_start: Binding,
    pub jump_end: Binding,
    pub tab_next: Binding,
    pub export: Binding,
    pub back: Binding,
}

impl Default for ThreadsBindings {
    fn default() -> Self {
        Self {
            navigate_left: Binding::keys(vec![
                KeyCombo::new(KeyCode::Left, KeyModifiers::empty()),
                KeyCombo::new(KeyCode::Char('h'), KeyModifiers::empty()),
            ]),
            navigate_right: Binding::keys(vec![
                KeyCombo::new(KeyCode::Right, KeyModifiers::empty()),
                KeyCombo::new(KeyCode::Char('l'), KeyModifiers::empty()),
            ]),
            navigate_up: Binding::keys(vec![
                KeyCombo::new(KeyCode::Up, KeyModifiers::empty()),
                KeyCombo::new(KeyCode::Char('k'), KeyModifiers::empty()),
            ]),
            navigate_down: Binding::keys(vec![
                KeyCombo::new(KeyCode::Down, KeyModifiers::empty()),
                KeyCombo::new(KeyCode::Char('j'), KeyModifiers::empty()),
            ]),
            jump_start: Binding::single(KeyCode::Char('g'), KeyModifiers::empty()),
            jump_end: Binding::single(KeyCode::Char('G'), KeyModifiers::SHIFT),
            tab_next: Binding::single(KeyCode::Tab, KeyModifiers::empty()),
            export: Binding::single(KeyCode::Char('e'), KeyModifiers::empty()),
            back: Binding::single(KeyCode::Esc, KeyModifiers::empty()),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct CreateWorkspaceBindings {
    pub cancel: Binding,
    pub switch_field: Binding,
    pub confirm: Binding,
}

impl Default for CreateWorkspaceBindings {
    fn default() -> Self {
        Self {
            cancel: Binding::single(KeyCode::Esc, KeyModifiers::empty()),
            switch_field: Binding::keys(vec![
                KeyCombo::new(KeyCode::Tab, KeyModifiers::empty()),
                KeyCombo::new(KeyCode::BackTab, KeyModifiers::empty()),
            ]),
            confirm: Binding::single(KeyCode::Enter, KeyModifiers::empty()),
        }
    }
}

// ── Top-level ────────────────────────────────────────────────────

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Keybindings {
    pub global: GlobalBindings,
    pub sidebar: SidebarBindings,
    pub agents: AgentsBindings,
    pub chat: ChatBindings,
    pub threads: ThreadsBindings,
    pub create_workspace: CreateWorkspaceBindings,
}

impl Keybindings {
    /// Load keybindings from a TOML file. Falls back to defaults for any
    /// missing sections or individual bindings.
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(content) => match toml::from_str(&content) {
                Ok(kb) => kb,
                Err(e) => {
                    eprintln!("warning: failed to parse {}: {e}", path.display());
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn parse_simple_char() {
        let combo = KeyCombo::parse("j").unwrap();
        assert_eq!(combo.code, KeyCode::Char('j'));
        assert_eq!(combo.modifiers, KeyModifiers::empty());
    }

    #[test]
    fn parse_ctrl_char() {
        let combo = KeyCombo::parse("Ctrl+c").unwrap();
        assert_eq!(combo.code, KeyCode::Char('c'));
        assert_eq!(combo.modifiers, KeyModifiers::CONTROL);
    }

    #[test]
    fn parse_special_key() {
        let combo = KeyCombo::parse("Enter").unwrap();
        assert_eq!(combo.code, KeyCode::Enter);
        assert_eq!(combo.modifiers, KeyModifiers::empty());
    }

    #[test]
    fn parse_unknown_key_fails() {
        assert!(KeyCombo::parse("FooBar").is_err());
    }

    #[test]
    fn binding_matches_single() {
        let binding = Binding::single(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(binding.matches(&press(KeyCode::Char('c'), KeyModifiers::CONTROL)));
        assert!(!binding.matches(&press(KeyCode::Char('c'), KeyModifiers::empty())));
    }

    #[test]
    fn binding_matches_multi() {
        let binding = Binding::keys(vec![
            KeyCombo::new(KeyCode::Char('j'), KeyModifiers::empty()),
            KeyCombo::new(KeyCode::Down, KeyModifiers::empty()),
        ]);
        assert!(binding.matches(&press(KeyCode::Char('j'), KeyModifiers::empty())));
        assert!(binding.matches(&press(KeyCode::Down, KeyModifiers::empty())));
        assert!(!binding.matches(&press(KeyCode::Char('k'), KeyModifiers::empty())));
    }

    #[test]
    fn deserialize_single_string() {
        let toml_str = r#"quit = "Ctrl+c""#;
        #[derive(Deserialize)]
        struct Test {
            quit: Binding,
        }
        let t: Test = toml::from_str(toml_str).unwrap();
        assert!(t
            .quit
            .matches(&press(KeyCode::Char('c'), KeyModifiers::CONTROL)));
    }

    #[test]
    fn deserialize_array() {
        let toml_str = r#"nav = ["j", "Down"]"#;
        #[derive(Deserialize)]
        struct Test {
            nav: Binding,
        }
        let t: Test = toml::from_str(toml_str).unwrap();
        assert!(t
            .nav
            .matches(&press(KeyCode::Char('j'), KeyModifiers::empty())));
        assert!(t.nav.matches(&press(KeyCode::Down, KeyModifiers::empty())));
    }

    #[test]
    fn deserialize_partial_config() {
        let toml_str = r#"
[global]
quit = "q"
"#;
        let kb: Keybindings = toml::from_str(toml_str).unwrap();
        // quit should be overridden
        assert!(kb
            .global
            .quit
            .matches(&press(KeyCode::Char('q'), KeyModifiers::empty())));
        // other bindings should be defaults
        assert!(kb
            .global
            .refresh
            .matches(&press(KeyCode::Char('r'), KeyModifiers::CONTROL)));
        // other sections should be defaults
        assert!(kb
            .sidebar
            .navigate_down
            .matches(&press(KeyCode::Char('j'), KeyModifiers::empty())));
    }

    #[test]
    fn display_ctrl_char() {
        let combo = KeyCombo::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(combo.to_string(), "^C");
    }

    #[test]
    fn display_plain_char() {
        let combo = KeyCombo::new(KeyCode::Char('j'), KeyModifiers::empty());
        assert_eq!(combo.to_string(), "j");
    }

    #[test]
    fn hint_all_joins_combos() {
        let binding = Binding::keys(vec![
            KeyCombo::new(KeyCode::Char('j'), KeyModifiers::empty()),
            KeyCombo::new(KeyCode::Down, KeyModifiers::empty()),
        ]);
        assert_eq!(binding.hint_all(), "j/Down");
    }

    #[test]
    fn default_keybindings_match_original() {
        let kb = Keybindings::default();
        // Global
        assert!(kb
            .global
            .quit
            .matches(&press(KeyCode::Char('c'), KeyModifiers::CONTROL)));
        assert!(kb
            .global
            .suspend
            .matches(&press(KeyCode::Char('z'), KeyModifiers::CONTROL)));
        // Sidebar — multiple bindings
        assert!(kb
            .sidebar
            .navigate_down
            .matches(&press(KeyCode::Char('j'), KeyModifiers::empty())));
        assert!(kb
            .sidebar
            .navigate_down
            .matches(&press(KeyCode::Down, KeyModifiers::empty())));
        // Threads — jump_end uses Shift+G
        assert!(kb
            .threads
            .jump_end
            .matches(&press(KeyCode::Char('G'), KeyModifiers::SHIFT)));
    }
}
