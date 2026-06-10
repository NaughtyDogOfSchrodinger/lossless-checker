//! Tiny localization helper for log/report output.
//!
//! Chinese is the default; pass `--lang en` for English. Machine-readable JSON is never
//! localized. Each message keeps both variants side by side at the call site via `pick`.

use clap::ValueEnum;

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Lang {
    /// 中文（默认）
    Zh,
    /// English
    En,
}

impl Lang {
    /// Pick the variant for this language. Chinese is always the first argument. The two
    /// variants share a lifetime, so it works with both string literals and temporaries
    /// (e.g. `lang.pick(&format!(...), &format!(...))`) used within the same expression.
    pub fn pick<'a>(self, zh: &'a str, en: &'a str) -> &'a str {
        match self {
            Lang::Zh => zh,
            Lang::En => en,
        }
    }
}
