#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShoppingMode {
    Manual,
    Auto,
}

impl ShoppingMode {
    pub fn parse(s: &str) -> Option<Self> {
        match normalized(s).as_str() {
            "manual" | "man" => Some(Self::Manual),
            "auto" | "automatic" => Some(Self::Auto),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Manual => "Manual",
            Self::Auto => "Auto",
        }
    }

    pub fn instruction(self) -> &'static str {
        match self {
            Self::Manual => {
                "Manual mode: search for products and return choices for the user to pick. Do not add products directly from a free-text shopping request unless the user has already selected a specific product."
            }
            Self::Auto => {
                "Auto mode: choose sensible products and add them directly when confidence is high. Do not show product candidate buttons. Ask one concise text clarification before adding only when the request is ambiguous, preference-sensitive, high cost, dietary, quantity-sensitive, or a recipe/pantry assumption would change the basket."
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShoppingStyle {
    BestValue,
    KnownBrands,
    Cheapest,
}

impl ShoppingStyle {
    pub fn parse(s: &str) -> Option<Self> {
        match normalized(s).as_str() {
            "best_value" | "best value" | "value" => Some(Self::BestValue),
            "known_brands" | "known brands" | "brands" | "brand" => Some(Self::KnownBrands),
            "cheapest" | "cheap" | "lowest" => Some(Self::Cheapest),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::BestValue => "Best value",
            Self::KnownBrands => "Known brands",
            Self::Cheapest => "Cheapest",
        }
    }

    pub fn instruction(self) -> &'static str {
        match self {
            Self::BestValue => {
                "Shopping style: prefer good value, balancing recognizable products, useful pack sizes, and price."
            }
            Self::KnownBrands => {
                "Shopping style: prefer familiar or well-known brands unless the user asks for cheaper alternatives."
            }
            Self::Cheapest => {
                "Shopping style: prefer the cheapest suitable option, while avoiding obviously poor matches."
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PantryPolicy {
    AskEachTime,
    AssumeNo,
    AssumeYes,
}

impl PantryPolicy {
    pub fn parse(s: &str) -> Option<Self> {
        match normalized(s).as_str() {
            "ask" | "ask_each_time" | "ask each time" => Some(Self::AskEachTime),
            "assume_no" | "assume no" | "no" | "include basics" | "include" => Some(Self::AssumeNo),
            "assume_yes" | "assume yes" | "yes" | "skip basics" | "skip" => Some(Self::AssumeYes),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::AskEachTime => "Ask each time",
            Self::AssumeNo => "Assume no",
            Self::AssumeYes => "Assume yes",
        }
    }

    pub fn instruction(self) -> &'static str {
        match self {
            Self::AskEachTime => {
                "Recipe pantry basics: ask before assuming the user has common basics like flour, sugar, salt, oil, butter, milk, eggs, baking powder, or spices."
            }
            Self::AssumeNo => {
                "Recipe pantry basics: assume the user does not have basics unless they say otherwise; include the full ingredient basket."
            }
            Self::AssumeYes => {
                "Recipe pantry basics: assume the user has common basics unless they ask for everything; still ask before skipping a core ingredient if it would make the result unusable."
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubstitutionPolicy {
    AskFirst,
    UseCloseSubstitutes,
}

impl SubstitutionPolicy {
    pub fn parse(s: &str) -> Option<Self> {
        match normalized(s).as_str() {
            "ask" | "ask_first" | "ask first" => Some(Self::AskFirst),
            "close" | "use_close" | "use close" | "substitute" | "substitutes" => {
                Some(Self::UseCloseSubstitutes)
            }
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::AskFirst => "Ask first",
            Self::UseCloseSubstitutes => "Use close substitutes",
        }
    }

    pub fn instruction(self) -> &'static str {
        match self {
            Self::AskFirst => {
                "Substitutions: ask before using substitutions when the exact requested item is unavailable."
            }
            Self::UseCloseSubstitutes => {
                "Substitutions: use close substitutes when the exact requested item is unavailable, but ask when the substitute changes diet, flavor, size, brand class, or price materially."
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct UserPreferences {
    #[serde(default)]
    pub mode: Option<ShoppingMode>,
    #[serde(default)]
    pub shopping_style: Option<ShoppingStyle>,
    #[serde(default)]
    pub pantry_basics: Option<PantryPolicy>,
    #[serde(default)]
    pub substitutions: Option<SubstitutionPolicy>,
}

impl UserPreferences {
    pub fn is_complete(&self) -> bool {
        self.mode.is_some()
            && self.shopping_style.is_some()
            && self.pantry_basics.is_some()
            && self.substitutions.is_some()
    }

    pub fn next_question(&self) -> Option<PreferenceQuestion> {
        if self.mode.is_none() {
            return Some(PreferenceQuestion {
                field: PreferenceField::Mode,
                prompt: "Quick setup: how should I shop for you?".to_string(),
                options: vec![
                    PreferenceOption::new("pref:mode:manual", "Manual"),
                    PreferenceOption::new("pref:mode:auto", "Auto"),
                ],
            });
        }
        if self.shopping_style.is_none() {
            return Some(PreferenceQuestion {
                field: PreferenceField::ShoppingStyle,
                prompt: "When I choose products, what should I optimize for?".to_string(),
                options: vec![
                    PreferenceOption::new("pref:style:value", "Best value"),
                    PreferenceOption::new("pref:style:brands", "Known brands"),
                    PreferenceOption::new("pref:style:cheap", "Cheapest"),
                ],
            });
        }
        if self.pantry_basics.is_none() {
            return Some(PreferenceQuestion {
                field: PreferenceField::PantryBasics,
                prompt: "For recipes, should I assume you already have pantry basics?".to_string(),
                options: vec![
                    PreferenceOption::new("pref:pantry:ask", "Ask each time"),
                    PreferenceOption::new("pref:pantry:no", "Assume no"),
                    PreferenceOption::new("pref:pantry:yes", "Assume yes"),
                ],
            });
        }
        if self.substitutions.is_none() {
            return Some(PreferenceQuestion {
                field: PreferenceField::Substitutions,
                prompt: "If something is unavailable, what should I do?".to_string(),
                options: vec![
                    PreferenceOption::new("pref:subs:ask", "Ask first"),
                    PreferenceOption::new("pref:subs:close", "Use close substitutes"),
                ],
            });
        }
        None
    }

    pub fn apply_action(&mut self, action: PreferenceAction) {
        match action {
            PreferenceAction::Mode(mode) => self.mode = Some(mode),
            PreferenceAction::ShoppingStyle(style) => self.shopping_style = Some(style),
            PreferenceAction::PantryBasics(policy) => self.pantry_basics = Some(policy),
            PreferenceAction::Substitutions(policy) => self.substitutions = Some(policy),
        }
    }

    pub fn apply_text_for_next_question(&mut self, text: &str) -> bool {
        let Some(question) = self.next_question() else {
            return false;
        };
        let action = match question.field {
            PreferenceField::Mode => ShoppingMode::parse(text).map(PreferenceAction::Mode),
            PreferenceField::ShoppingStyle => {
                ShoppingStyle::parse(text).map(PreferenceAction::ShoppingStyle)
            }
            PreferenceField::PantryBasics => {
                PantryPolicy::parse(text).map(PreferenceAction::PantryBasics)
            }
            PreferenceField::Substitutions => {
                SubstitutionPolicy::parse(text).map(PreferenceAction::Substitutions)
            }
        };
        if let Some(action) = action {
            self.apply_action(action);
            true
        } else {
            false
        }
    }

    pub fn summary(&self) -> String {
        format!(
            "Mode: {}\nShopping style: {}\nPantry basics: {}\nSubstitutions: {}",
            self.mode.map(ShoppingMode::label).unwrap_or("not set"),
            self.shopping_style
                .map(ShoppingStyle::label)
                .unwrap_or("not set"),
            self.pantry_basics
                .map(PantryPolicy::label)
                .unwrap_or("not set"),
            self.substitutions
                .map(SubstitutionPolicy::label)
                .unwrap_or("not set")
        )
    }

    pub fn agent_instructions(&self) -> String {
        let mode = self.mode.unwrap_or(ShoppingMode::Manual);
        let style = self.shopping_style.unwrap_or(ShoppingStyle::BestValue);
        let pantry = self.pantry_basics.unwrap_or(PantryPolicy::AskEachTime);
        let substitutions = self.substitutions.unwrap_or(SubstitutionPolicy::AskFirst);
        format!(
            "Current user preferences:\n- {}\n- {}\n- {}\n- {}\n\nBefore handling a shopping request, check these preferences and the user's wording. Ask a short clarification question when these preferences say to ask, or when adding directly would likely be wrong.",
            mode.instruction(),
            style.instruction(),
            pantry.instruction(),
            substitutions.instruction()
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreferenceField {
    Mode,
    ShoppingStyle,
    PantryBasics,
    Substitutions,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreferenceQuestion {
    pub field: PreferenceField,
    pub prompt: String,
    pub options: Vec<PreferenceOption>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreferenceOption {
    pub id: String,
    pub label: String,
}

impl PreferenceOption {
    fn new(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreferenceAction {
    Mode(ShoppingMode),
    ShoppingStyle(ShoppingStyle),
    PantryBasics(PantryPolicy),
    Substitutions(SubstitutionPolicy),
}

pub fn parse_preference_callback(data: &str) -> Option<PreferenceAction> {
    match data {
        "pref:mode:manual" => Some(PreferenceAction::Mode(ShoppingMode::Manual)),
        "pref:mode:auto" => Some(PreferenceAction::Mode(ShoppingMode::Auto)),
        "pref:style:value" => Some(PreferenceAction::ShoppingStyle(ShoppingStyle::BestValue)),
        "pref:style:brands" => Some(PreferenceAction::ShoppingStyle(ShoppingStyle::KnownBrands)),
        "pref:style:cheap" => Some(PreferenceAction::ShoppingStyle(ShoppingStyle::Cheapest)),
        "pref:pantry:ask" => Some(PreferenceAction::PantryBasics(PantryPolicy::AskEachTime)),
        "pref:pantry:no" => Some(PreferenceAction::PantryBasics(PantryPolicy::AssumeNo)),
        "pref:pantry:yes" => Some(PreferenceAction::PantryBasics(PantryPolicy::AssumeYes)),
        "pref:subs:ask" => Some(PreferenceAction::Substitutions(
            SubstitutionPolicy::AskFirst,
        )),
        "pref:subs:close" => Some(PreferenceAction::Substitutions(
            SubstitutionPolicy::UseCloseSubstitutes,
        )),
        _ => None,
    }
}

fn normalized(s: &str) -> String {
    s.trim()
        .to_ascii_lowercase()
        .replace('-', " ")
        .replace('_', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_questions_follow_missing_preference_order() {
        let mut prefs = UserPreferences::default();
        assert_eq!(prefs.next_question().unwrap().field, PreferenceField::Mode);

        prefs.apply_action(PreferenceAction::Mode(ShoppingMode::Auto));
        assert_eq!(
            prefs.next_question().unwrap().field,
            PreferenceField::ShoppingStyle
        );

        prefs.apply_action(PreferenceAction::ShoppingStyle(ShoppingStyle::BestValue));
        assert_eq!(
            prefs.next_question().unwrap().field,
            PreferenceField::PantryBasics
        );

        prefs.apply_action(PreferenceAction::PantryBasics(PantryPolicy::AskEachTime));
        assert_eq!(
            prefs.next_question().unwrap().field,
            PreferenceField::Substitutions
        );

        prefs.apply_action(PreferenceAction::Substitutions(
            SubstitutionPolicy::AskFirst,
        ));
        assert!(prefs.is_complete());
        assert!(prefs.next_question().is_none());
    }

    #[test]
    fn parses_callbacks_and_text_answers() {
        assert_eq!(
            parse_preference_callback("pref:mode:auto"),
            Some(PreferenceAction::Mode(ShoppingMode::Auto))
        );
        let mut prefs = UserPreferences::default();
        assert!(prefs.apply_text_for_next_question("auto"));
        assert_eq!(prefs.mode, Some(ShoppingMode::Auto));
        assert!(prefs.apply_text_for_next_question("known brands"));
        assert_eq!(prefs.shopping_style, Some(ShoppingStyle::KnownBrands));
    }

    #[test]
    fn agent_instructions_include_mode_and_pantry_rules() {
        let prefs = UserPreferences {
            mode: Some(ShoppingMode::Auto),
            shopping_style: Some(ShoppingStyle::Cheapest),
            pantry_basics: Some(PantryPolicy::AssumeNo),
            substitutions: Some(SubstitutionPolicy::UseCloseSubstitutes),
        };
        let instructions = prefs.agent_instructions();
        assert!(instructions.contains("Auto mode"));
        assert!(instructions.contains("assume the user does not have basics"));
        assert!(instructions.contains("cheapest suitable option"));
    }
}
